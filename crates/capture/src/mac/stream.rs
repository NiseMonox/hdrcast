//! One capture session: an `SCStream` on one display, with a screen output for the pictures and / or an
//! audio output for the system sound.
//!
//! Ownership: [`Session`] is shared (`Arc`) by the video source and the audio source of one engine; the
//! backend only keeps a `Weak` so that `open_audio` can find the stream `open_video` started. Each source
//! takes its output off the stream when it is dropped, and the stream itself is stopped when the last of
//! them lets go of the session. The `SCStream` object can be replaced inside a living session
//! ([`Session::restart`], after ScreenCaptureKit ended it), which is why the sound survives a recovery of
//! the picture.
//!
//! Threads: everything here runs on the engine's thread. ScreenCaptureKit's completion handlers are
//! bridged to it with a channel and a time limit: they arrive on queues of the framework's own, so nothing
//! depends on a run loop (the engine has none) or on the main thread.

use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::ProtocolObject;
use objc2::{msg_send, AnyThread};
use objc2_core_foundation::CFString;
use objc2_core_graphics::{
    kCGColorSpaceITUR_2100_PQ, kCGColorSpaceSRGB, kCGDisplayStreamYCbCrMatrix_ITU_R_709_2,
    CGDirectDisplayID,
};
use objc2_core_media::CMTime;
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCCaptureDynamicRange, SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamConfigurationPreset, SCStreamDelegate, SCStreamErrorCode, SCStreamOutput,
    SCStreamOutputType, SCWindow,
};

use super::output::{FrameSpec, PixelPlan, Shared, StreamOutput};
use super::{permission, system};
use crate::log;
use crate::platform::AudioSink;
use crate::repack::fourcc;
use crate::resample::{TARGET_CHANNELS, TARGET_SAMPLE_RATE};

/// How long a completion handler of ScreenCaptureKit is waited for when a stream is set up.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
/// ... and when one is torn down (nothing depends on the answer then).
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// Surfaces ScreenCaptureKit keeps for pictures on their way to the callback. The callback copies and
/// returns, holding on to nothing, so the documented minimum is enough; a newer picture replacing an
/// undelivered one is exactly what the engine wants.
const QUEUE_DEPTH: isize = 3;

/// What a stream is configured with. Fixed for the life of a session (the engine's frame layout depends on
/// it), except that the outputs can be taken away.
#[derive(Debug, Clone)]
pub(super) struct StreamParams {
    pub display_id: CGDirectDisplayID,
    /// Output size in pixels; ScreenCaptureKit scales the display to it.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub cursor: bool,
    pub plan: PixelPlan,
    /// Deliver pictures (add the screen output).
    pub video: bool,
    /// Capture the system audio (`capturesAudio`).
    pub audio: bool,
}

impl StreamParams {
    fn frame(&self) -> Option<FrameSpec> {
        self.video.then_some(FrameSpec {
            plan: self.plan,
            width: self.width,
            height: self.height,
        })
    }
}

/// `NSError` as text for the log: the description plus what identifies it in Apple's documentation.
pub(super) fn error_text(error: &NSError) -> String {
    let text = format!(
        "{} ({} {})",
        error.localizedDescription(),
        error.domain(),
        error.code()
    );
    if error.code() == SCStreamErrorCode::UserDeclined.0 {
        // What ScreenCaptureKit answers when the screen-recording permission is missing.
        format!("{text}; {}", permission::HOW_TO_ALLOW)
    } else {
        text
    }
}

/// Carries the retained shareable content from the completion handler to the thread waiting for it.
struct Handoff(Retained<SCShareableContent>);

// SAFETY: the object is retained in the completion handler and from then on owned by this value alone: the
// handler does not keep a reference, so whichever thread ends up with the `Handoff` has exclusive use of
// it (or releases it, if nobody is waiting any more). SCShareableContent is an immutable snapshot with no
// ties to the thread it was created on.
unsafe impl Send for Handoff {}

