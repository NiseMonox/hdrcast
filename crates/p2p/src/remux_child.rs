//! One viewer's ffmpeg: reads the shared MPEG-TS on stdin, copies it (no re-encode) and serves it as an SRT
//! listener on 127.0.0.1 with that viewer's latency, overhead and the group's passphrase (docs/design.md §8.8).
//! libsrt does the retransmission and pacing per viewer; [`HostLink`](crate::HostLink) carries its datagrams.

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ChildStdin;
use std::sync::{Arc, Mutex};

use hdrcast_core::srt_url::{self, SrtRole, SrtTopology, SrtUrlError, SrtUrlOptions};
use hdrcast_core::{secret_redactor, LineProcess, ProcessJob};

use crate::util::lock;

pub struct RemuxChild {
    process: LineProcess,
    srt_url: String,
    stdin: Mutex<Option<Arc<ChildStdin>>>,
}

/// The child's stdin for whoever feeds it (the fan-out). It can be handed out several times; the pipe itself
/// stays open until the [`RemuxChild`] is disposed, however many of these were dropped before.
#[derive(Debug, Clone)]
pub struct RemuxInput(Arc<ChildStdin>);

impl Write for RemuxInput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self.0).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&*self.0).flush()
    }
}

impl RemuxChild {
    pub fn new(ffmpeg_path: impl Into<OsString>, srt_url: &str, job: Option<Arc<ProcessJob>>) -> RemuxChild {
        let mut process = LineProcess::new(ffmpeg_path, RemuxChild::build_arguments(srt_url));
        process.redirect_stdin(true).job(job);
        RemuxChild { process, srt_url: srt_url.to_string(), stdin: Mutex::new(None) }
    }

    pub fn srt_url(&self) -> &str {
        &self.srt_url
    }

    /// ffmpeg's stderr lines, already redacted. Set before [`start`](Self::start).
    pub fn on_log(&mut self, mut callback: impl FnMut(&str) + Send + 'static) -> &mut Self {
        self.process.on_stderr_line(move |line| callback(&secret_redactor::redact(line)));
        self
    }

    /// Called once with the exit code, also for a child that was disposed. Set before [`start`](Self::start).
    pub fn on_exited(&mut self, callback: impl FnOnce(i32) + Send + 'static) -> &mut Self {
        self.process.on_exited(callback);
        self
    }

    pub fn start(&mut self) -> io::Result<()> {
        self.process.start()?;
        *lock(&self.stdin) = self.process.take_stdin().map(Arc::new);
        Ok(())
    }

    /// Where the MPEG-TS goes; `None` before [`start`](Self::start) and after [`dispose`](Self::dispose).
    pub fn input(&self) -> Option<RemuxInput> {
        lock(&self.stdin).clone().map(RemuxInput)
    }

    pub fn has_exited(&self) -> bool {
        self.process.has_exited()
    }

    pub fn safe_command_line(&self) -> String {
        self.process.safe_command_line()
    }

    /// Small probe (the input is already clean MPEG-TS), copy every stream, keep repeating the parameter sets
    /// for a viewer that joins mid-stream, no mux delay.
    pub fn build_arguments(srt_url: &str) -> Vec<String> {
        [
            "-hide_banner", "-loglevel", "warning",
            "-probesize", "65536", "-analyzeduration", "500000",
            "-f", "mpegts", "-i", "pipe:0",
            "-map", "0", "-c", "copy",
            "-f", "mpegts", "-mpegts_flags", "+resend_headers", "-muxdelay", "0", "-muxpreload", "0",
            srt_url,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// The per-viewer listener URL: 127.0.0.1 only; the WAN side is [`HostLink`](crate::HostLink)'s job.
    pub fn build_listener_url(port: u16, passphrase: &str, latency_ms: u32, overhead_percent: i32, bitrate_bps: u64) -> Result<String, SrtUrlError> {
        srt_url::build(&SrtUrlOptions {
            topology: SrtTopology::Listener,
            listen_address: Some("127.0.0.1".to_string()),
            port,
            role: SrtRole::Publish,
            passphrase: Some(passphrase.to_string()),
            pb_key_len: 32,
            latency_ms,
            bitrate_bps,
            rate_control: true,
            overhead_percent: overhead_percent.clamp(5, 100) as u32,
            ..SrtUrlOptions::default()
        })
    }

    /// A UDP port that is free on 127.0.0.1 right now (the child binds it a moment later).
    pub fn free_loopback_udp_port() -> io::Result<u16> {
        crate::udp_channel::free_loopback_udp_port()
    }

    /// Kills the child (waiting up to 3 s for it to be gone), then closes its stdin.
    ///
    /// In that order, always: a child still waiting for its viewer is not reading stdin, and with the C# apps
    /// closing the write end of such a full pipe never returned (WINDOWS-HANDOFF.md). A writer blocked on the
    /// pipe gets out when the reader dies. The pipe is shared with every [`RemuxInput`] handed out, so a
    /// fan-out sink that drops its copy first does not close it early either.
    pub fn dispose(&self) {
        self.process.stop();
        lock(&self.stdin).take();
    }
}

impl Drop for RemuxChild {
    fn drop(&mut self) {
        self.dispose();
    }
}

impl std::fmt::Debug for RemuxChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemuxChild").field("command_line", &self.safe_command_line()).field("has_exited", &self.has_exited()).finish()
    }
}
