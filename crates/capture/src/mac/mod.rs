//! The macOS backend: ScreenCaptureKit for the picture and the system sound, FIFOs towards ffmpeg.
//!
//! Written on a machine that cannot run it: it is type-checked for both Mac targets, and everything that
//! only a Mac can show is listed, step by step, in `docs/macos-checklist.md`. The pure parts (plane
//! packing, the 4:4:4 conversion, PCM interleaving) live in [`crate::repack`] and are tested everywhere.
//!
//! * [`displays`]: the outputs, from Quartz Display Services and `NSScreen`.
//! * [`stream`]: the session - one `SCStream` shared by the video and the audio source.
//! * [`output`]: the Objective-C object ScreenCaptureKit calls back, and the "latest frame" slot.
//! * [`video`] / [`audio`]: the two sources the engine sees.
//! * [`system`], [`permission`]: what this Mac supports and whether it lets us.
//! * pipes, parent watch, signals, local time: [`crate::unix`].
//!
//! `unsafe` here is FFI only; each block states what it relies on.

mod audio;
mod displays;
mod output;
mod permission;
mod stream;
mod system;
mod timer;
mod video;

use std::io;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use objc2::rc::autoreleasepool;

use crate::args::CaptureArgs;
use crate::engine::exit_codes;
use crate::log;
use crate::output::OutputInfo;
use crate::platform::{
    AudioDeviceInfo, AudioSink, AudioSource, Backend, PipeServer, ProcessWatch, VideoSource,
};
use crate::unix::{ignore_sigpipe, FifoServer, PidWatch};
use stream::Session;

pub(crate) use crate::unix::{
    install_stop_handler, local_time_of_day, pipe_listening, pipe_path, remove_stop_handler,
};
pub(crate) use timer::PreciseSleep;

/// One backend serves one session. The picture and the sound of a Mac come from the same `SCStream`, so
/// the backend remembers the session `open_video` started for `open_audio` to join - weakly: the sources
/// own it, and it ends with the last of them.
pub(crate) struct NativeBackend {
    session: Mutex<Weak<Session>>,
}

impl NativeBackend {
    pub(crate) fn new() -> Self {
        Self {
            session: Mutex::new(Weak::new()),
        }
    }

    fn remember(&self, session: &Arc<Session>) {
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = Arc::downgrade(session);
    }
}

impl Backend for NativeBackend {
    fn open_video(&self, args: &CaptureArgs) -> Result<Box<dyn VideoSource>, String> {
        let source = autoreleasepool(|_| video::open(args))?;
        self.remember(source.session());
        Ok(Box::new(source))
    }

    /// A FIFO; its kernel buffer cannot be sized (see [`crate::unix`]), so `_out_buffer_bytes` is unused.
    fn create_pipe(&self, name: &str, _out_buffer_bytes: usize) -> io::Result<Box<dyn PipeServer>> {
        Ok(Box::new(FifoServer::create(name)?))
    }

    fn open_audio(
        &self,
        device: &str,
        exclude_app: &str,
        sink: AudioSink,
    ) -> Result<Box<dyn AudioSource>, String> {
        let existing = self
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .upgrade();
        let source = autoreleasepool(|_| audio::open(existing, device, exclude_app, sink))?;
        self.remember(source.session());
        Ok(Box::new(source))
    }

    fn watch_process(&self, pid: u32) -> Option<Box<dyn ProcessWatch>> {
        Some(Box::new(PidWatch::open(pid)?))
    }

    fn self_test(&self, _sdr_white: f32) -> i32 {
        self_test()
    }
}

/// One entry per active display, main display first. Needs no permission.
///
/// `hdr` is true when the engine would capture the display as HDR10: the panel is in an HDR state AND
/// this system can capture HDR (see [`video::hdr_verdict`]). The app turns `hdr` into the pixel format it
/// announces to ffmpeg, so it must not promise what the engine will not deliver.
pub(crate) fn enumerate_outputs() -> Result<Vec<OutputInfo>, String> {
    autoreleasepool(|_| {
        Ok(displays::list()?
            .iter()
            .enumerate()
            .map(|(index, display)| {
                display.to_output_info(index as u32, video::hdr_verdict(display).is_ok())
            })
            .collect())
    })
}

/// The system audio of a Mac is captured as a whole: there is no device to choose.
pub(crate) fn list_audio_devices() -> Vec<AudioDeviceInfo> {
    Vec::new()
}

/// Leaving one program out is not supported on macOS yet, so there is nothing to offer.
pub(crate) fn list_audio_apps(_device: &str) -> Vec<String> {
    Vec::new()
}

pub(crate) fn prepare_cli() {
    // A consumer that closes its end of a pipe (or of stdout) must show up as a write error.
    ignore_sigpipe();
}

/// `capture --selftest`. On Windows this runs HdrCast's own conversion shader on a synthetic gradient and
/// compares the GPU's output with the reference implementation. On macOS the conversion is done by
/// ScreenCaptureKit - there is no shader of ours to verify - so this reports what the capture depends on
/// instead: the system's capabilities, the permission and the displays. Nothing is captured and nothing is
/// asked of the user.
fn self_test() -> i32 {
    let yes_no = |value: bool| if value { "yes" } else { "no" };
    let mut out = String::from(
        "selftest: the GPU colour self-test is Windows-only (on macOS ScreenCaptureKit converts the \
         colours; HdrCast has no shader of its own to compare with the reference implementation)\n",
    );
    let kit = system::screen_capture_kit_available();
    out.push_str(&format!(
        "macOS {} on {}\n",
        system::os_version(),
        if system::apple_silicon() {
            "Apple silicon"
        } else {
            "Intel"
        }
    ));
    out.push_str(&format!(
        "  ScreenCaptureKit (macOS 12.3+):        {}\n",
        yes_no(kit)
    ));
    out.push_str(&format!(
        "  system audio capture (macOS 13+):      {}\n",
        yes_no(system::audio_capture_supported())
    ));
    out.push_str(&format!(
        "  HDR capture (macOS 15+, Apple silicon): {}\n",
        match system::hdr_capture_obstacle() {
            None => "yes".to_owned(),
            Some(reason) => format!("no - {reason}"),
        }
    ));
    out.push_str(&format!(
        "  screen recording permission:           {}\n",
        if permission::screen_capture_allowed() {
            "granted".to_owned()
        } else {
            format!("NOT granted - {}", permission::HOW_TO_ALLOW)
        }
    ));
    let displays = enumerate_outputs();
    match &displays {
        Ok(outputs) => {
            out.push_str("displays:\n");
            out.push_str(&OutputInfo::to_table(outputs));
        }
        Err(message) => out.push_str(&format!("displays: cannot be listed: {message}\n")),
    }
    let usable = kit && displays.is_ok_and(|outputs| !outputs.is_empty());
    out.push_str(if usable {
        "selftest: SKIPPED - nothing to verify without capturing; see docs/macos-checklist.md\n"
    } else {
        "selftest: FAIL - this system cannot capture the screen\n"
    });
    log::stdout_text(&out);
    if usable {
        exit_codes::OK
    } else {
        exit_codes::DEVICE_ERROR
    }
}
