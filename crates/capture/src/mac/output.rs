//! Where ScreenCaptureKit's callbacks end: the Objective-C object that is the stream's output and delegate,
//! and the state it shares with the engine's side of the backend.
//!
//! Threads: ScreenCaptureKit calls `stream:didOutputSampleBuffer:ofType:` on the serial dispatch queue the
//! output was added with (one for pictures, one for sound) and `stream:didStopWithError:` on a queue of its
//! own. Everything those calls touch lives in [`Shared`], behind mutexes that are only ever held for a copy
//! or a swap - never across a call into the system.
//!
//! A picture travels through three buffers of one frame each: the callback fills `staging` (it is the only
//! one using it), swaps it with `front` under the lock, and `next_frame` swaps `front` with its own spare
//! buffer under the lock and copies out of that afterwards. Neither side ever waits for the other's copy.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::NSObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_foundation::{CFDictionary, CFNumber, CFString, CFType};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_2020,
    kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
    kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_2020,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    kCVPixelFormatType_444YpCbCr10BiPlanarFullRange, kCVReturnSuccess, CVPixelBuffer,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount,
    CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSError, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCFrameStatus, SCStream, SCStreamDelegate, SCStreamErrorCode, SCStreamFrameInfoStatus,
    SCStreamOutput, SCStreamOutputType,
};

use super::audio::{self, AudioState};
use super::stream::error_text;
use crate::args::CaptureFormat;
use crate::color;
use crate::log;
use crate::repack::{copy_plane, fourcc, xf44_to_p010};

/// How the stream is asked for its pictures, and what the callback does with them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PixelPlan {
    /// SDR: `'420v'`, 8-bit limited-range 4:2:0 in two planes - NV12 as it is.
    Nv12,
    /// HDR: `'x420'`, 10-bit limited-range 4:2:0 in two planes of 16-bit little-endian samples with the 10
    /// bits on top - P010 as it is.
    P010,
    /// HDR: `'xf44'`, 10-bit FULL-range 4:4:4 in two planes; converted to P010 on the CPU
    /// ([`xf44_to_p010`]). The HDR format the ScreenCaptureKit header documents; the fallback when `'x420'`
    /// is not delivered.
    P010From444,
}

impl PixelPlan {
    /// The CoreVideo pixel format the stream is configured with.
    pub(super) fn pixel_format(self) -> u32 {
        match self {
            PixelPlan::Nv12 => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            PixelPlan::P010 => kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
            PixelPlan::P010From444 => kCVPixelFormatType_444YpCbCr10BiPlanarFullRange,
        }
    }

    /// What the engine gets (and tells ffmpeg).
    pub(super) fn capture_format(self) -> CaptureFormat {
        match self {
            PixelPlan::Nv12 => CaptureFormat::Nv12,
            PixelPlan::P010 | PixelPlan::P010From444 => CaptureFormat::P010,
        }
    }

    pub(super) fn is_hdr(self) -> bool {
        self != PixelPlan::Nv12
    }

    /// For the log.
    pub(super) fn describe(self) -> String {
        match self {
            PixelPlan::P010From444 => format!(
                "{} converted to p010 on the CPU",
                fourcc(self.pixel_format())
            ),
            _ => format!(
                "{} = {}",
                fourcc(self.pixel_format()),
                self.capture_format().name()
            ),
        }
    }
}

/// The pictures a stream was configured to deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FrameSpec {
    pub plan: PixelPlan,
    /// Even.
    pub width: u32,
    /// Even.
    pub height: u32,
}

impl FrameSpec {
    pub(super) fn frame_bytes(&self) -> usize {
        color::frame_bytes(self.plan.capture_format(), self.width, self.height)
    }
}

/// Why ScreenCaptureKit ended a stream.
#[derive(Debug, Clone)]
pub(super) struct StreamStop {
    /// The user ended it from the system's screen-sharing menu: not something to undo by restarting.
    pub by_user: bool,
    pub message: String,
}

pub(super) struct VideoSlot {
    /// The newest complete picture, packed the way the engine wants it.
    pub front: Vec<u8>,
    /// Counts the pictures parked in `front`; `next_frame` compares it with what it last took.
    pub seq: u64,
    /// What copying (or converting) that picture out of the pixel buffer took.
    pub copy_ms: f64,
    /// A picture arrived that cannot be used (other format or size than configured). Stays set.
    pub fatal: Option<String>,
    /// The first sample of the stream carried a picture of another kind than configured. Not fatal by
    /// itself (only "complete" pictures decide that), but enough for [`Shared::probe_first_frame`] to turn
    /// a format down.
    suspect: Option<String>,
    /// The stream of the current epoch ended; taken by `next_frame`.
    pub stopped: Option<StreamStop>,
    /// The first picture of this epoch has been described in the log.
    described: bool,
}

