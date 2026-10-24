//! What HdrCast says to its two external programs, and what it understands of their answers.
//!
//! * [`sender`]: the encoder catalogue and the `ffmpeg` argument list that turns the capture engine's raw
//!   frames into HEVC in MPEG-TS (docs/design.md §2.5), the `-progress` parser, the encoder probe and
//!   `sender.json`.
//! * [`receiver`]: the `mpv` argument list for a viewer (§3.1), mpv's JSON IPC messages, the classification of
//!   mpv's last log lines and `receiver.json`.
//! * [`keyframe`]: keyframes on request (§8.12), the datagrams a viewer and its host exchange about them and
//!   when the host's encoder makes one.
//!
//! A port of the C# `shared/Sender` and `shared/Receiver` (0.2.4). On Windows the generated argument lists are
//! the ones 0.2.4 generated, option for option: many of them were tuned against real ffmpeg and mpv builds, and
//! the two settings files are still the ones the C# apps wrote. What is new is selected by a [`Platform`]
//! parameter, so the macOS choices are checked by the tests on the Windows build machine too.
//!
//! Nothing here starts a long-lived process: the app owns the children. Only the encoder probe runs ffmpeg,
//! for a second at a time.

#[macro_use]
mod int_enum;
mod numfmt;
mod settings_file;
mod text;

pub mod keyframe;
pub mod receiver;
pub mod sender;

/// Which operating system's conventions apply (encoders offered, mpv's GPU API and hardware decoder names).
pub use hdrcast_core::common::tool_locator::Platform;
