//! Small things every module here needs: a lock that survives a panicked callback, a thread that can be joined
//! with a time limit, and callback lists.

use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

/// Where log lines go (the app's log file and window). Lines are complete sentences without a line ending.
pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

/// A callback that panicked while a lock was held must not take the networking down with it.
pub(crate) fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// A named background thread that can be joined with a time limit, as the C# did with `Thread.Join(ms)`:
/// a thread that is stuck (in a user callback, in a router that does not answer) is left behind instead of
/// hanging the caller. Joining from the thread itself returns at once.
pub(crate) struct Worker {
    id: ThreadId,
    handle: Mutex<Option<JoinHandle<()>>>,
    done: Arc<(Mutex<bool>, Condvar)>,
}

impl Worker {
    pub fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> io::Result<Worker> {
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let signal = SetOnDrop(done.clone());
        let handle = thread::Builder::new().name(name.to_string()).spawn(move || {
            let _signal = signal; // also when `body` panics
            body();
        })?;
        Ok(Worker { id: handle.thread().id(), handle: Mutex::new(Some(handle)), done })
    }

    pub fn is_current(&self) -> bool {
        thread::current().id() == self.id
    }

    /// True when the thread has ended (within `timeout`); always false from the thread itself.
    pub fn join(&self, timeout: Duration) -> bool {
        if self.is_current() {
            return false;
        }
        let deadline = Instant::now() + timeout;
        let (flag, cond) = &*self.done;
        let mut done = lock(flag);
        while !*done {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            done = cond.wait_timeout(done, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        drop(done);
        // The body has returned; what is left of the thread ends without running any of our code.
        if let Some(handle) = lock(&self.handle).take() {
            let _ = handle.join();
        }
        true
    }
}

struct SetOnDrop(Arc<(Mutex<bool>, Condvar)>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        let (flag, cond) = &*self.0;
        *lock(flag) = true;
        cond.notify_all();
    }
}

/// Subscribers of an event (the C# `event Action`). Handlers are called outside the list's lock, in the order
/// they were added, on whatever thread raises the event.
pub(crate) struct Callbacks<F: ?Sized> {
    list: Mutex<Vec<Arc<F>>>,
}

impl<F: ?Sized> Callbacks<F> {
    pub fn new() -> Callbacks<F> {
        Callbacks { list: Mutex::new(Vec::new()) }
    }

    pub fn add(&self, handler: Arc<F>) {
        lock(&self.list).push(handler);
    }

    pub fn snapshot(&self) -> Vec<Arc<F>> {
        lock(&self.list).clone()
    }
}

impl Callbacks<dyn Fn() + Send + Sync> {
    pub fn raise(&self) {
        for handler in self.snapshot() {
            handler();
        }
    }
}

/// `Math.Round(double)`: to the nearest integer, halves to the even one.
pub(crate) fn round_half_even(value: f64) -> f64 {
    value.round_ties_even()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn a_worker_is_joined_within_the_limit_or_left_behind() {
        let quick = Worker::spawn("quick", || {}).unwrap();
        assert!(quick.join(Duration::from_secs(5)));
        assert!(quick.join(Duration::from_secs(5)), "joining twice is fine");

        let release = Arc::new(AtomicBool::new(false));
        let flag = release.clone();
        let slow = Worker::spawn("slow", move || {
            while !flag.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
        })
        .unwrap();
        let started = Instant::now();
        assert!(!slow.join(Duration::from_millis(50)));
        assert!(started.elapsed() < Duration::from_secs(2));
        release.store(true, Ordering::SeqCst);
        assert!(slow.join(Duration::from_secs(5)));
    }

    #[test]
    fn a_panicking_worker_still_counts_as_ended() {
        let worker = Worker::spawn("panics", || panic!("on purpose")).unwrap();
        assert!(worker.join(Duration::from_secs(5)));
    }

    #[test]
    fn rounding_is_half_to_even_like_dotnet() {
        assert_eq!(round_half_even(0.5), 0.0);
        assert_eq!(round_half_even(1.5), 2.0);
        assert_eq!(round_half_even(2.5), 2.0);
        assert_eq!(round_half_even(37.6), 38.0);
    }
}
