//! Timestamped log lines on stderr (design.md section 2.1: stdout is reserved for the JSON stats).

use std::io::Write;

pub fn info(message: impl AsRef<str>) {
    write('I', message.as_ref());
}

pub fn warn(message: impl AsRef<str>) {
    write('W', message.as_ref());
}

pub fn error(message: impl AsRef<str>) {
    write('E', message.as_ref());
}

fn write(level: char, message: &str) {
    let (h, m, s, ms) = crate::platform::local_time_of_day();
    // One write per line so lines from different threads do not interleave; a closed stderr is nobody's
    // problem (nothing we can do).
    let line = format!("{h:02}:{m:02}:{s:02}.{ms:03} [{level}] {message}\n");
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

/// Writes one line to stdout and flushes it; a closed stdout (the parent went away) is ignored.
pub(crate) fn stdout_line(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Writes text to stdout as is.
pub(crate) fn stdout_text(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// Writes one line to stderr without the timestamp (argument errors, usage).
pub(crate) fn stderr_text(text: &str) {
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
}