/// What the first picture is waited for when a stream is opened.
pub(super) enum Probe {
    /// A usable picture arrived.
    Frame,
    /// The stream delivers something else than configured, or ended.
    Refused(String),
    /// Nothing within the time limit.
    Nothing,
}

/// State shared between the callbacks and the engine's thread.
pub(super) struct Shared {
    /// `None`: a stream that is only there for the sound.
    frame: Option<FrameSpec>,
    /// Bumped for every stream started on this state; callbacks of an older stream (one that was replaced
    /// by `recover`) are ignored.
    epoch: AtomicU64,
    video: Mutex<VideoSlot>,
    video_wake: Condvar,
    /// The buffer the picture callback fills. Its own lock is never contended (the queue is serial); it is
    /// a mutex only so that no `unsafe` is needed to mutate through `&self`.
    staging: Mutex<Vec<u8>>,
    pub(super) audio: Mutex<AudioState>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    pub(super) fn new(frame: Option<FrameSpec>) -> Arc<Self> {
        let frame_bytes = frame.map_or(0, |spec| spec.frame_bytes());
        // Not zero-filled, for the reason the frame pool gives: untouched zero pages would be committed in
        // the middle of the first captures.
        let buffer = || vec![0x10u8; frame_bytes];
        Arc::new(Self {
            frame,
            epoch: AtomicU64::new(0),
            video: Mutex::new(VideoSlot {
                front: buffer(),
                seq: 0,
                copy_ms: 0.0,
                fatal: None,
                suspect: None,
                stopped: None,
                described: false,
            }),
            video_wake: Condvar::new(),
            staging: Mutex::new(buffer()),
            audio: Mutex::new(AudioState::default()),
        })
    }

    pub(super) fn frame(&self) -> Option<FrameSpec> {
        self.frame
    }

    /// Starts a new epoch for a stream about to be created and forgets how the previous one ended.
    pub(super) fn begin_epoch(&self) -> u64 {
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let mut slot = lock(&self.video);
        slot.stopped = None;
        slot.suspect = None;
        slot.described = false;
        epoch
    }

    pub(super) fn video(&self) -> MutexGuard<'_, VideoSlot> {
        lock(&self.video)
    }

    /// Waits on the slot (which is released meanwhile) for a picture or a stop, at most `timeout`.
    pub(super) fn wait<'a>(
        &self,
        slot: MutexGuard<'a, VideoSlot>,
        timeout: Duration,
    ) -> MutexGuard<'a, VideoSlot> {
        self.video_wake
            .wait_timeout(slot, timeout)
            .unwrap_or_else(PoisonError::into_inner)
            .0
    }

    /// Waits up to `timeout` for the first picture of a freshly started stream.
    pub(super) fn probe_first_frame(&self, timeout: Duration) -> Probe {
        let deadline = Instant::now() + timeout;
        let mut slot = self.video();
        loop {
            if let Some(message) = &slot.fatal {
                return Probe::Refused(message.clone());
            }
            if let Some(stop) = &slot.stopped {
                return Probe::Refused(format!("the stream ended at once: {}", stop.message));
            }
            if slot.seq > 0 {
                return Probe::Frame;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                // No usable picture. If the stream's opening sample was of another kind, that is all
                // there is to go by (an idle screen sends nothing more for a while), and it says no.
                return match &slot.suspect {
                    Some(message) => Probe::Refused(message.clone()),
                    None => Probe::Nothing,
                };
            }
            slot = self.wait(slot, left);
        }
    }

    fn fail_video(&self, message: String) {
        let mut slot = self.video();
        if slot.fatal.is_none() {
            slot.fatal = Some(message);
        }
        drop(slot);
        self.video_wake.notify_all();
    }

    /// A screen sample buffer, on the picture queue.
    fn on_picture(&self, sample: &CMSampleBuffer) {
        let Some(spec) = self.frame else {
            return;
        };
        // Idle / blank / suspended frames carry no pixels: the engine repeats the last picture itself.
        // "Complete" is a new picture. "Started" marks the first sample of a stream; where it comes with a
        // picture of the configured kind that picture is used (an idle screen sends nothing else for a
        // while). Where it does not, it is passed over: only a complete picture of the wrong kind ends
        // the session, though the mismatch is remembered for the check made while a stream is opened.
        let status = frame_status(sample);
        let complete = status == Some(SCFrameStatus::Complete.0 as i64);
        if !complete && status != Some(SCFrameStatus::Started.0 as i64) {
            return;
        }
        // SAFETY: `sample` is a valid sample buffer for the duration of the callback. The image buffer is
        // retained by the returned wrapper and released when it goes out of scope, before the callback
        // returns.
        let Some(image) = (unsafe { sample.image_buffer() }) else {
            return;
        };

        let started = Instant::now();
        let mut staging = lock(&self.staging);
        if let Err(message) = unpack(&image, spec, &mut staging) {
            drop(staging);
            if complete {
                // Not what was configured: never copy it, end the session with the reason.
                self.fail_video(message);
            } else {
                self.video().suspect.get_or_insert(message);
                self.video_wake.notify_all();
            }
            return;
        }
        let copy_ms = started.elapsed().as_secs_f64() * 1000.0;

        let mut slot = self.video();
        if slot.fatal.is_some() {
            return;
        }
        std::mem::swap(&mut slot.front, &mut *staging);
        slot.seq += 1;
        slot.copy_ms = copy_ms;
        let first = !slot.described;
        slot.described = true;
        drop(slot);
        drop(staging);
        self.video_wake.notify_all();

        if first {
            describe_first_picture(&image, spec);
        }
    }

    /// `stream:didStopWithError:`.
    fn on_stopped(&self, error: &NSError) {
        let stop = StreamStop {
            by_user: error.code() == SCStreamErrorCode::UserStopped.0,
            message: error_text(error),
        };
        if self.frame.is_none() {
            // Nobody polls a sound-only stream for its state.
            log::warn(format!(
                "audio: ScreenCaptureKit stopped the stream: {}",
                stop.message
            ));
        }
        self.video().stopped = Some(stop);
        self.video_wake.notify_all();
    }
}

