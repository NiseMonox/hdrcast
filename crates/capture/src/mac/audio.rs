//! The system audio (design.md section 2.4): the audio output of the session's `SCStream`.
//!
//! The stream is asked for 48 kHz stereo and delivers 32-bit float PCM, normally one buffer per channel
//! ("non-interleaved"). Nothing of that is taken for granted: every sample buffer is read according to the
//! `AudioStreamBasicDescription` it carries - interleaved or not, one channel or several, float or integer -
//! and brought to the engine's interleaved 48 kHz stereo by the shared [`StereoConverter`].

use std::ptr::{self, NonNull};
use std::sync::Arc;

use objc2_core_audio_types::{
    kAudioFormatFlagIsBigEndian, kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved,
    kAudioFormatFlagIsSignedInteger, kAudioFormatLinearPCM, AudioBuffer, AudioBufferList,
    AudioStreamBasicDescription,
};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::CGMainDisplayID;
use objc2_core_media::{
    kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMSampleBuffer,
};

use super::output::PixelPlan;
use super::stream::{Session, StreamParams};
use super::{permission, system};
use crate::log;
use crate::platform::{AudioSink, AudioSource};
use crate::repack::{fourcc, interleave_planes};
use crate::resample::{PcmFormat, StereoConverter, TARGET_CHANNELS, TARGET_SAMPLE_RATE};

/// The audio half of a session's shared state; only touched under its mutex.
#[derive(Default)]
pub(super) struct AudioState {
    /// Where the sound goes; `None` while no audio source is open.
    sink: Option<AudioSink>,
    converter: Option<StereoConverter>,
    /// Storage for the `AudioBufferList` of the buffer being read (u64: it holds pointers).
    list: Vec<u64>,
    /// The channels of a non-interleaved buffer, interleaved.
    interleaved: Vec<u8>,
    /// One converted chunk: interleaved 48 kHz stereo.
    stereo: Vec<f32>,
    /// The delivered format has been logged.
    described: bool,
    /// An unusable buffer has been reported.
    complained: bool,
}

impl AudioState {
    pub(super) fn attach(&mut self, sink: AudioSink) {
        *self = Self {
            sink: Some(sink),
            ..Self::default()
        };
    }

    pub(super) fn detach(&mut self) {
        self.sink = None;
    }
}

/// An audio sample buffer, on the audio queue. `state` stays locked for the whole delivery, which is what
/// lets [`Session::detach_audio`] promise that the sink is not called after it returned.
pub(super) fn deliver(state: &mut AudioState, sample: &CMSampleBuffer) {
    let Some(sink) = state.sink.clone() else {
        return;
    };
    match convert(state, sample) {
        Ok(()) => {
            if !state.stereo.is_empty() {
                sink(&state.stereo);
            }
        }
        Err(message) => {
            if !state.complained {
                state.complained = true;
                log::warn(format!(
                    "audio: {message}; such buffers are left out (the stream carries silence instead)"
                ));
            }
        }
    }
}

/// Checks that `description` is PCM this code can read and says how.
fn pcm_format(description: &AudioStreamBasicDescription) -> Result<PcmFormat, String> {
    if description.mFormatID != kAudioFormatLinearPCM {
        return Err(format!(
            "ScreenCaptureKit delivers audio format {} instead of linear PCM",
            fourcc(description.mFormatID)
        ));
    }
    let flags = description.mFormatFlags;
    let bits = description.mBitsPerChannel;
    let float = flags & kAudioFormatFlagIsFloat != 0;
    let supported = flags & kAudioFormatFlagIsBigEndian == 0
        && if float {
            bits == 32
        } else {
            flags & kAudioFormatFlagIsSignedInteger != 0 && (bits == 16 || bits == 32)
        };
    let rate = description.mSampleRate;
    if !supported
        || description.mChannelsPerFrame == 0
        || !(rate.is_finite() && (1000.0..=768_000.0).contains(&rate))
    {
        return Err(format!(
            "ScreenCaptureKit delivers PCM this engine cannot read ({rate} Hz, {} ch, {bits} bit, flags 0x{flags:x})",
            description.mChannelsPerFrame
        ));
    }
    Ok(PcmFormat {
        sample_rate: rate.round() as u32,
        channels: description.mChannelsPerFrame.min(u32::from(u16::MAX)) as u16,
        bits_per_sample: bits as u16,
        float,
    })
}

