//! What the tests of this module share: an injector that only writes down what it was asked to do.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::inject::Injector;
use crate::keys::{Button, Key};

#[derive(Clone, Default)]
pub(crate) struct Recorder(Arc<Mutex<Vec<String>>>);

impl Recorder {
    pub(crate) fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    fn push(&self, line: String) {
        self.0.lock().unwrap().push(line);
    }

    /// The lines so far, after waiting (up to two seconds) for there to be `count` of them: the other end of a pipe
    /// takes a moment.
    pub(crate) fn wait_for(&self, count: usize) -> Vec<String> {
        wait_until(|| self.lines().len() >= count);
        self.lines()
    }
}

/// Waits (up to three seconds) for `done`; whether it came true.
pub(crate) fn wait_until(done: impl Fn() -> bool) -> bool {
    for _ in 0..300 {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    done()
}

impl Injector for Recorder {
    fn pointer(&mut self, x: u16, y: u16) {
        self.push(format!("move {x},{y}"));
    }
    fn button(&mut self, button: Button, down: bool) {
        self.push(format!("{button:?} {}", if down { "down" } else { "up" }));
    }
    fn wheel(&mut self, axis: u8, delta: i16) {
        self.push(format!("wheel {axis} {delta}"));
    }
    fn modifier(&mut self, bit: u8, down: bool) {
        self.push(format!("mod{bit} {}", if down { "down" } else { "up" }));
    }
    fn key(&mut self, key: Key, down: bool) {
        self.push(format!("{key:?} {}", if down { "down" } else { "up" }));
    }
}
