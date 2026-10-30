//! HdrCast capture engine (design.md sections 2.1 - 2.4, 2.7).
//!
//! A child process of the app (same executable, `capture` verb) grabs one desktop, converts every frame on
//! the GPU to raw P010 (HDR10) or NV12 (SDR) and serves a constant-rate stream through a named pipe that
//! ffmpeg reads as rawvideo; desktop audio goes through a second pipe as f32le 48 kHz stereo. On stdout, one
//! JSON line first says which frames go into the video pipe ([`FrameLayout`]), then statistics follow once per
//! second; logs go to stderr.
//!
//! Layout of the crate:
//! * platform-neutral model and logic: [`args`], [`stats`], [`output`], [`pacer`], [`aligner`], [`color`],
//!   [`shader`], [`app_audio`], [`resample`], [`clock`], [`timer`], [`frame`], [`audio`], and the engine loop
//!   itself in [`engine`];
//! * the platform seam in [`platform`] (traits + the free functions below);
//! * the Windows backend under `win/`, the macOS backend (ScreenCaptureKit) under `mac/` with its POSIX
//!   parts in `unix/`, and a stub for everything else.
//!
//! The app's `main` starts with [`cli_main`]; to launch the engine it builds a [`CaptureArgs`], spawns itself
//! with `capture` + [`CaptureArgs::to_argv`], reads the frame layout from the child's stdout
//! ([`FrameLayout::try_parse`]), waits for both pipes with [`pipe_listening`], starts ffmpeg on [`pipe_path`]
//! for that layout and parses the rest of the child's stdout with [`CaptureStats::try_parse`].

pub mod aligner;
pub mod app_audio;
pub mod args;
pub mod audio;
pub mod cli;
pub mod clock;
pub mod color;
pub mod encode;
pub mod engine;
pub mod frame;
pub mod log;
mod numfmt;
pub mod output;
pub mod pacer;
pub mod platform;
pub mod playback;
// Used by the macOS backend only, but compiled (and tested) everywhere.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod repack;
pub mod resample;
pub mod shader;
pub mod stats;
pub mod timer;

#[cfg(windows)]
mod win;

#[cfg(target_os = "macos")]
mod mac;

// The POSIX parts of a backend (FIFOs, signals, ...); macOS is their only user so far.
#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code, unused_imports))]
mod unix;

#[cfg(not(any(windows, target_os = "macos")))]
mod stub;

pub use aligner::{AudioAlignment, AudioClockAligner};
pub use app_audio::ProcessEntry;
pub use args::{quote_argv, CaptureArgs, CaptureArgsError, CaptureFormat, EncodeArgs, EngineEncoder, Multipass};
pub use encode::{frames::FrameRecord, EncoderEvent};
pub use cli::cli_main;
pub use engine::{exit_codes, CaptureEngine, StopHandle};
pub use output::OutputInfo;
pub use pacer::{EagerPacer, FramePacer};
pub use platform::{
    enumerate_outputs, list_audio_apps, list_audio_devices, pipe_listening, pipe_path,
    AudioDeviceInfo, FrameLayout,
};
pub use stats::CaptureStats;