/// The `SCFrameStatus` a screen sample carries in its attachments, if any.
fn frame_status(sample: &CMSampleBuffer) -> Option<i64> {
    // SAFETY: `sample` is valid for the duration of the callback; `false` = do not create the array.
    let attachments = unsafe { sample.sample_attachments_array(false) }?;
    if attachments.count() < 1 {
        return None;
    }
    // SAFETY: index 0 is in bounds (checked above). The array holds one CFDictionary per sample; the
    // element is only treated as "some CF object" here and its type is checked before use. The reference
    // cannot outlive `attachments`, which owns the element.
    let first = unsafe { attachments.value_at_index(0).cast::<CFType>().as_ref() }?;
    let info = first.downcast_ref::<CFDictionary>()?;
    // SAFETY: a constant NSString exported by ScreenCaptureKit, valid for the life of the process.
    let key: &NSString = unsafe { SCStreamFrameInfoStatus };
    // SAFETY: NSString is toll-free bridged to CFString, the key type of this dictionary, so the pointer
    // is a valid key; the value (if any) is an object the dictionary keeps alive while `attachments` does.
    let value = unsafe {
        info.value((key as *const NSString).cast())
            .cast::<CFType>()
            .as_ref()
    }?;
    value.downcast_ref::<CFNumber>()?.as_i64()
}

/// A pixel buffer whose memory is mapped for reading until this is dropped.
struct LockedPixels<'a> {
    buffer: &'a CVPixelBuffer,
}

impl<'a> LockedPixels<'a> {
    fn lock(buffer: &'a CVPixelBuffer) -> Result<Self, String> {
        // SAFETY: `buffer` is a valid pixel buffer. A successful lock is undone exactly once, with the same
        // flags, in `drop`.
        let status =
            unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) };
        if status != kCVReturnSuccess {
            return Err(format!(
                "cannot map the captured picture (CVPixelBufferLockBaseAddress: {status})"
            ));
        }
        Ok(Self { buffer })
    }

    /// The bytes of plane `index` and the distance between its rows: `rows` rows, each holding at least
    /// `row_bytes` bytes of `samples_wide` samples.
    fn plane(
        &self,
        index: usize,
        samples_wide: usize,
        rows: usize,
        row_bytes: usize,
    ) -> Result<(&[u8], usize), String> {
        let base = CVPixelBufferGetBaseAddressOfPlane(self.buffer, index) as *const u8;
        let stride = CVPixelBufferGetBytesPerRowOfPlane(self.buffer, index);
        let plane_width = CVPixelBufferGetWidthOfPlane(self.buffer, index);
        let plane_rows = CVPixelBufferGetHeightOfPlane(self.buffer, index);
        if base.is_null()
            || plane_width != samples_wide
            || plane_rows != rows
            || stride < row_bytes
            || rows == 0
        {
            return Err(format!(
                "plane {index} of the captured picture is {plane_width}x{plane_rows} with {stride} bytes \
                 per row where {samples_wide}x{rows} with at least {row_bytes} bytes per row was expected"
            ));
        }
        let length = stride * (rows - 1) + row_bytes;
        // SAFETY: while the buffer is locked, plane `index` is mapped at `base` as `plane_rows` rows that
        // start `stride` bytes apart and hold at least `row_bytes` bytes each (rows == plane_rows and
        // stride >= row_bytes were checked above), so `length` bytes are readable. The slice borrows `self`
        // and therefore cannot outlive the lock; nothing writes to a buffer ScreenCaptureKit has handed
        // out while a reader holds it.
        Ok((unsafe { std::slice::from_raw_parts(base, length) }, stride))
    }
}

