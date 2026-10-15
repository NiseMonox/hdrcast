//! Helpers shared by the integration tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Finds `ffmpeg` / `ffprobe`: in the directory named by the environment variable `HDRCAST_TOOLS`, then on
/// PATH. `None` (after printing a note) when it is not installed - the caller skips its check then.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let tools = std::env::var_os("HDRCAST_TOOLS").map(PathBuf::from);
    let path_dirs =
        std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect::<Vec<_>>());
    let found = tools
        .into_iter()
        .chain(path_dirs.into_iter().flatten())
        .map(|dir| dir.join(&file))
        .find(|candidate| candidate.is_file());
    if found.is_none() {
        eprintln!("note: {name} not found (set HDRCAST_TOOLS or put it on PATH); skipping the check that needs it");
    }
    found
}

/// Runs a tool to its end; returns (exit code, stdout, stderr).
pub fn run(exe: &Path, args: &[&str]) -> (i32, Vec<u8>, String) {
    let output = Command::new(exe)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", exe.display()));
    (
        output.status.code().unwrap_or(-1),
        output.stdout,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A fresh directory under the temp directory, removed when dropped.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let unique = format!(
            "hdrcast-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).expect("temp directory");
        Self(dir)
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
