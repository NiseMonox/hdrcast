//! High-resolution waitable timer (Windows 10 1803+) with `timeBeginPeriod(1)` held for its lifetime.

use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
};

use super::util::Handle;

pub(crate) struct PreciseSleep {
    timer: Option<Handle>,
    period_set: bool,
}

impl PreciseSleep {
    pub(crate) fn new() -> Self {
        // SAFETY: plain call; balanced by timeEndPeriod(1) in Drop when it succeeded (TIMERR_NOERROR = 0).
        let period_set = unsafe { timeBeginPeriod(1) } == 0;
        // SAFETY: no attributes, no name; on success the returned handle is ours to close.
        let timer = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
            .ok()
            .map(|handle| Handle::from_raw(handle))
        };
        Self { timer, period_set }
    }

    /// False on a Windows without high-resolution timers: the caller then sleeps coarsely and spins.
    pub(crate) fn high_resolution(&self) -> bool {
        self.timer.is_some()
    }

    pub(crate) fn sleep(&mut self, duration: Duration) {
        let Some(timer) = &self.timer else {
            std::thread::sleep(duration);
            return;
        };
        let due_100ns = -((duration.as_nanos() / 100) as i64); // negative = relative
        if due_100ns >= 0 {
            return;
        }
        // SAFETY: `timer` is a live waitable-timer handle owned by `self`; `due_100ns` outlives the call.
        unsafe {
            if SetWaitableTimer(timer.raw(), &due_100ns, 0, None, None, false).is_ok() {
                WaitForSingleObject(timer.raw(), duration.as_millis() as u32 + 2);
            }
        }
    }
}

impl Drop for PreciseSleep {
    fn drop(&mut self) {
        if self.period_set {
            // SAFETY: balances the successful timeBeginPeriod(1) in `new`.
            unsafe { timeEndPeriod(1) };
        }
    }
}