impl Drop for LockedPixels<'_> {
    fn drop(&mut self) {
        // SAFETY: undoes the lock taken in `lock`, with the same flags.
        unsafe { CVPixelBufferUnlockBaseAddress(self.buffer, CVPixelBufferLockFlags::ReadOnly) };
    }
}

/// Copies (or converts) the picture in `image` into `frame`, the packed layout of design.md section 2.2:
/// the Y plane, then the interleaved chroma plane, no padding.
fn unpack(image: &CVPixelBuffer, spec: FrameSpec, frame: &mut [u8]) -> Result<(), String> {
    let (width, height) = (spec.width as usize, spec.height as usize);
    let format = CVPixelBufferGetPixelFormatType(image);
    let (got_width, got_height) = (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image));
    if format != spec.plan.pixel_format() || got_width != width || got_height != height {
        return Err(format!(
            "ScreenCaptureKit delivered {} {got_width}x{got_height} pictures where {} {width}x{height} was configured",
            fourcc(format),
            fourcc(spec.plan.pixel_format()),
        ));
    }
    let planes = CVPixelBufferGetPlaneCount(image);
    if planes != 2 {
        return Err(format!(
            "the captured {} picture has {planes} planes instead of 2",
            fourcc(format)
        ));
    }
    if frame.len() != spec.frame_bytes() {
        return Err("the frame buffer does not match the configured picture".to_owned());
    }

    let pixels = LockedPixels::lock(image)?;
    let result = match spec.plan {
        PixelPlan::Nv12 | PixelPlan::P010 => {
            let sample_bytes = if spec.plan == PixelPlan::P010 { 2 } else { 1 };
            // One row of either plane: W luma samples, or W/2 (Cb, Cr) pairs.
            let row_bytes = width * sample_bytes;
            let (luma, luma_stride) = pixels.plane(0, width, height, row_bytes)?;
            let (chroma, chroma_stride) = pixels.plane(1, width / 2, height / 2, row_bytes)?;
            let (frame_luma, frame_chroma) = frame.split_at_mut(row_bytes * height);
            copy_plane(luma, luma_stride, row_bytes, height, frame_luma).and_then(|()| {
                copy_plane(chroma, chroma_stride, row_bytes, height / 2, frame_chroma)
            })
        }
        PixelPlan::P010From444 => {
            let (luma, luma_stride) = pixels.plane(0, width, height, width * 2)?;
            let (chroma, chroma_stride) = pixels.plane(1, width, height, width * 4)?;
            xf44_to_p010(
                luma,
                luma_stride,
                chroma,
                chroma_stride,
                width,
                height,
                frame,
            )
        }
    };
    result.map_err(|reason| format!("cannot unpack the captured picture: {reason}"))
}

/// One colour attachment of a pixel buffer as text; `-` when it carries none.
fn colour_tag(image: &CVPixelBuffer, key: &CFString) -> String {
    // SAFETY: `image` and `key` are valid; the attachment-mode out-pointer may be null. The value comes
    // back retained (Copy rule) and is released by the wrapper.
    match unsafe { image.attachment(key, std::ptr::null_mut()) } {
        Some(value) => value
            .downcast_ref::<CFString>()
            .map_or_else(|| "?".to_owned(), ToString::to_string),
        None => "-".to_owned(),
    }
}

