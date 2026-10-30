//! The platform seam: everything the engine needs from the operating system, as small traits.
//!
//! The engine loop ([`crate::engine`]), the pacing, the audio alignment and the pipe writer threads are
//! platform-neutral and talk only to these traits. Windows implements them in `win/` (DXGI Desktop
//! Duplication + a D3D11 compute shader, WASAPI loopback, named pipes), macOS in `mac/` (ScreenCaptureKit
//! for picture and sound, FIFOs); every other platform currently gets a stub that reports "not supported on
//! this platform yet".
//!
//! How a push-based capture API (macOS / ScreenCaptureKit) fits the seam:
//! * [`VideoSource::next_frame`] is a pull of "the newest picture, if it changed". A push-based API fits by
//!   letting its callback thread park each delivered frame in a "latest frame" slot (mutex + condvar) and
//!   having `next_frame` wait on that slot for up to `timeout`, then copy / convert into `dst`.
//! * [`AudioSource`] is push-based already: deliver interleaved 48 kHz stereo f32 to the [`AudioSink`] from
//!   whatever thread the OS calls back on (one thread at a time).
//! * [`PipeServer`] is a byte pipe the encoder opens by path ([`pipe_path`]): a FIFO on unix.
//! * One [`Backend`] value serves one session (`open_video`, then `create_pipe` twice, then `open_audio`), so a
//!   backend whose picture and sound come from one OS object can keep that object in itself.
//!
//! A backend is a module (`win`, `mac`, `stub`) selected below as `imp`. Besides `NativeBackend` (the
//! [`Backend`], created with `NativeBackend::new()`) it provides the free functions this file forwards to -
//! `enumerate_outputs`, `list_audio_devices`, `list_audio_apps`, `pipe_path`, `pipe_listening`,
//! `local_time_of_day`, `prepare_cli`, `install_stop_handler`, `remove_stop_handler` - and the
//! `PreciseSleep` type behind [`crate::timer`].

use std::io;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::args::{CaptureArgs, CaptureFormat, EncodeArgs};
use crate::encode::VideoEncoder;
use crate::output::OutputInfo;

#[cfg(windows)]
use crate::win as imp;

#[cfg(target_os = "macos")]
use crate::mac as imp;

#[cfg(not(any(windows, target_os = "macos")))]
use crate::stub as imp;

/// What a [`VideoSource`] writes into the frame buffer: the resolved pixel format (never
/// [`CaptureFormat::Auto`]) and the even output size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    pub format: CaptureFormat,
    pub width: u32,
    pub height: u32,
}

impl FrameLayout {
    /// Bytes of one raw frame (see [`crate::color::frame_bytes`]).
    pub fn frame_bytes(&self) -> usize {
        crate::color::frame_bytes(self.format, self.width, self.height)
    }

    /// The line the engine prints to stdout once the display is open and before it creates the pipes
    /// (design.md section 2.2): what its consumer has to read from the video pipe, e.g.
    /// `{"layout":{"format":"p010","width":3840,"height":2160}}`. With `--format auto` the backend chooses
    /// (macOS falls back to nv12 when the system refuses HDR capture), so this is the only reliable answer.
    pub fn to_json_line(&self) -> String {
        // By hand, so the key order does not depend on serde_json's preserve_order feature.
        format!(
            r#"{{"layout":{{"format":"{}","width":{},"height":{}}}}}"#,
            self.format.name(),
            self.width,
            self.height
        )
    }

    /// Parses a [`to_json_line`](Self::to_json_line) line; `None` for anything else, the statistics lines
    /// included.
    pub fn try_parse(line: &str) -> Option<FrameLayout> {
        let line = line.trim();
        if !(line.starts_with('{') && line.contains("\"layout\"")) {
            return None;
        }
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        let layout = value.get("layout")?;
        let format = match layout.get("format")?.as_str()? {
            "p010" => CaptureFormat::P010,
            "nv12" => CaptureFormat::Nv12,
            _ => return None,
        };
        let size = |key: &str| {
            layout
                .get(key)?
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| *v >= 2 && v % 2 == 0)
        };
        Some(FrameLayout {
            format,
            width: size("width")?,
            height: size("height")?,
        })
    }
}

/// Outcome of one [`VideoSource::next_frame`] call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FrameStatus {
    /// A new picture was converted into the buffer; `gpu_ms` is what the conversion and read-back took (with
    /// zero copy only what handing the conversion to the GPU took: nothing waits for it there).
    New { gpu_ms: f64 },
    /// Nothing changed within the timeout; the buffer was not written and the engine repeats the last frame.
    Unchanged,
}

