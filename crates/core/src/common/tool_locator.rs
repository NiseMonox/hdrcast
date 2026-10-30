//! Finds an external executable (mpv / ffmpeg / ffprobe): next to our exe, in tools/, then on PATH.
//!
//! macOS adds the places a bundled or Homebrew copy lives: an app started from Finder gets a PATH of
//! /usr/bin:/bin:/usr/sbin:/sbin, so Homebrew's directories have to be probed by name. ffmpeg and ffprobe are
//! looked for in Homebrew's `ffmpeg-full` before PATH: the plain `ffmpeg` formula has no libsrt.

use std::path::{Path, PathBuf};

pub const TOOLS_FOLDER_NAME: &str = "tools";

/// The tools HdrCast starts, without extension; [`exe_name`] turns one into this platform's file name.
pub const FFMPEG: &str = "ffmpeg";
pub const FFPROBE: &str = "ffprobe";
pub const MPV: &str = "mpv";

/// Which operating system's conventions apply. A parameter (rather than `cfg`) so the macOS rules are tested on
/// the Windows build machine too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    MacOs,
    Other,
}

impl Platform {
    pub const CURRENT: Platform = if cfg!(windows) {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::MacOs
    } else {
        Platform::Other
    };

    fn path_separator(self) -> char {
        if self == Platform::Windows {
            ';'
        } else {
            ':'
        }
    }
}

/// "ffmpeg" → "ffmpeg.exe" on Windows, "ffmpeg" elsewhere.
pub fn exe_name(tool: &str) -> String {
    exe_name_on(Platform::CURRENT, tool)
}

pub fn exe_name_on(platform: Platform, tool: &str) -> String {
    if platform == Platform::Windows {
        format!("{tool}.exe")
    } else {
        tool.to_string()
    }
}

/// Directory containing the running executable.
pub fn app_directory() -> PathBuf {
    std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf)).unwrap_or_default()
}

/// First existing candidate for `exe_name` (a file name such as "ffmpeg.exe"; see [`exe_name`]).
pub fn find(exe_name: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH").map(|p| p.to_string_lossy().into_owned());
    find_in(Platform::CURRENT, exe_name, &app_directory(), path_env.as_deref(), |p| p.is_file())
}

/// Testable core: first candidate for which `exists` returns true.
pub fn find_in(
    platform: Platform,
    exe_name: &str,
    app_dir: &Path,
    path_env: Option<&str>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    candidates(platform, exe_name, app_dir, path_env).into_iter().find(|candidate| exists(candidate))
}

/// Candidate paths in probe order.
pub fn candidates(platform: Platform, exe_name: &str, app_dir: &Path, path_env: Option<&str>) -> Vec<PathBuf> {
    let mut list = Vec::new();
    if exe_name.is_empty() {
        return list;
    }
    if !app_dir.as_os_str().is_empty() {
        list.push(app_dir.join(exe_name));
        list.push(app_dir.join(TOOLS_FOLDER_NAME).join(exe_name));
        if platform == Platform::MacOs {
            // HdrCast.app/Contents/MacOS/<exe> → HdrCast.app/Contents/Resources/tools/<tool>
            if let Some(contents) = app_dir.parent() {
                list.push(contents.join("Resources").join(TOOLS_FOLDER_NAME).join(exe_name));
            }
        }
    }
    // Homebrew's ffmpeg formula has no libsrt (nor zscale); ffmpeg-full has both. It is keg-only, so not linked into
    // bin/: probed by name, and ahead of PATH, where a terminal finds the plain ffmpeg first.
    let ffmpeg_tool = Path::new(exe_name).file_stem().is_some_and(|stem| stem == FFMPEG || stem == FFPROBE);
    if platform == Platform::MacOs && ffmpeg_tool {
        list.push(Path::new("/opt/homebrew/opt/ffmpeg-full/bin").join(exe_name)); // Apple silicon Homebrew
        list.push(Path::new("/usr/local/opt/ffmpeg-full/bin").join(exe_name)); // Intel Homebrew
    }
    if let Some(path_env) = path_env {
        for raw in path_env.split(platform.path_separator()) {
            let dir = raw.trim().trim_matches('"');
            if !dir.is_empty() {
                list.push(Path::new(dir).join(exe_name));
            }
        }
    }
    if platform == Platform::MacOs {
        list.push(Path::new("/opt/homebrew/bin").join(exe_name)); // Apple silicon Homebrew
        list.push(Path::new("/usr/local/bin").join(exe_name)); // Intel Homebrew, hand installs
        if Path::new(exe_name).file_stem().is_some_and(|stem| stem == MPV) {
            list.push(PathBuf::from("/Applications/mpv.app/Contents/MacOS/mpv"));
        }
    }
    list
}

/// Uses the configured path when it points to an existing file, otherwise auto-locates `exe_name`.
/// Returns `None` when nothing is found.
pub fn resolve(configured_path: Option<&str>, exe_name: &str) -> Option<PathBuf> {
    if let Some(configured) = configured_path.filter(|p| !p.trim().is_empty()) {
        let p = Path::new(configured.trim().trim_matches('"'));
        if p.is_file() {
            // fall through to auto-detection when the path cannot be made absolute
            if let Ok(full) = std::path::absolute(p) {
                return Some(full);
            }
        }
    }
    find(exe_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_path_wins_when_it_is_a_file() {
        let me = std::env::current_exe().unwrap();
        let quoted = format!("  \"{}\" ", me.display());
        assert_eq!(resolve(Some(&quoted), "hdrcast-no-such-tool"), Some(me.clone()));
        // a directory or a missing file is not a tool: auto-detection takes over (and finds nothing here)
        let dir = me.parent().unwrap().to_string_lossy().into_owned();
        assert_eq!(resolve(Some(&dir), "hdrcast-no-such-tool"), None);
        assert_eq!(resolve(Some("   "), "hdrcast-no-such-tool"), None);
        assert_eq!(resolve(None, "hdrcast-no-such-tool"), None);
    }

    #[test]
    fn the_running_test_binary_is_found_next_to_itself() {
        let me = std::env::current_exe().unwrap();
        let name = me.file_name().unwrap().to_str().unwrap();
        assert_eq!(find(name), Some(me.clone()));
        assert_eq!(app_directory(), me.parent().unwrap());
    }
}
