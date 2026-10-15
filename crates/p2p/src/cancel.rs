//! What the C# code used a `CancellationToken` for: a flag that is set once, a wait that it cuts short, and a
//! way for a blocked operation to be told the moment it is set.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::util::lock;

type Waker = Arc<dyn Fn() + Send + Sync>;

/// Cloning gives another handle to the same flag.
#[derive(Clone)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    cond: Condvar,
}

struct State {
    cancelled: bool,
    next_id: u64,
    wakers: Vec<(u64, Waker)>,
}

impl CancelToken {
    pub fn new() -> CancelToken {
        CancelToken {
            inner: Arc::new(Inner {
                state: Mutex::new(State { cancelled: false, next_id: 0, wakers: Vec::new() }),
                cond: Condvar::new(),
            }),
        }
    }

    /// A token nobody cancels (`CancellationToken.None`).
    pub fn never() -> CancelToken {
        CancelToken::new()
    }

    /// Sets the flag (once) and ends every [`wait`](Self::wait) and every signaling call that was given this token.
    pub fn cancel(&self) {
        let wakers = {
            let mut state = lock(&self.inner.state);
            if state.cancelled {
                return;
            }
            state.cancelled = true;
            self.inner.cond.notify_all();
            std::mem::take(&mut state.wakers)
        };
        for (_, waker) in wakers {
            waker();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        lock(&self.inner.state).cancelled
    }

    /// Sleeps for `timeout`, or less when cancelled meanwhile. True = cancelled (as `WaitHandle.WaitOne`).
    pub fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.inner.state);
        while !state.cancelled {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self.inner.cond.wait_timeout(state, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        true
    }

    pub fn wait_ms(&self, milliseconds: u64) -> bool {
        self.wait(Duration::from_millis(milliseconds))
    }

    /// Calls `waker` when the token is cancelled (at once when it already is), until the registration is
    /// dropped. The waker runs on the cancelling thread and must not block.
    pub(crate) fn on_cancel(&self, waker: impl Fn() + Send + Sync + 'static) -> CancelRegistration {
        let waker: Waker = Arc::new(waker);
        {
            let mut state = lock(&self.inner.state);
            if !state.cancelled {
                let id = state.next_id;
                state.next_id += 1;
                state.wakers.push((id, waker));
                return CancelRegistration { inner: Some((self.inner.clone(), id)) };
            }
        }
        waker();
        CancelRegistration { inner: None }
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        CancelToken::new()
    }
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelToken").field("cancelled", &self.is_cancelled()).finish()
    }
}

/// Keeps an [`on_cancel`](CancelToken::on_cancel) waker registered for as long as it lives.
pub(crate) struct CancelRegistration {
    inner: Option<(Arc<Inner>, u64)>,
}

impl Drop for CancelRegistration {
    fn drop(&mut self) {
        if let Some((inner, id)) = self.inner.take() {
            lock(&inner.state).wakers.retain(|(other, _)| *other != id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    #[test]
    fn a_wait_runs_its_time_unless_cancelled() {
        let token = CancelToken::new();
        let started = Instant::now();
        assert!(!token.wait_ms(60));
        assert!(started.elapsed() >= Duration::from_millis(55));
        assert!(!token.is_cancelled());

        let other = token.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            other.cancel();
        });
        let started = Instant::now();
        assert!(token.wait(Duration::from_secs(30)));
        assert!(started.elapsed() < Duration::from_secs(5));
        canceller.join().unwrap();
        assert!(token.is_cancelled());
        assert!(token.wait(Duration::from_secs(30)), "a cancelled token never waits again");
    }

    #[test]
    fn wakers_are_told_once_and_only_while_registered() {
        let token = CancelToken::new();
        let count = Arc::new(AtomicUsize::new(0));
        let add = |count: &Arc<AtomicUsize>| {
            let count = count.clone();
            move || {
                count.fetch_add(1, Ordering::SeqCst);
            }
        };
        let kept = token.on_cancel(add(&count));
        drop(token.on_cancel(add(&count))); // unregistered again
        token.cancel();
        token.cancel();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop(kept);

        // registering on a cancelled token calls back at once
        let _late = token.on_cancel(add(&count));
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}