/// Why [`VideoSource::next_frame`] produced nothing.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoError {
    /// The capture was invalidated (mode change, full-screen app, session switch); the engine calls
    /// [`VideoSource::recover`] until it works or 30 s have passed. Carries the error text when the loss was
    /// not one of the expected "access lost" results.
    Lost(Option<String>),
    /// The conversion itself failed; the engine stops with the device-error exit code.
    Fatal(String),
}

/// The screen: captures one output and converts it to raw P010 / NV12 frames (design.md sections 2.2, 2.3).
/// Used from the engine's thread only.
pub trait VideoSource {
    /// Format and size of the frames this source writes.
    fn layout(&self) -> FrameLayout;

    /// Waits up to `timeout` for the picture to change. When it did (new desktop image, or the pointer moved
    /// while the cursor is drawn), writes the newest converted frame into `dst` (exactly
    /// `layout().frame_bytes()` bytes) and returns [`FrameStatus::New`]; otherwise leaves `dst` alone.
    fn next_frame(&mut self, dst: &mut [u8], timeout: Duration) -> Result<FrameStatus, VideoError>;

    /// Re-creates the capture after [`VideoError::Lost`].
    fn recover(&mut self) -> Result<(), String>;

    /// Zero copy (design.md section 2.8): an encoder on this source's own GPU device that reads the pictures
    /// straight from surfaces this source converts into, instead of frames in memory. `frames`: how many frames
    /// the engine holds at once (its queue, the latest frame, the one being converted); the encoder makes that
    /// many surfaces plus the ones it holds itself ([`VideoEncoder::surfaces`]). `None` (the default) when this
    /// source cannot feed one (no such encoder on its GPU, a feature missing; the reason is logged): the engine
    /// then opens the [`Backend`]'s encoder, which takes frames in memory.
    fn surface_encoder(
        &mut self,
        args: &EncodeArgs,
        fps: u32,
        frames: usize,
    ) -> Option<Box<dyn VideoEncoder>> {
        let _ = (args, fps, frames);
        None
    }

    /// Like [`next_frame`](Self::next_frame), but converts into surface `surface` of the encoder that
    /// [`surface_encoder`](Self::surface_encoder) returned. The engine calls it only then, and only with a
    /// surface nobody is reading.
    fn next_surface(
        &mut self,
        surface: usize,
        timeout: Duration,
    ) -> Result<FrameStatus, VideoError> {
        let _ = (surface, timeout);
        Err(VideoError::Fatal(NO_SURFACES.to_owned()))
    }

    /// Fills surface `surface` with black: the first frame, before the screen delivered one.
    fn black_surface(&mut self, surface: usize) -> Result<(), String> {
        let _ = surface;
        Err(NO_SURFACES.to_owned())
    }

    /// Control mode: puts this process's GPU work ahead of other programs' where the platform allows it. Says what
    /// came of it, for the log; `None` where there is nothing to raise.
    fn raise_gpu_priority(&mut self) -> Option<String> {
        None
    }
}

const NO_SURFACES: &str = "this screen converts into memory, not into encoder surfaces";

/// Receives captured desktop audio: interleaved stereo f32 at 48 kHz (`samples.len()` is even).
/// Called from the source's capture thread, one call at a time.
pub type AudioSink = Arc<dyn Fn(&[f32]) + Send + Sync>;

/// Desktop audio capture (design.md section 2.4). Capturing starts when the source is opened and stops when
/// it is dropped.
pub trait AudioSource {
    /// What is being captured, for the log (e.g. `IeeeFloat 48000 Hz 2 ch 32 bit`).
    fn description(&self) -> String;
}

/// Lets another thread abort the pending and all future operations of a [`PipeServer`].
#[derive(Clone)]
pub struct PipeCanceller(Arc<dyn Fn() + Send + Sync>);

impl PipeCanceller {
    pub fn new(cancel: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(cancel))
    }

    pub fn cancel(&self) {
        (self.0)();
    }
}

/// The server end of a one-way byte pipe to the encoder (design.md sections 2.2, 2.7). The pipe exists (and
/// [`pipe_listening`] sees it) from the moment it is created.
pub trait PipeServer: Send {
    /// Waits up to `timeout` for the client. `Ok(false)` = nobody yet (call again); once it returned
    /// `Ok(true)` it keeps doing so.
    fn wait_for_client(&mut self, timeout: Duration) -> io::Result<bool>;

    /// Blocks until all of `data` is in the pipe. An error means the consumer went away (or the operation was
    /// cancelled); the pipe is unusable afterwards.
    fn write_all(&mut self, data: &[u8]) -> io::Result<()>;

    fn canceller(&self) -> PipeCanceller;
}

/// Tells whether another process is still alive without enumerating processes (`--parent-pid`).
pub trait ProcessWatch {
    fn is_gone(&self) -> bool;
}

