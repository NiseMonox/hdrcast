//! The plain POSIX parts of a backend: FIFOs towards ffmpeg, the parent-process watch, the stop signals and
//! the local time of day.
//!
//! Compiled on every unix so that the code and its tests exist wherever such a system is at hand; the macOS
//! backend is its only user today (other unix systems still get the stub). `unsafe` here is libc only; each
//! block states what it relies on.

mod fifo;
mod process;
mod signals;
mod time;

pub(crate) use fifo::{pipe_listening, pipe_path, FifoServer};
pub(crate) use process::PidWatch;
pub(crate) use signals::{ignore_sigpipe, install_stop_handler, remove_stop_handler};
pub(crate) use time::local_time_of_day;