/// The displays (and windows, which are not looked at) ScreenCaptureKit is willing to capture right now.
fn shareable_content() -> Result<Retained<SCShareableContent>, String> {
    let (answer, waiting) = mpsc::channel();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: ScreenCaptureKit passes a valid object or nil for each parameter, valid for the
            // duration of the call; the content is retained here, before it leaves the block.
            let result = match unsafe { Retained::retain(content) } {
                Some(content) => Ok(Handoff(content)),
                // SAFETY: as above.
                None => Err(unsafe { error.as_ref() }
                    .map_or_else(|| "no content and no error".to_owned(), error_text)),
            };
            let _ = answer.send(result);
        },
    );
    // SAFETY: the block has the signature the method declares. ScreenCaptureKit copies it and calls it
    // once, on a queue of its own; all it owns is the sending end of a channel.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    match waiting.recv_timeout(ANSWER_TIMEOUT) {
        Ok(Ok(content)) => Ok(content.0),
        Ok(Err(message)) => Err(format!(
            "ScreenCaptureKit does not list the displays: {message}"
        )),
        Err(_) => Err(format!(
            "ScreenCaptureKit did not list the displays within {}s",
            ANSWER_TIMEOUT.as_secs()
        )),
    }
}

/// The block type of the completion handlers that report only an error.
type ErrorHandler = RcBlock<dyn Fn(*mut NSError)>;

/// A completion handler for the calls that report only an error, and where its answer arrives (`None` =
/// no error).
fn completion() -> (ErrorHandler, mpsc::Receiver<Option<String>>) {
    let (answer, waiting) = mpsc::channel();
    let handler = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: nil or a valid NSError for the duration of the call.
        let _ = answer.send(unsafe { error.as_ref() }.map(error_text));
    });
    (handler, waiting)
}

fn start_capture(stream: &SCStream) -> Result<(), String> {
    let (handler, waiting) = completion();
    // SAFETY: the block has the signature the method declares; see `shareable_content`.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
    match waiting.recv_timeout(ANSWER_TIMEOUT) {
        Ok(None) => Ok(()),
        Ok(Some(message)) => Err(format!(
            "ScreenCaptureKit cannot start the capture: {message}"
        )),
        Err(_) => Err(format!(
            "ScreenCaptureKit did not start the capture within {}s",
            ANSWER_TIMEOUT.as_secs()
        )),
    }
}

/// Stops a stream and waits a moment for it. Errors are of no interest: a stream ScreenCaptureKit already
/// ended answers with one.
fn stop_capture(stream: &SCStream) {
    let (handler, waiting) = completion();
    // SAFETY: as in `start_capture`.
    unsafe { stream.stopCaptureWithCompletionHandler(Some(&handler)) };
    let _ = waiting.recv_timeout(STOP_TIMEOUT);
}

/// A `CFStringRef` property of the configuration as text; `None` when it is not set.
fn string_property(value: *const CFString) -> Option<String> {
    // SAFETY: the pointer was just returned by a getter of a configuration the caller still holds, which
    // keeps the string alive (these properties hold constants of CoreGraphics in practice); it is only
    // read here.
    unsafe { value.as_ref() }.map(ToString::to_string)
}

