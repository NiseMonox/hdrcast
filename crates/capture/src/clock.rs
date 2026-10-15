//! The one time base of the engine: monotonic nanosecond ticks since the first use in this process.
//!
//! The frame pacer, the waits, the statistics and the audio alignment all read this clock. They must: the C#
//! engine once fed the pacer "ticks since the session started" while the timer compared against absolute
//! timestamps, so every deadline looked long past, the loop never waited and spun at the desktop's refresh
//! rate instead of the requested frame rate.

use std::sync::OnceLock;
use std::time::Instant;

pub const TICKS_PER_SECOND: i64 = 1_000_000_000;
pub const TICKS_PER_MS: i64 = TICKS_PER_SECOND / 1000;

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Absolute monotonic time in ticks ([`TICKS_PER_SECOND`] per second).
pub fn now_ticks() -> i64 {
    epoch().elapsed().as_nanos() as i64
}

/// Elapsed time of one capture session, started once (when the consumer is connected) and shared by the
/// video loop and the audio pipeline so both streams count from the same instant.
#[derive(Debug, Default)]
pub struct SessionClock {
    start: OnceLock<i64>,
}

impl SessionClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts the clock; later calls change nothing. Returns the tick ([`now_ticks`]) the clock started at.
    pub fn start(&self) -> i64 {
        *self.start.get_or_init(now_ticks)
    }

    pub fn is_running(&self) -> bool {
        self.start.get().is_some()
    }

    /// Ticks since [`start`](Self::start); 0 while the clock has not been started.
    pub fn elapsed_ticks(&self) -> i64 {
        self.start.get().map_or(0, |start| now_ticks() - start)
    }

    pub fn elapsed_seconds(&self) -> f64 {
        self.elapsed_ticks() as f64 / TICKS_PER_SECOND as f64
    }
}
