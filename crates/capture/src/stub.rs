//! Backend for platforms without a capture implementation yet: everything compiles, and every entry point
//! reports [`NOT_SUPPORTED`] (the engine exits with the device-error code).

use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::args::CaptureArgs;
use crate::engine::{exit_codes, StopHandle};
use crate::log;
use crate::output::OutputInfo;
use crate::platform::{
    AudioDeviceInfo, AudioSink, AudioSource, Backend, PipeServer, ProcessWatch, VideoSource,
    NOT_SUPPORTED,
};

pub(crate) struct NativeBackend;

impl NativeBackend {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl Backend for NativeBackend {
    fn open_video(&self, _args: &CaptureArgs) -> Result<Box<dyn VideoSource>, String> {
        Err(NOT_SUPPORTED.to_owned())
    }

    fn create_pipe(
        &self,
        _name: &str,
        _out_buffer_bytes: usize,
    ) -> io::Result<Box<dyn PipeServer>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, NOT_SUPPORTED))
    }

    fn open_audio(
        &self,
        _device: &str,
        _exclude_app: &str,
        _sink: AudioSink,
    ) -> Result<Box<dyn AudioSource>, String> {
        Err(NOT_SUPPORTED.to_owned())
    }

    fn watch_process(&self, _pid: u32) -> Option<Box<dyn ProcessWatch>> {
        None
    }

    fn self_test(&self, _sdr_white: f32) -> i32 {
        log::stderr_text(&format!("selftest: {NOT_SUPPORTED}\n"));
        exit_codes::DEVICE_ERROR
    }
}

pub(crate) fn enumerate_outputs() -> Result<Vec<OutputInfo>, String> {
    Err(NOT_SUPPORTED.to_owned())
}

pub(crate) fn list_audio_devices() -> Vec<AudioDeviceInfo> {
    Vec::new()
}

pub(crate) fn list_audio_apps(_device: &str) -> Vec<String> {
    Vec::new()
}

/// A FIFO path under the temp directory; the backend that creates the FIFO has to use the same one.
pub(crate) fn pipe_path(name: &str) -> String {
    std::env::temp_dir()
        .join(format!("{name}.fifo"))
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn pipe_listening(_name: &str) -> bool {
    false
}

/// UTC: the local time zone needs a platform call this stub does not make.
pub(crate) fn local_time_of_day() -> (u32, u32, u32, u32) {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = since_epoch.as_secs() % 86400;
    (
        (seconds / 3600) as u32,
        (seconds / 60 % 60) as u32,
        (seconds % 60) as u32,
        since_epoch.subsec_millis(),
    )
}

/// Plain `thread::sleep`; the caller spins the last stretch.
pub(crate) struct PreciseSleep;

impl PreciseSleep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn high_resolution(&self) -> bool {
        false
    }

    pub(crate) fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

pub(crate) fn prepare_cli() {}

pub(crate) fn install_stop_handler(_stop: StopHandle) {}

pub(crate) fn remove_stop_handler() {}