/// The stream configuration for `params` (design.md sections 2.2 - 2.4, in ScreenCaptureKit's terms).
fn configuration(params: &StreamParams) -> Retained<SCStreamConfiguration> {
    let hdr = params.video && params.plan.is_hdr();
    // SAFETY (the whole function): plain property accesses on a configuration object this function owns.
    // Every selector newer than ScreenCaptureKit itself is checked before use (`system`); the CFString
    // properties are not retained by the object, and are only ever given constants that live forever.
    let config = if hdr && system::hdr_preset_supported() {
        // Apple's own starting point for HDR that is meant for other screens than this one. Whatever it
        // sets beyond the properties below (its YCbCr matrix above all - there is no public constant for
        // BT.2020 among the kCGDisplayStreamYCbCrMatrix values) is kept.
        let preset = unsafe {
            SCStreamConfiguration::streamConfigurationWithPreset(
                SCStreamConfigurationPreset::CaptureHDRStreamCanonicalDisplay,
            )
        };
        let (format, space, matrix) = unsafe {
            let space: *const CFString = msg_send![&*preset, colorSpaceName];
            let matrix: *const CFString = msg_send![&*preset, colorMatrix];
            (preset.pixelFormat(), space, matrix)
        };
        log::info(format!(
            "capture: the system's HDR preset uses pixel format {}, colour space {}, matrix {}",
            fourcc(format),
            string_property(space).as_deref().unwrap_or("(unset)"),
            string_property(matrix).as_deref().unwrap_or("(unset)"),
        ));
        preset
    } else {
        unsafe { SCStreamConfiguration::new() }
    };

    unsafe {
        config.setWidth(params.width as usize);
        config.setHeight(params.height as usize);
        // At most one picture per frame interval of the engine; it repeats pictures itself when the
        // screen is idle.
        config.setMinimumFrameInterval(CMTime::new(1, params.fps.clamp(1, 480) as i32));
        config.setQueueDepth(QUEUE_DEPTH);
        config.setShowsCursor(params.cursor);
        // Always fill the requested size, like the scaler of the Windows engine: up as well as down ...
        config.setScalesToFit(true);
        if system::aspect_ratio_switch_supported() {
            // ... and without bars when the rounded size is a pixel off the display's aspect ratio.
            config.setPreservesAspectRatio(false);
        }
        config.setPixelFormat(params.plan.pixel_format());
        match params.plan {
            PixelPlan::Nv12 => {
                // BT.709 limited range from sRGB-encoded values: what the nv12 path of the Windows shader
                // produces.
                config.setColorSpaceName(kCGColorSpaceSRGB);
                config.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_709_2);
            }
            PixelPlan::P010 | PixelPlan::P010From444 => {
                // HDR10: PQ on BT.2020 primaries, rendered for a reference HDR display rather than for
                // this particular panel.
                config.setColorSpaceName(kCGColorSpaceITUR_2100_PQ);
                if system::hdr_capture_obstacle().is_none() {
                    config.setCaptureDynamicRange(SCCaptureDynamicRange::HDRCanonicalDisplay);
                }
            }
        }
        if params.audio && system::audio_capture_supported() {
            config.setCapturesAudio(true);
            config.setSampleRate(TARGET_SAMPLE_RATE as isize);
            config.setChannelCount(TARGET_CHANNELS as isize);
            // Nothing this process plays may come back through its own stream.
            config.setExcludesCurrentProcessAudio(true);
        }
    }
    config
}

/// An `SCStream`. See the `Send` implementation for how it is used.
struct StreamHandle(Retained<SCStream>);

// SAFETY: SCStream is not tied to the thread that created it (it is not a main-thread class; its work
// happens in a system service and its callbacks arrive on dispatch queues). The handle is only reachable
// through `Session::state`, a mutex, so no two threads ever message the stream at the same time; all `Send`
// adds is that the engine's thread may be a different one from the thread that built the backend.
unsafe impl Send for StreamHandle {}

/// A started stream and what is attached to it.
struct Running {
    stream: StreamHandle,
    /// The output / delegate object of this stream; kept for as long as the stream so that it certainly
    /// outlives every reference ScreenCaptureKit holds without retaining.
    output: Retained<StreamOutput>,
    screen_output: bool,
    audio_output: bool,
}

impl Running {
    fn as_output(&self) -> &ProtocolObject<dyn SCStreamOutput> {
        ProtocolObject::from_ref(&*self.output)
    }

    /// Adds the output for pictures or sound; its callbacks then arrive on `queue`.
    fn add_output(&self, kind: SCStreamOutputType, queue: &DispatchQueue) -> Result<(), String> {
        // SAFETY: the output object implements SCStreamOutput and outlives the stream (see `output`); the
        // queue is a serial queue owned by the session, with no threading requirements of its own.
        unsafe {
            self.stream.0.addStreamOutput_type_sampleHandlerQueue_error(
                self.as_output(),
                kind,
                Some(queue),
            )
        }
        .map_err(|error| error_text(&error))
    }

