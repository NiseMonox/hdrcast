//! A read-only stream that reads from whichever source is attached. When the current source ends (the
//! encoder exited) it waits for the next one (the restarted encoder), so the fan-out and every viewer's
//! session survive an encoder restart. `read` returns 0 only after [`SwitchableStream::close`].

use std::io::{self, Read};
use std::sync::{Arc, Condvar, Mutex};

use crate::util::lock;

type Source = Arc<Mutex<Box<dyn Read + Send>>>;

/// Cloning gives another handle to the same stream: one goes to the reader (the host's fan-out), the other
/// stays with whoever attaches the sources and closes the stream in the end.
#[derive(Clone)]
pub struct SwitchableStream {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    current: Option<Source>,
    closed: bool,
}

impl SwitchableStream {
    pub fn new() -> SwitchableStream {
        SwitchableStream { inner: Arc::new(Inner { state: Mutex::new(State { current: None, closed: false }), changed: Condvar::new() }) }
    }

    /// Makes `source` what the next read reads from. A read that is blocked waiting for a source wakes up; a
    /// read that is blocked inside the previous source finishes there first (its data, if any, still counts).
    /// The previous source is dropped once no read is using it. Ignored (and `source` dropped) after
    /// [`close`](Self::close).
    pub fn attach(&self, source: Box<dyn Read + Send>) {
        let previous = {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return;
            }
            let previous = state.current.replace(Arc::new(Mutex::new(source)));
            self.inner.changed.notify_all();
            previous
        };
        drop(previous); // outside the lock: dropping a pipe may take a moment
    }

    /// Ends the stream: every read returns 0 from now on (one blocked inside a source does so when that
    /// source lets go of it).
    pub fn close(&self) {
        let previous = {
            let mut state = lock(&self.inner.state);
            state.closed = true;
            self.inner.changed.notify_all();
            state.current.take()
        };
        drop(previous);
    }

    pub fn is_closed(&self) -> bool {
        lock(&self.inner.state).closed
    }
}

impl Default for SwitchableStream {
    fn default() -> Self {
        SwitchableStream::new()
    }
}

impl Read for SwitchableStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buf)
    }
}

impl Read for &SwitchableStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let source = {
                let mut state = lock(&self.inner.state);
                loop {
                    if state.closed {
                        return Ok(0);
                    }
                    if let Some(source) = &state.current {
                        break source.clone();
                    }
                    state = self.inner.changed.wait(state).unwrap_or_else(|e| e.into_inner());
                }
            };
            // Outside the stream's lock, so a new source can be attached while this one blocks.
            let n = loop {
                match lock(&source).read(buf) {
                    Ok(n) => break n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break 0, // a broken source counts as ended
                }
            };
            if n > 0 {
                return Ok(n);
            }
            let ended = {
                let mut state = lock(&self.inner.state);
                // wait for the next source, unless one was attached meanwhile
                if state.current.as_ref().is_some_and(|current| Arc::ptr_eq(current, &source)) {
                    state.current.take()
                } else {
                    None
                }
            };
            drop(ended);
            drop(source);
        }
    }
}

impl std::fmt::Debug for SwitchableStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = lock(&self.inner.state);
        f.debug_struct("SwitchableStream").field("attached", &state.current.is_some()).field("closed", &state.closed).finish()
    }
}
