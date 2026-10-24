//! Turns [`ReceiverSettings`] into the mpv argument list of docs/design.md §3.1.

use std::path::{Path, PathBuf};

use hdrcast_core::common::arg_quote;
use hdrcast_core::common::srt_url::{self, SrtUrlError};

use super::settings::{DisplayMode, HwDecMode, ReceiverFrontend, ReceiverSettings};
use crate::numfmt;
use crate::text::words;
use crate::Platform;

/// Where Windows keeps named pipes.
pub const PIPE_PREFIX: &str = r"\\.\pipe\";
pub const WINDOW_TITLE: &str = "HdrCast";
pub const DEMUXER_MAX_BYTES: &str = "256MiB";

/// Everything needed to launch the playback pipeline (mpv alone, or ffmpeg | mpv).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MpvLaunchPlan {
    pub mpv_exe: PathBuf,
    pub mpv_args: Vec<String>,
    /// What mpv was told to serve its JSON IPC on (`--input-ipc-server`), in full: a named pipe path on
    /// Windows (`\\.\pipe\hdrcast-mpv-<pid>`), the path of a Unix socket elsewhere.
    pub ipc_endpoint: String,
    pub url: String,
    /// Set for the ffmpeg pipe: ffmpeg's stdout goes through the guard into mpv's stdin.
    pub ffmpeg_exe: Option<PathBuf>,
    pub ffmpeg_args: Option<Vec<String>>,
}

impl MpvLaunchPlan {
    pub fn uses_ffmpeg_pipe(&self) -> bool {
        self.ffmpeg_exe.is_some() && self.ffmpeg_args.is_some()
    }

    pub fn mpv_command_line(&self) -> String {
        arg_quote::command_line(&self.mpv_exe.to_string_lossy(), &self.mpv_args)
    }

    pub fn ffmpeg_command_line(&self) -> Option<String> {
        match (&self.ffmpeg_exe, &self.ffmpeg_args) {
            (Some(exe), Some(args)) => Some(arg_quote::command_line(&exe.to_string_lossy(), args)),
            _ => None,
        }
    }

    /// Copy-and-paste form ("ffmpeg ... | mpv ..." for the fallback). It carries the passphrase: run it
    /// through the secret redactor before it goes into a log.
    pub fn to_command_line(&self) -> String {
        match self.ffmpeg_command_line() {
            Some(ffmpeg) => format!("{ffmpeg} | {}", self.mpv_command_line()),
            None => self.mpv_command_line(),
        }
    }
}

/// Why [`build`] refused its input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MpvPlanError {
    #[error("mpv path required")]
    MpvPathRequired,
    #[error("ffmpeg path required for the pipe frontend")]
    FfmpegPathRequired,
    #[error(transparent)]
    Url(#[from] SrtUrlError),
}

/// Name of the IPC pipe of the app instance with process id `pid` (one pipe per instance).
pub fn pipe_name(pid: u32) -> String {
    format!("hdrcast-mpv-{pid}")
}

/// The IPC endpoint the app normally passes to [`build`] on the platform this was compiled for:
/// `\\.\pipe\hdrcast-mpv-<pid>` on Windows, `<temp dir>/hdrcast-mpv-<pid>.sock` elsewhere.
pub fn default_ipc_endpoint(pid: u32) -> String {
    if cfg!(windows) {
        format!("{PIPE_PREFIX}{}", pipe_name(pid))
    } else {
        std::env::temp_dir()
            .join(format!("{}.sock", pipe_name(pid)))
            .to_string_lossy()
            .into_owned()
    }
}

pub fn build_url(
    s: &ReceiverSettings,
    local_port: u16,
    passphrase: &str,
) -> Result<String, SrtUrlError> {
    srt_url::build(&s.to_srt_options(local_port, passphrase))
}