    fn remove_output(&self, kind: SCStreamOutputType) {
        // SAFETY: plain call on a live stream with the object it was given in `add_output`. An error (the
        // stream is gone already) changes nothing for the caller.
        let _ = unsafe {
            self.stream
                .0
                .removeStreamOutput_type_error(self.as_output(), kind)
        };
    }

    /// Takes the outputs off and stops the stream.
    fn shut_down(self) {
        if self.screen_output {
            self.remove_output(SCStreamOutputType::Screen);
        }
        if self.audio_output {
            self.remove_output(SCStreamOutputType::Audio);
        }
        stop_capture(&self.stream.0);
    }
}

/// Why a stream did not start.
enum StartError {
    /// ScreenCaptureKit answered the start request with an error: a configuration it does not accept, an
    /// audio capture it cannot begin, a missing permission.
    Refused(String),
    /// Everything before that: no such display, no answer, an output that cannot be added.
    Failed(String),
}

impl StartError {
    fn into_message(self) -> String {
        match self {
            StartError::Refused(message) | StartError::Failed(message) => message,
        }
    }
}

struct State {
    params: StreamParams,
    /// `None` between a failed restart and the next one that works.
    running: Option<Running>,
    /// An audio source holds the session.
    audio_attached: bool,
}

/// See the module documentation.
pub(super) struct Session {
    shared: Arc<Shared>,
    video_queue: DispatchRetained<DispatchQueue>,
    audio_queue: DispatchRetained<DispatchQueue>,
    state: Mutex<State>,
}

// A session is used by the engine's thread while the backend, which may be moved to that thread from
// another, holds a weak reference to it.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Session>();
};

impl Session {
    /// Creates the stream described by `params` and starts it.
    pub(super) fn start(params: StreamParams) -> Result<Arc<Self>, String> {
        if !system::screen_capture_kit_available() {
            return Err(
                "ScreenCaptureKit is not available: screen capture needs macOS 12.3 or later"
                    .to_owned(),
            );
        }
        let mut params = params;
        let shared = Shared::new(params.frame());
        let video_queue = DispatchQueue::new("hdrcast.capture.pictures", DispatchQueueAttr::SERIAL);
        let audio_queue = DispatchQueue::new("hdrcast.capture.sound", DispatchQueueAttr::SERIAL);
        let running = match start_stream(&params, &shared, &video_queue, &audio_queue) {
            Ok(running) => running,
            // The sound must never cost the pictures (design.md section 2.7: a session without audio
            // carries silence). ScreenCaptureKit starts both together, so a refusal is tried once more
            // without the sound before it counts.
            Err(StartError::Refused(message)) if params.audio && params.video => {
                log::warn(format!(
                    "capture: {message}; trying once more without the system audio"
                ));
                params.audio = false;
                start_stream(&params, &shared, &video_queue, &audio_queue)
                    .map_err(StartError::into_message)?
            }
            Err(error) => return Err(error.into_message()),
        };
        Ok(Arc::new(Self {
            shared,
            video_queue,
            audio_queue,
            state: Mutex::new(State {
                params,
                running: Some(running),
                audio_attached: false,
            }),
        }))
    }