/// The `AudioBufferList` of a sample buffer, with the memory its buffers point into kept alive.
struct SampleData<'a> {
    list: &'a [u64],
    buffers: usize,
    /// Owns the bytes the list's `mData` pointers refer to.
    _block: CFRetained<CMBlockBuffer>,
}

impl<'a> SampleData<'a> {
    fn of(sample: &CMSampleBuffer, storage: &'a mut Vec<u64>) -> Result<Self, String> {
        let mut needed = 0usize;
        // SAFETY: `sample` is valid for the duration of the callback. This first call only asks for the
        // size of the list: every other out-pointer is null, which the function documents as allowed.
        let status = unsafe {
            sample.audio_buffer_list_with_retained_block_buffer(
                &mut needed,
                ptr::null_mut(),
                0,
                None,
                None,
                0,
                ptr::null_mut(),
            )
        };
        if status != 0 || needed < size_of::<AudioBufferList>() {
            return Err(format!(
                "an audio buffer does not describe its data (status {status}, {needed} bytes)"
            ));
        }
        storage.clear();
        storage.resize(needed.div_ceil(size_of::<u64>()), 0);

        let mut block: *mut CMBlockBuffer = ptr::null_mut();
        // SAFETY: `storage` holds at least `needed` writable bytes, aligned for AudioBufferList (which
        // contains pointers: 8-byte alignment, that of u64). On success the function has filled the list
        // and stored a block buffer in `block` that the caller owns (+1).
        let status = unsafe {
            sample.audio_buffer_list_with_retained_block_buffer(
                ptr::null_mut(),
                storage.as_mut_ptr().cast::<AudioBufferList>(),
                needed,
                None,
                None,
                kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
                &mut block,
            )
        };
        let Some(block) = NonNull::new(block).filter(|_| status == 0) else {
            return Err(format!("cannot read an audio buffer (status {status})"));
        };
        // SAFETY: a +1 reference handed to us by the call above; released when `_block` is dropped, after
        // the last use of the data it owns.
        let block = unsafe { CFRetained::from_raw(block) };

        // SAFETY: the call succeeded, so the storage starts with an initialised AudioBufferList header.
        let buffers =
            unsafe { (*storage.as_ptr().cast::<AudioBufferList>()).mNumberBuffers } as usize;
        let end = std::mem::offset_of!(AudioBufferList, mBuffers)
            .checked_add(buffers.saturating_mul(size_of::<AudioBuffer>()));
        if end.is_none_or(|end| end > needed) {
            return Err(format!(
                "an audio buffer list of {buffers} buffers does not fit its {needed} bytes"
            ));
        }
        Ok(Self {
            list: storage,
            buffers,
            _block: block,
        })
    }

    /// The bytes of buffer `index`.
    fn buffer(&self, index: usize) -> Option<&[u8]> {
        if index >= self.buffers {
            return None;
        }
        // SAFETY: the list holds `buffers` AudioBuffer records starting at the offset of `mBuffers`, all
        // inside the storage (checked in `of`), which is 8-byte aligned like the records.
        let record = unsafe {
            self.list
                .as_ptr()
                .cast::<u8>()
                .add(std::mem::offset_of!(AudioBufferList, mBuffers))
                .cast::<AudioBuffer>()
                .add(index)
                .read()
        };
        if record.mData.is_null() {
            return None;
        }
        // SAFETY: the record describes `mDataByteSize` readable bytes inside the retained block buffer,
        // which lives as long as `self`; the returned slice borrows `self`.
        Some(unsafe {
            std::slice::from_raw_parts(record.mData.cast::<u8>(), record.mDataByteSize as usize)
        })
    }
}

