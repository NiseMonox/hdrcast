//! Shared by the integration tests that run the real ffmpeg / ffprobe / mpv.
//!
//! The tools are looked for in the directory named by the environment variable `HDRCAST_TOOLS`, then on PATH.
//! A test whose tool is missing passes without testing anything, so it says so with a note that is printed
//! even when the test passes: a silently skipped test looks exactly like a passing one, and on the Windows
//! build machine the tools are not on PATH.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use hdrcast_core::common::tool_locator;

/// First existing `tool` ("ffmpeg", "ffprobe", "mpv") in `HDRCAST_TOOLS`, then on PATH.
pub fn find_tool(tool: &str) -> Option<PathBuf> {
    let exe = tool_locator::exe_name(tool);
    let tools_dir = std::env::var_os("HDRCAST_TOOLS")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from);
    let path_dirs =
        std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect::<Vec<_>>());
    tools_dir
        .into_iter()
        .chain(path_dirs.unwrap_or_default())
        .map(|dir| dir.join(&exe))
        .find(|candidate| candidate.is_file())
}

/// [`find_tool`], with a note when the tool is missing.
pub fn tool_or_skip(tool: &str, test: &str) -> Option<PathBuf> {
    let found = find_tool(tool);
    if found.is_none() {
        note(&format!(
            "SKIPPED {test}: no {tool} in HDRCAST_TOOLS or on PATH"
        ));
    }
    found
}

/// A line on the real stderr, which the test harness does not capture.
pub fn note(text: &str) {
    let _ = writeln!(std::io::stderr().lock(), "[hdrcast-media] {text}");
}

/// Runs `exe` to its end; returns stdout followed by stderr, and the exit code.
pub fn run<I, S>(exe: &Path, args: I) -> (String, i32)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("{} could not be started: {e}", exe.display()));
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (text, output.status.code().unwrap_or(-1))
}

/// True when this ffmpeg build names `encoder` in `-encoders`.
pub fn has_encoder(ffmpeg: &Path, encoder: &str) -> bool {
    let (output, _) = run(ffmpeg, ["-hide_banner", "-encoders"]);
    output.contains(&format!(" {encoder} "))
}

/// The last lines of a tool's output, for a failure message.
pub fn tail(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    lines[lines.len().saturating_sub(8)..].join("\n")
}

/// True when ffmpeg refused the command line itself: an option name it does not know, or a value it cannot
/// parse. ffmpeg checks both before it opens the encoder, so they show even on a PC without that GPU;
/// anything else that fails a hardware encoder ("Could not open encoder", a missing driver DLL) only means the
/// hardware is not there.
pub fn is_option_error(output: &str) -> bool {
    [
        "Unrecognized option",
        "Option not found",
        "Error setting option",
        "Error applying encoder options",
        "Unable to parse option value",
        "Undefined constant",
        "Error parsing",
        "Invalid option",
        "Unknown encoder",
    ]
    .iter()
    .any(|needle| output.contains(needle))
}

/// A directory of its own under the system temp directory, removed when dropped.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(name: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!("hdrcast-media-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp directory");
        TempDir(dir)
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
