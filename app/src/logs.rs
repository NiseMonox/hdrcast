//! The one log: every line goes to the daily file in the logs folder and to the Activity page.
//! Everything is masked on the way in (passphrases, invites), because the log is what people send each other
//! when something breaks.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;

use hdrcast_core::common::{app_paths, secret_redactor};

/// Lines the Activity page starts with when the window opens later than the log did.
const HISTORY: usize = 1500;

#[derive(Default)]
pub struct Logs {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    history: VecDeque<String>,
    pending: Vec<String>,
}

impl Logs {
    pub fn write(&self, line: &str) {
        let line = secret_redactor::redact(line);
        append_to_file(&line);
        let mut inner = self.inner.lock().unwrap();
        if inner.history.len() >= HISTORY {
            inner.history.pop_front();
        }
        inner.history.push_back(line.clone());
        if inner.pending.len() < HISTORY {
            inner.pending.push(line);
        }
    }

    pub fn tagged(&self, tag: &str, line: &str) {
        self.write(&format!("[{tag}] {line}"));
    }

    /// Lines written since the last call (for the `log` event).
    pub fn take_pending(&self) -> Vec<String> {
        std::mem::take(&mut self.inner.lock().unwrap().pending)
    }

    pub fn history(&self) -> Vec<String> {
        self.inner.lock().unwrap().history.iter().cloned().collect()
    }
}

/// Appends a timestamped line to `<logs>/hdrcast-yyyyMMdd.log`. Never fails: logging must not take the app down.
fn append_to_file(line: &str) {
    static GATE: Mutex<()> = Mutex::new(());
    let _guard = GATE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = app_paths::logs_dir();
    if !app_paths::ensure_dir(&dir) {
        return;
    }
    let now = chrono::Local::now();
    let path = app_paths::daily_log_file("hdrcast", app_paths::LocalDate::today());
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {}", now.format("%H:%M:%S%.3f"), line);
    }
}