/// Reads `sample` into `state.stereo` as interleaved 48 kHz stereo (left empty when it holds no audio).
fn convert(state: &mut AudioState, sample: &CMSampleBuffer) -> Result<(), String> {
    state.stereo.clear();
    // SAFETY: `sample` is a valid sample buffer for the duration of the callback.
    let frames = unsafe { sample.num_samples() };
    if frames <= 0 {
        return Ok(());
    }
    let frames = frames as usize;

    // SAFETY: as above; the format description is retained by the wrapper.
    let description = unsafe { sample.format_description() }
        .ok_or("an audio buffer arrived without a format description")?;
    // SAFETY: `description` is a valid format description. The function returns null for anything that is
    // not audio, otherwise a pointer into the description, which is copied out while that is still held.
    let description =
        unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&description).as_ref() }
            .copied()
            .ok_or("an audio buffer arrived with a format description that is not audio")?;
    let format = pcm_format(&description)?;
    let planar = description.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
    let channels = usize::from(format.channels);
    let sample_bytes = format.bytes_per_sample();

    if !state.described {
        state.described = true;
        log::info(format!(
            "audio: ScreenCaptureKit delivers {} {} Hz {} ch {} bit, {}, {frames} frames per buffer",
            if format.float { "Float" } else { "Integer" },
            format.sample_rate,
            format.channels,
            format.bits_per_sample,
            if planar {
                "one buffer per channel"
            } else {
                "interleaved"
            },
        ));
    }

    let AudioState {
        converter,
        list,
        interleaved,
        stereo,
        ..
    } = state;
    let data = SampleData::of(sample, list)?;

    // The converter keeps the front pair of anything wider than stereo, so only those are gathered.
    let (input, input_format): (&[u8], PcmFormat) = if planar && channels > 1 {
        let used = channels.min(TARGET_CHANNELS);
        let plane_bytes = frames * sample_bytes;
        let mut planes: [&[u8]; TARGET_CHANNELS] = [&[]; TARGET_CHANNELS];
        for (index, plane) in planes.iter_mut().enumerate().take(used) {
            let bytes = data
                .buffer(index)
                .ok_or("a non-interleaved audio buffer holds fewer buffers than channels")?;
            *plane = &bytes[..bytes.len().min(plane_bytes)];
        }
        interleave_planes(&planes[..used], sample_bytes, interleaved);
        (
            interleaved.as_slice(),
            PcmFormat {
                channels: used as u16,
                ..format
            },
        )
    } else {
        // Interleaved, or a single channel (where the two layouts are the same thing).
        let bytes = data.buffer(0).ok_or("an audio buffer holds no data")?;
        let wanted = frames * sample_bytes * channels;
        (&bytes[..bytes.len().min(wanted)], format)
    };

    if converter.as_ref().map(StereoConverter::format) != Some(input_format) {
        *converter = Some(StereoConverter::new(input_format));
    }
    if let Some(converter) = converter.as_mut() {
        converter.convert(input, stereo);
    }
    Ok(())
}

/// The macOS [`AudioSource`]: holds the session and takes the audio output off it when dropped.
pub(super) struct ScreenAudioSource {
    session: Arc<Session>,
}

impl ScreenAudioSource {
    pub(super) fn session(&self) -> &Arc<Session> {
        &self.session
    }
}

impl AudioSource for ScreenAudioSource {
    fn description(&self) -> String {
        format!(
            "ScreenCaptureKit system audio, Float {TARGET_SAMPLE_RATE} Hz {TARGET_CHANNELS} ch 32 bit requested"
        )
    }
}

impl Drop for ScreenAudioSource {
    fn drop(&mut self) {
        self.session.detach_audio();
    }
}

/// Starts the system audio flowing into `sink`: on the stream `existing` (the one `open_video` started),
/// or on a stream of its own when there is none.
pub(super) fn open(
    existing: Option<Arc<Session>>,
    device: &str,
    exclude_app: &str,
    sink: AudioSink,
) -> Result<ScreenAudioSource, String> {
    if !system::audio_capture_supported() {
        return Err("capturing the system audio needs macOS 13 or later".to_owned());
    }
    if !exclude_app.is_empty() {
        log::info(format!(
            "audio: leaving one program out ({exclude_app}) is not supported on macOS yet; capturing all of the system audio"
        ));
    }
    if device != "default" {
        log::info(
            "audio: macOS captures the system audio as a whole; the audio device setting is ignored",
        );
    }
    let session = match existing {
        Some(session) => session,
        None => {
            permission::ensure_screen_capture_access()?;
            // A stream needs a display even when only its sound is wanted: the smallest picture at the
            // lowest rate, delivered to nobody.
            Session::start(StreamParams {
                display_id: CGMainDisplayID(),
                width: 320,
                height: 180,
                fps: 1,
                cursor: false,
                plan: PixelPlan::Nv12,
                video: false,
                audio: true,
            })?
        }
    };
    session.attach_audio(sink)?;
    Ok(ScreenAudioSource { session })
}
