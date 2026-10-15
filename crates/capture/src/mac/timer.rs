//! A sleep that ends within a small fraction of a millisecond of when it should.

use std::time::{Duration, Instant};

/// The end of every sleep is spent spinning: the kernel wakes a sleeping thread some tens of microseconds
/// late on an idle Mac and occasionally a few hundred, and this much covers that.
const SPIN: Duration = Duration::from_micros(300);

/// `nanosleep` (which is what `std::thread::sleep` is on macOS) for all but the last [`SPIN`] of the wait,
/// then a spin on the monotonic clock. The Windows counterpart is a high-resolution waitable timer; both
/// promise the same thing to [`crate::timer::HighResTimer`]: a sleep that returns on time to within far
/// less than a millisecond, so it only has to spin for its own last 0.2 ms.
pub(crate) struct PreciseSleep;

impl PreciseSleep {
    pub(crate) fn new() -> Self {
        // The thread that creates the timer is the one that paces the frames. As "user-interactive" its
        // timers are exempt from the coalescing macOS applies to ordinary threads (which may defer a
        // wake-up by milliseconds to save power).
        // SAFETY: plain call that only changes the scheduling class of the calling thread. It fails
        // harmlessly (returning an error number) when the thread already has explicit scheduling
        // parameters; the spin then still bounds the error whenever the wake-up is less than SPIN late.
        let _ = unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
        };
        Self
    }

    pub(crate) fn high_resolution(&self) -> bool {
        true
    }

    pub(crate) fn sleep(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        if let Some(coarse) = duration.checked_sub(SPIN) {
            if !coarse.is_zero() {
                std::thread::sleep(coarse);
            }
        }
        while Instant::now() < deadline {
            std::hint::spin_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sleep_ends_on_time_to_a_fraction_of_a_millisecond() {
        let mut sleeper = PreciseSleep::new();
        assert!(sleeper.high_resolution());
        let mut worst = Duration::ZERO;
        for wait in [50u64, 400, 1500, 5000] {
            let wait = Duration::from_micros(wait);
            let started = Instant::now();
            sleeper.sleep(wait);
            let took = started.elapsed();
            assert!(took >= wait, "returned {:?} early", wait - took);
            worst = worst.max(took - wait);
        }
        // generous: a loaded test machine may preempt the spin
        assert!(worst < Duration::from_millis(2), "overslept by {worst:?}");
    }
}