    pub(super) fn shared(&self) -> &Shared {
        &self.shared
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replaces the stream by a new one with the same configuration (after ScreenCaptureKit ended it).
    pub(super) fn restart(&self) -> Result<(), String> {
        let mut state = self.state();
        self.restart_locked(&mut state)
    }

    fn restart_locked(&self, state: &mut State) -> Result<(), String> {
        if let Some(old) = state.running.take() {
            autoreleasepool(|_| old.shut_down());
        }
        let started = start_stream(
            &state.params,
            &self.shared,
            &self.video_queue,
            &self.audio_queue,
        );
        state.running = Some(started.map_err(StartError::into_message)?);
        Ok(())
    }

    /// Starts handing the system audio to `sink`, from the audio queue, one call at a time.
    pub(super) fn attach_audio(&self, sink: AudioSink) -> Result<(), String> {
        let mut state = self.state();
        if !state.params.audio {
            // Sound cannot be switched on in a running stream without replacing it, and replacing it
            // would risk the pictures for the sake of the sound.
            return Err("this capture session runs without the system audio".to_owned());
        }
        if state.audio_attached {
            return Err("this capture session already has an audio consumer".to_owned());
        }
        // Normally there since the stream started; missing after an earlier audio source took it off, or
        // when the stream refused it. (With no stream at all - between a loss and the recovery - the new
        // stream gets it.)
        if let Some(running) = state.running.as_mut() {
            if !running.audio_output {
                running
                    .add_output(SCStreamOutputType::Audio, &self.audio_queue)
                    .map_err(|error| format!("cannot add the audio output: {error}"))?;
                running.audio_output = true;
            }
        }
        self.shared
            .audio
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .attach(sink);
        state.audio_attached = true;
        Ok(())
    }

    /// After this returns the sink is not called any more.
    fn clear_sink(&self) {
        // Waits for a callback that is in the middle of a delivery: it holds this lock.
        self.shared
            .audio
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .detach();
    }

    /// The audio source is gone.
    pub(super) fn detach_audio(&self) {
        self.clear_sink();
        let mut state = self.state();
        state.audio_attached = false;
        if let Some(running) = state.running.as_mut() {
            if running.audio_output {
                autoreleasepool(|_| running.remove_output(SCStreamOutputType::Audio));
                running.audio_output = false;
            }
        }
    }

    /// The video source is gone. A stream that still feeds an audio source keeps running without pictures.
    pub(super) fn detach_video(&self) {
        let mut state = self.state();
        state.params.video = false;
        if let Some(running) = state.running.as_mut() {
            if running.screen_output {
                autoreleasepool(|_| running.remove_output(SCStreamOutputType::Screen));
                running.screen_output = false;
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        if let Some(running) = state.running.take() {
            autoreleasepool(|_| running.shut_down());
        }
    }
}

/// Builds a stream for `params` on the display's current state and starts it.
fn start_stream(
    params: &StreamParams,
    shared: &Arc<Shared>,
    video_queue: &DispatchQueue,
    audio_queue: &DispatchQueue,
) -> Result<Running, StartError> {
    autoreleasepool(|_| {
        let content = shareable_content().map_err(StartError::Failed)?;
        // SAFETY: plain property reads on the objects ScreenCaptureKit just handed over.
        let displays = unsafe { content.displays() }.to_vec();
        let display = displays
            .iter()
            // SAFETY: as above.
            .find(|display| unsafe { display.displayID() } == params.display_id)
            .ok_or_else(|| {
                StartError::Failed(format!(
                    "display {} is not available for capture (disconnected or asleep?)",
                    params.display_id
                ))
            })?;

        // The whole display: no window is left out.
        let no_windows = NSArray::<SCWindow>::new();
        // SAFETY: plain initialisers with valid arguments. The delegate is the object that is kept in
        // `Running` for as long as the stream.
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                display,
                &no_windows,
            )
        };
        let config = configuration(params);
        let output = StreamOutput::new(Arc::clone(shared), shared.begin_epoch());
        let delegate: &ProtocolObject<dyn SCStreamDelegate> = ProtocolObject::from_ref(&*output);
        // SAFETY: as above.
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(delegate),
            )
        };

        let mut running = Running {
            stream: StreamHandle(stream),
            output,
            screen_output: false,
            audio_output: false,
        };
        if params.video {
            running
                .add_output(SCStreamOutputType::Screen, video_queue)
                .map_err(|error| {
                    StartError::Failed(format!("cannot add the screen output: {error}"))
                })?;
            running.screen_output = true;
        }
        // The audio output is there from the start whenever the stream captures sound - before anybody
        // listens, too (the callback drops what has no sink) - so that nothing depends on a running
        // stream accepting another output. Sound is never worth the pictures: failing here is not fatal.
        if params.audio && system::audio_capture_supported() {
            match running.add_output(SCStreamOutputType::Audio, audio_queue) {
                Ok(()) => running.audio_output = true,
                Err(error) => log::warn(format!("audio: cannot add the audio output: {error}")),
            }
        }
        start_capture(&running.stream.0).map_err(StartError::Refused)?;
        Ok(running)
    })
}