/// Factory for the platform objects one capture session needs. [`native`] is the real one; tests drive the
/// engine with fakes. `Send`, so an engine can be handed to the thread that runs it; everything a backend
/// creates stays on that thread.
pub trait Backend: Send {
    /// Opens the output named by `args.output_index` and prepares the conversion to `args.format` /
    /// `args.width` x `args.height` (0 = native size, forced even). The error text is logged after
    /// `capture: ` and the engine exits with the device-error code.
    fn open_video(&self, args: &CaptureArgs) -> Result<Box<dyn VideoSource>, String>;

    /// Creates the server end of the pipe `name` (see [`pipe_path`]) with room for `out_buffer_bytes`.
    fn create_pipe(&self, name: &str, out_buffer_bytes: usize) -> io::Result<Box<dyn PipeServer>>;

    /// Starts capturing desktop audio from `device` ("default" or an endpoint id) into `sink`, leaving out
    /// the program `exclude_app` (with everything it started) unless that is empty. On an error the session
    /// carries silence instead.
    fn open_audio(
        &self,
        device: &str,
        exclude_app: &str,
        sink: AudioSink,
    ) -> Result<Box<dyn AudioSource>, String>;

    /// A watch on process `pid`, or `None` when it cannot be opened.
    fn watch_process(&self, pid: u32) -> Option<Box<dyn ProcessWatch>>;

    /// `capture --selftest`: runs the conversion on a synthetic gradient, prints the comparison with
    /// [`crate::color`] to stdout and returns the exit code.
    fn self_test(&self, sdr_white: f32) -> i32;

    /// Opens the encoder `args` names for frames of `layout` at `fps` (`--encode`, design.md section 2.8). The
    /// error says why it cannot be had here (no such GPU, a driver too old); the engine then exits with
    /// `ENCODER_UNAVAILABLE` and the app has ffmpeg encode instead.
    fn open_encoder(&self, args: &EncodeArgs, layout: FrameLayout, fps: u32) -> Result<Box<dyn VideoEncoder>, String> {
        let _ = (layout, fps);
        Err(format!("{} is not available on this platform", args.encoder.name()))
    }
}

/// One audio render endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDeviceInfo {
    /// What `--audio-device` takes.
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// The message every entry point of an unsupported platform reports.
pub const NOT_SUPPORTED: &str = "screen capture is not supported on this platform yet";

/// The backend of the platform this was compiled for. A fresh value per session: a backend may keep the
/// session's OS object in itself (macOS does, see the notes above).
pub fn native() -> Box<dyn Backend> {
    Box::new(imp::NativeBackend::new())
}

/// The outputs in the order `capture --output` indexes them.
pub fn enumerate_outputs() -> Result<Vec<OutputInfo>, String> {
    imp::enumerate_outputs()
}

/// The active audio render endpoints (empty when they cannot be listed; the reason is logged).
pub fn list_audio_devices() -> Vec<AudioDeviceInfo> {
    imp::list_audio_devices()
}

/// Programs that have an audio session on the endpoint `device` ("default" or an id), as executable names,
/// for the "Leave out" list. This process itself is not listed.
pub fn list_audio_apps(device: &str) -> Vec<String> {
    imp::list_audio_apps(device)
}

/// The path the encoder opens for the pipe `name`: `\\.\pipe\<name>` on Windows, a FIFO under the temp
/// directory elsewhere.
pub fn pipe_path(name: &str) -> String {
    imp::pipe_path(name)
}

/// True once the pipe `name` exists and waits for a client. This only asks: it never connects.
///
/// Never test for a pipe by opening it or by querying its attributes (`Path::exists`, `fs::metadata`): on
/// Windows either one is itself a client connection. It takes the only instance, so ffmpeg then gets
/// ERROR_PIPE_BUSY (reported as "Error opening input: Invalid argument") and the engine's writes fail with a
/// broken pipe - the v0.2.0 failure that never encoded a single frame.
pub fn pipe_listening(name: &str) -> bool {
    imp::pipe_listening(name)
}

/// Local wall-clock time of day (hours, minutes, seconds, milliseconds) for the log.
pub(crate) fn local_time_of_day() -> (u32, u32, u32, u32) {
    imp::local_time_of_day()
}

/// A sleep that is accurate to a fraction of a millisecond where the platform has one.
pub(crate) type PreciseSleep = imp::PreciseSleep;

/// Called once before an engine mode of the command line runs (console attachment, DPI awareness).
pub(crate) fn prepare_cli() {
    imp::prepare_cli();
}

/// Routes Ctrl+C / console close / logoff to `stop` until [`remove_stop_handler`] is called.
pub(crate) fn install_stop_handler(stop: crate::engine::StopHandle) {
    imp::install_stop_handler(stop);
}

pub(crate) fn remove_stop_handler() {
    imp::remove_stop_handler();
}