/// Builds the launch plan for the local end of a P2P session (`local_port` on 127.0.0.1, the group's
/// `passphrase`). `ipc_endpoint` is where mpv serves its JSON IPC (see [`default_ipc_endpoint`]); `ffmpeg_exe`
/// is needed for [`ReceiverFrontend::FfmpegPipe`], and [`ReceiverFrontend::Auto`] uses the pipe when it is given.
pub fn build(
    s: &ReceiverSettings,
    local_port: u16,
    passphrase: &str,
    ipc_endpoint: &str,
    mpv_exe: &Path,
    ffmpeg_exe: Option<&Path>,
    platform: Platform,
) -> Result<MpvLaunchPlan, MpvPlanError> {
    if mpv_exe.as_os_str().is_empty() {
        return Err(MpvPlanError::MpvPathRequired);
    }
    let url = build_url(s, local_port, passphrase)?;

    let ffmpeg_exe = ffmpeg_exe.filter(|exe| !exe.as_os_str().is_empty());
    let pipe = match s.frontend {
        ReceiverFrontend::Auto => ffmpeg_exe,
        ReceiverFrontend::FfmpegPipe => Some(ffmpeg_exe.ok_or(MpvPlanError::FfmpegPathRequired)?),
        ReceiverFrontend::MpvDirect => None,
    };
    if let Some(ffmpeg_exe) = pipe {
        return Ok(MpvLaunchPlan {
            mpv_exe: mpv_exe.to_path_buf(),
            mpv_args: mpv_args(s, ipc_endpoint, "-", true, platform),
            ipc_endpoint: ipc_endpoint.to_string(),
            ffmpeg_exe: Some(ffmpeg_exe.to_path_buf()),
            ffmpeg_args: Some(ffmpeg_pipe_args(&url)),
            url,
        });
    }

    Ok(MpvLaunchPlan {
        mpv_exe: mpv_exe.to_path_buf(),
        mpv_args: mpv_args(s, ipc_endpoint, &url, false, platform),
        ipc_endpoint: ipc_endpoint.to_string(),
        url,
        ffmpeg_exe: None,
        ffmpeg_args: None,
    })
}

/// ffmpeg's side of the pipe: receive SRT and write its payload to stdout as it came (`-f data` in and out:
/// nothing is demuxed or remuxed), so that the guard sees the stream's own continuity counters and timestamps.
pub fn ffmpeg_pipe_args(url: &str) -> Vec<String> {
    let mut a = words("-hide_banner -loglevel warning -nostdin -f data -i");
    a.push(url.to_string());
    a.extend(words("-map 0 -c copy -f data -flush_packets 1 pipe:1"));
    a
}

