//! Sleeps until an absolute deadline on [`crate::clock`] with sub-millisecond accuracy.

use std::time::Duration;

use crate::clock::{now_ticks, TICKS_PER_MS};
use crate::platform::PreciseSleep;

/// A high-resolution waitable timer where the platform has one (Windows 10 1803+), otherwise a coarse sleep;
/// the last stretch is always a spin. The platform part keeps the system timer at 1 ms for the lifetime of
/// the object so the fallback and the pipe waits stay tight.
pub struct HighResTimer {
    sleeper: PreciseSleep,
}

impl HighResTimer {
    pub fn new() -> Self {
        Self {
            sleeper: PreciseSleep::new(),
        }
    }

    pub fn high_resolution(&self) -> bool {
        self.sleeper.high_resolution()
    }

    /// Blocks until `now_ticks() >= deadline_ticks` (returns immediately if already past).
    pub fn wait_until(&mut self, deadline_ticks: i64) {
        let remaining = deadline_ticks - now_ticks();
        if remaining <= 0 {
            return;
        }

        if self.sleeper.high_resolution() {
            // Leave the last ~0.2 ms to a spin so we do not overshoot even if the timer is slightly late.
            let spin_ticks = TICKS_PER_MS / 5;
            let timer_ticks = remaining - spin_ticks;
            if timer_ticks > 0 {
                self.sleeper.sleep(Duration::from_nanos(timer_ticks as u64));
            }
        } else {
            let ms = remaining / TICKS_PER_MS;
            if ms > 2 {
                std::thread::sleep(Duration::from_millis((ms - 1) as u64));
            }
        }

        let mut spins = 0u32;
        while now_ticks() < deadline_ticks {
            spins += 1;
            if spins % 64 == 0 {
                std::thread::yield_now();
            } else {
                std::hint::spin_loop();
            }
        }
    }
}

impl Default for HighResTimer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TICKS_PER_SECOND;
    use crate::pacer::FramePacer;

    #[test]
    fn waits_for_an_absolute_deadline_and_not_a_tick_longer_than_needed() {
        let mut timer = HighResTimer::new();
        let start = now_ticks();
        let deadline = start + 20 * TICKS_PER_MS;
        timer.wait_until(deadline);
        let after = now_ticks();
        assert!(
            after >= deadline,
            "returned {} ticks early",
            deadline - after
        );
        assert!(
            after - deadline < 15 * TICKS_PER_MS,
            "overslept by {} us",
            (after - deadline) / 1000
        );
        // A deadline in the past returns at once.
        let before = now_ticks();
        timer.wait_until(before - TICKS_PER_SECOND);
        assert!(now_ticks() - before < 5 * TICKS_PER_MS);
    }

    /// The v0.2.0 capture loop gave the pacer a clock that counted from zero while the timer compared with
    /// absolute timestamps: every deadline looked long past, nothing ever waited and the loop ran at the
    /// desktop's refresh rate. With one time base a pacer started "now" really makes the loop wait a frame.
    #[test]
    fn pacer_deadlines_and_the_timer_share_one_time_base() {
        let mut timer = HighResTimer::new();
        let mut pacer = FramePacer::new(50, TICKS_PER_SECOND);
        let start = now_ticks();
        pacer.start(start);
        assert_eq!(pacer.frames_due(now_ticks()), 1);
        pacer.consume(1);
        // The next deadline is an absolute tick in the future, one frame after the start.
        assert_eq!(pacer.next_deadline_ticks(), start + 20 * TICKS_PER_MS);
        assert!(pacer.ticks_until_next(now_ticks()) > 0);

        let mut emitted = 1;
        while emitted < 6 {
            timer.wait_until(pacer.next_deadline_ticks());
            let due = pacer.frames_due(now_ticks());
            assert!(due >= 1, "the timer returned before the deadline");
            pacer.consume(due);
            emitted += due;
        }
        let elapsed = now_ticks() - start;
        // Five frame intervals at 50 fps are 100 ms; a loop that does not wait would be done in microseconds.
        assert!(
            elapsed >= 100 * TICKS_PER_MS,
            "the loop only took {} us",
            elapsed / 1000
        );
        assert!(
            elapsed < 400 * TICKS_PER_MS,
            "the loop took {} ms",
            elapsed / TICKS_PER_MS
        );
    }
}