/// Logs what the stream really delivers, once per stream: the one place where the colour handling this
/// backend asked ScreenCaptureKit for can be seen to have happened (or not).
fn describe_first_picture(image: &CVPixelBuffer, spec: FrameSpec) {
    // SAFETY (all statics below): constant CFStrings exported by CoreVideo, valid for the life of the
    // process.
    let (matrix_key, primaries_key, transfer_key) = unsafe {
        (
            kCVImageBufferYCbCrMatrixKey,
            kCVImageBufferColorPrimariesKey,
            kCVImageBufferTransferFunctionKey,
        )
    };
    let matrix = colour_tag(image, matrix_key);
    let primaries = colour_tag(image, primaries_key);
    let transfer = colour_tag(image, transfer_key);
    log::info(format!(
        "capture: first picture {} {}x{}, bytes per row {} / {}, matrix={matrix} primaries={primaries} transfer={transfer}",
        fourcc(CVPixelBufferGetPixelFormatType(image)),
        CVPixelBufferGetWidth(image),
        CVPixelBufferGetHeight(image),
        CVPixelBufferGetBytesPerRowOfPlane(image, 0),
        CVPixelBufferGetBytesPerRowOfPlane(image, 1),
    ));

    // What ffmpeg is told the stream is (design.md section 2.5): bt2020nc / smpte2084 / bt2020 for HDR,
    // bt709 for SDR. A tag that says otherwise means the pictures do not match their description.
    // SAFETY: constant CFStrings exported by CoreVideo, as above.
    let (matrix_2020, primaries_2020, transfer_pq, matrix_709) = unsafe {
        (
            kCVImageBufferYCbCrMatrix_ITU_R_2020.to_string(),
            kCVImageBufferColorPrimaries_ITU_R_2020.to_string(),
            kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ.to_string(),
            kCVImageBufferYCbCrMatrix_ITU_R_709_2.to_string(),
        )
    };
    let expected: &[(&str, &str, &str)] = if spec.plan.is_hdr() {
        &[
            ("matrix", matrix.as_str(), matrix_2020.as_str()),
            ("primaries", primaries.as_str(), primaries_2020.as_str()),
            ("transfer", transfer.as_str(), transfer_pq.as_str()),
        ]
    } else {
        &[("matrix", matrix.as_str(), matrix_709.as_str())]
    };
    for (what, got, wanted) in expected {
        if *got != "-" && got != wanted {
            log::warn(format!(
                "capture: the pictures are tagged {what}={got} where {wanted} is what the stream is \
                 described as; colours will be off until the capture configuration is corrected"
            ));
        }
    }
}

/// The instance variables of [`StreamOutput`].
pub(super) struct OutputIvars {
    shared: Arc<Shared>,
    /// The epoch of the stream this object belongs to.
    epoch: u64,
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - StreamOutput does not implement Drop.
    // The instance variables are immutable after `new` and thread-safe (`Arc<Shared>`, an integer), so the
    // object may be used from the dispatch queues ScreenCaptureKit calls it on.
    #[unsafe(super(NSObject))]
    #[ivars = OutputIvars]
    pub(super) struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    // SAFETY: the signature is that of the protocol method: (SCStream *, CMSampleBufferRef,
    // SCStreamOutputType), returning void.
    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output_sample_buffer(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            self.deliver(sample, kind);
        }
    }

    // SAFETY: the signature is that of the protocol method: (SCStream *, NSError *), returning void.
    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_did_stop_with_error(&self, _stream: &SCStream, error: &NSError) {
            self.stopped(error);
        }
    }
);

impl StreamOutput {
    pub(super) fn new(shared: Arc<Shared>, epoch: u64) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { shared, epoch });
        // SAFETY: `init` is NSObject's designated initialiser and the instance variables are set.
        unsafe { msg_send![super(this), init] }
    }

    /// True while the stream this object was made for is the current one.
    fn current(&self) -> bool {
        let ivars = self.ivars();
        ivars.epoch == ivars.shared.epoch.load(Ordering::SeqCst)
    }

    /// Runs a callback body. A panic must not unwind into the Objective-C frames that called us; it ends
    /// the session through the normal fatal path instead.
    fn guarded(&self, body: impl FnOnce(&Shared)) {
        let shared = &self.ivars().shared;
        let outcome = catch_unwind(AssertUnwindSafe(|| autoreleasepool(|_| body(shared))));
        if let Err(panic) = outcome {
            let reason = panic
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "no message".to_owned());
            shared.fail_video(format!("a capture callback panicked: {reason}"));
        }
    }

    fn deliver(&self, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
        if !self.current() {
            return;
        }
        self.guarded(|shared| {
            if kind == SCStreamOutputType::Screen {
                shared.on_picture(sample);
            } else if kind == SCStreamOutputType::Audio {
                audio::deliver(&mut lock(&shared.audio), sample);
            }
        });
    }

    fn stopped(&self, error: &NSError) {
        if !self.current() {
            return;
        }
        self.guarded(|shared| shared.on_stopped(error));
    }
}