/// mpv arguments; `input` is the srt:// URL or "-" for stdin.
///
/// Windows gets exactly what 0.2.4 passed. macOS differs in two places only: no `--gpu-api=d3d11` (mpv picks
/// the API of its own build there), and VideoToolbox where Windows names D3D11VA. The HDR logic is the same:
/// gpu-next with `--target-colorspace-hint`, tone mapping and the target peak. The macOS list was checked
/// against the option list of mpv 0.41 but STILL NEEDS A RUN ON A REAL MAC.
pub fn mpv_args(
    s: &ReceiverSettings,
    ipc_endpoint: &str,
    input: &str,
    pipe_input: bool,
    platform: Platform,
) -> Vec<String> {
    let mut a: Vec<String> = Vec::with_capacity(32);
    a.push("--vo=gpu-next".to_string());
    if platform == Platform::Windows {
        a.push("--gpu-api=d3d11".to_string());
    }
    a.push(format!("--hwdec={}", hwdec_value(s.hw_dec, platform)));
    a.push(format!(
        "--target-colorspace-hint={}",
        colorspace_hint_value(s.display_mode)
    ));
    if s.display_mode == DisplayMode::SdrToneMap {
        let algorithm = match s.tone_mapping.trim() {
            "" => ReceiverSettings::DEFAULT_TONE_MAPPING,
            algorithm => algorithm,
        };
        a.push(format!("--tone-mapping={algorithm}"));
    }
    if s.target_peak_nits > 0 {
        a.push(format!("--target-peak={}", s.target_peak_nits));
    }
    a.push("--hdr-compute-peak=auto".to_string());

    let buffer = if s.buffer_seconds > 0.0 {
        s.buffer_seconds
    } else {
        1.5
    };
    let readahead = (buffer * 2.0).max(1.0);
    // --cache-secs overrides --demuxer-readahead-secs whenever the cache is on and its value is larger
    // (default 3600000), so it is the effective read-ahead cap; keep it generous to never throttle the SRT socket.
    let cache_secs = (buffer * 4.0).max(8.0);
    a.push("--cache=yes".to_string());
    a.push(format!("--demuxer-readahead-secs={}", num(readahead)));
    a.push(format!("--cache-secs={}", num(cache_secs)));
    a.push("--cache-pause-initial=yes".to_string());
    a.push(format!("--cache-pause-wait={}", num(buffer)));
    a.push(format!("--demuxer-max-bytes={DEMUXER_MAX_BYTES}"));

    a.push("--video-sync=audio".to_string());
    a.push("--interpolation=no".to_string());
    a.push("--framedrop=vo".to_string());
    a.push("--demuxer-lavf-probesize=5000000".to_string());
    a.push("--demuxer-lavf-analyzeduration=2".to_string());
    if pipe_input {
        a.push("--demuxer-lavf-format=mpegts".to_string());
    }

    a.push(format!("--input-ipc-server={ipc_endpoint}"));
    // The player window shows the picture and, under remote control, passes input on to the host; nothing else.
    // No mpv.conf / input.conf / scripts of the user's, none of mpv's own keys and mouse actions (the wheel's volume,
    // space, q, a double click...), no moving the window by dragging the picture and no file dropped on it in place
    // of the stream. HdrCast binds the few keys the player keeps itself, over IPC (docs/design.md §3.1).
    a.push("--config=no".to_string());
    a.push("--input-default-bindings=no".to_string());
    a.push("--window-dragging=no".to_string());
    a.push("--drag-and-drop=no".to_string());
    a.push("--keep-open=no".to_string());
    a.push("--force-window=yes".to_string());
    // A live stream has nothing to seek in: no on-screen controller (its progress bar and buttons come up over the
    // picture whenever the mouse moves, also while it moves the host's mouse) and no bar for volume or seeking.
    a.push("--osc=no".to_string());
    a.push("--osd-bar=no".to_string());
    if platform == Platform::Windows {
        // No Windows media overlay for a live stream (mpv 0.39+). It also keeps mpv's exit from stalling: with the
        // on-screen controller off, a load failure left mpv 0.41 stuck in the media controls' shutdown every time
        // (12 s until killed; 0.2 s with this), and the app restarts mpv only once it has exited.
        a.push("--media-controls=no".to_string());
    }
    a.push(format!("--title={WINDOW_TITLE}"));
    a.push("--msg-level=all=info".to_string());
    a.push("--term-status-msg=".to_string());
    if pipe_input {
        a.push("--input-terminal=no".to_string());
    }

    if s.fullscreen {
        a.push("--fs".to_string());
        a.push(format!("--fs-screen={}", s.fullscreen_screen.max(0)));
    }

    a.push(input.to_string());
    a
}

/// The value of `--hwdec` (see [`HwDecMode::mpv_value`]).
pub fn hwdec_value(mode: HwDecMode, platform: Platform) -> &'static str {
    mode.mpv_value(platform)
}

pub fn colorspace_hint_value(mode: DisplayMode) -> &'static str {
    match mode {
        DisplayMode::HdrPassthrough => "yes",
        DisplayMode::SdrToneMap => "no",
        DisplayMode::Auto => "auto",
    }
}

/// `v.ToString("0.###")`
fn num(v: f64) -> String {
    numfmt::fixed(v, 0, 3)
}
