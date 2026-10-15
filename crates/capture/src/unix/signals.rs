//! Signals: SIGINT / SIGTERM / SIGHUP stop the engine cleanly, SIGPIPE is ignored.

use std::mem::MaybeUninit;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, Once, PoisonError};

use crate::engine::StopHandle;

/// Ctrl+C, `kill` (what the app sends to stop the engine) and the terminal going away.
const STOP_SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// The engine the signal handler stops. A raw pointer and not a lock: a handler may interrupt any thread
/// at any instruction, including one that holds a lock.
static STOP: AtomicPtr<StopHandle> = AtomicPtr::new(ptr::null_mut());

/// What the stop signals did before [`install_stop_handler`]; only touched outside of signal handlers.
static PREVIOUS: Mutex<Vec<(libc::c_int, libc::sigaction)>> = Mutex::new(Vec::new());

/// Async-signal-safe: one atomic load, then [`StopHandle::request_stop`], which is one atomic store
/// through a pointer. No allocation, no lock, no I/O.
extern "C" fn on_stop_signal(_signal: libc::c_int) {
    let stop = STOP.load(Ordering::SeqCst);
    if !stop.is_null() {
        // SAFETY: a non-null pointer in STOP comes from Box::into_raw in `install_stop_handler` and is
        // never freed (see `remove_stop_handler`), so it is valid whenever a handler runs.
        unsafe { (*stop).request_stop() };
    }
}

/// Routes the stop signals to `stop` until [`remove_stop_handler`].
pub(crate) fn install_stop_handler(stop: StopHandle) {
    // Deliberately leaked (a pointer and a reference count, once per session): a handler that started on
    // another thread just before the removal may still be reading it.
    STOP.store(Box::into_raw(Box::new(stop)), Ordering::SeqCst);

    let mut previous = PREVIOUS.lock().unwrap_or_else(PoisonError::into_inner);
    if !previous.is_empty() {
        return; // already installed: only the target changed
    }
    for signal in STOP_SIGNALS {
        let mut action = MaybeUninit::<libc::sigaction>::zeroed();
        let mut old = MaybeUninit::<libc::sigaction>::zeroed();
        // SAFETY: an all-zero sigaction is a valid value to start from; the pointers are valid for the
        // calls. The handler has the one-argument form (no SA_SIGINFO) and only does async-signal-safe
        // work. SA_RESTART keeps interrupted system calls of other threads from failing with EINTR.
        let installed = unsafe {
            let action_ptr = action.as_mut_ptr();
            (*action_ptr).sa_sigaction = on_stop_signal as extern "C" fn(libc::c_int) as usize;
            (*action_ptr).sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut (*action_ptr).sa_mask);
            libc::sigaction(signal, action_ptr, old.as_mut_ptr()) == 0
        };
        if installed {
            // SAFETY: sigaction succeeded and stored the previous action.
            previous.push((signal, unsafe { old.assume_init() }));
        }
    }
}

/// Gives the stop signals back what they did before.
pub(crate) fn remove_stop_handler() {
    let mut previous = PREVIOUS.lock().unwrap_or_else(PoisonError::into_inner);
    for (signal, old) in previous.drain(..) {
        // SAFETY: `old` is the action sigaction returned for this signal.
        unsafe { libc::sigaction(signal, &old, ptr::null_mut()) };
    }
    // The StopHandle itself stays allocated, see `install_stop_handler`.
    STOP.store(ptr::null_mut(), Ordering::SeqCst);
}

/// Makes a write into a pipe whose reader went away fail with EPIPE instead of ending the process.
///
/// Rust programs start this way already; this makes it true for whatever process hosts the engine.
pub(crate) fn ignore_sigpipe() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: SIG_IGN is a valid disposition for SIGPIPE; no handler function is involved.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test for the whole life cycle: the handler and its target are process-wide.
    #[test]
    fn a_stop_signal_reaches_the_engine_and_the_old_action_comes_back() {
        let stop = StopHandle::new();
        install_stop_handler(stop.clone());
        assert!(!stop.stop_requested());
        // SAFETY: raises a signal this process handles (the handler was installed just above).
        unsafe { libc::raise(libc::SIGTERM) };
        assert!(stop.stop_requested());

        // A second engine in the same process replaces the target without stacking handlers.
        let second = StopHandle::new();
        install_stop_handler(second.clone());
        // SAFETY: as above.
        unsafe { libc::raise(libc::SIGHUP) };
        assert!(second.stop_requested());

        remove_stop_handler();
        assert!(PREVIOUS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty());
        assert!(STOP.load(Ordering::SeqCst).is_null());
    }
}
