//! Where settings and logs live: `<settings root>/HdrCast` and `<settings root>/HdrCast/logs`.
//!
//! The settings root is %APPDATA% on Windows (unchanged from the C# apps, so their `group.json` is found and
//! shared), `~/Library/Application Support` on macOS, and `$XDG_CONFIG_HOME` or `~/.config` elsewhere; the
//! environment variable `HDRCAST_DATA_ROOT` overrides it.

use std::path::{Path, PathBuf};

pub const APP_FOLDER_NAME: &str = "HdrCast";
pub const LOGS_FOLDER_NAME: &str = "logs";

/// Environment variable that moves the settings root somewhere else (a second, separate profile: for trying a
/// build without touching the real group and settings, or for running from a USB stick).
pub const ROOT_OVERRIDE_ENV: &str = "HDRCAST_DATA_ROOT";

/// The per-user settings root of this platform (not created, not checked).
pub fn app_data_root() -> PathBuf {
    match std::env::var_os(ROOT_OVERRIDE_ENV) {
        Some(root) if !root.is_empty() => PathBuf::from(root),
        _ => imp::app_data_root(),
    }
}

pub fn config_dir() -> PathBuf {
    config_dir_under(&app_data_root())
}

pub fn logs_dir() -> PathBuf {
    logs_dir_under(&app_data_root())
}

pub fn config_dir_under(app_data_root: &Path) -> PathBuf {
    app_data_root.join(APP_FOLDER_NAME)
}

pub fn logs_dir_under(app_data_root: &Path) -> PathBuf {
    config_dir_under(app_data_root).join(LOGS_FOLDER_NAME)
}

pub fn settings_file(file_name: &str) -> PathBuf {
    config_dir().join(file_name)
}

/// `logs/<prefix>-yyyyMMdd.log`
pub fn daily_log_file(prefix: &str, day: LocalDate) -> PathBuf {
    logs_dir().join(daily_log_name(prefix, day))
}

pub fn daily_log_name(prefix: &str, day: LocalDate) -> String {
    format!("{prefix}-{:04}{:02}{:02}.log", day.year, day.month, day.day)
}

/// Creates the directory when missing; never fails loudly (returns false on failure).
pub fn ensure_dir(dir: &Path) -> bool {
    std::fs::create_dir_all(dir).is_ok()
}

/// A calendar day in the PC's local time zone (log files roll over at local midnight).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalDate {
    pub year: i32,
    /// 1..=12
    pub month: u32,
    /// 1..=31
    pub day: u32,
}

impl LocalDate {
    pub fn today() -> LocalDate {
        imp::today()
    }
}

/// The settings root of a unix-like system from its environment. Pure, so the macOS rule is tested on the
/// Windows build machine too. An empty or relative `XDG_CONFIG_HOME` is ignored, as the XDG specification says.
#[cfg(any(unix, test))]
fn unix_root(macos: bool, home: Option<PathBuf>, xdg_config_home: Option<PathBuf>) -> PathBuf {
    let home = home.filter(|h| !h.as_os_str().is_empty());
    if macos {
        return home.map(|h| h.join("Library").join("Application Support")).unwrap_or_else(|| PathBuf::from("."));
    }
    if let Some(xdg) = xdg_config_home.filter(|x| x.has_root()) {
        return xdg;
    }
    home.map(|h| h.join(".config")).unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::System::SystemInformation::GetLocalTime;
    use windows::Win32::UI::Shell::{FOLDERID_RoamingAppData, SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY};

    use super::LocalDate;

    pub fn app_data_root() -> PathBuf {
        // The shell's answer first (what the C# apps used), so a redirected or renamed profile is followed and
        // their group.json is found; the environment only as a fallback.
        known_folder()
            .or_else(|| std::env::var_os("APPDATA").filter(|v| !v.is_empty()).map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn known_folder() -> Option<PathBuf> {
        // SAFETY: on success the call returns a zero-terminated UTF-16 string that we must free with
        // CoTaskMemFree, which is done after copying it.
        unsafe {
            let raw = SHGetKnownFolderPath(&FOLDERID_RoamingAppData, KF_FLAG_DONT_VERIFY, None).ok()?;
            let path = PathBuf::from(OsString::from_wide(raw.as_wide()));
            CoTaskMemFree(Some(raw.as_ptr() as *const core::ffi::c_void));
            (!path.as_os_str().is_empty()).then_some(path)
        }
    }

    pub fn today() -> LocalDate {
        // SAFETY: GetLocalTime has no preconditions.
        let now = unsafe { GetLocalTime() };
        LocalDate { year: i32::from(now.wYear), month: u32::from(now.wMonth), day: u32::from(now.wDay) }
    }
}

#[cfg(unix)]
mod imp {
    use std::path::PathBuf;

    use super::LocalDate;

    pub fn app_data_root() -> PathBuf {
        super::unix_root(
            cfg!(target_os = "macos"),
            std::env::var_os("HOME").map(PathBuf::from),
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        )
    }

    pub fn today() -> LocalDate {
        // SAFETY: `time` accepts a null pointer; `localtime_r` writes only into `tm`, which is a valid, zeroed
        // struct of the right type.
        unsafe {
            let now = libc::time(std::ptr::null_mut());
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&now, &mut tm);
            LocalDate { year: tm.tm_year + 1900, month: (tm.tm_mon + 1) as u32, day: tm.tm_mday as u32 }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folders_hang_under_the_settings_root() {
        let root = Path::new("root");
        assert_eq!(config_dir_under(root), Path::new("root").join("HdrCast"));
        assert_eq!(logs_dir_under(root), Path::new("root").join("HdrCast").join("logs"));
        assert_eq!(config_dir(), app_data_root().join("HdrCast"));
        assert_eq!(logs_dir(), config_dir().join("logs"));
        assert_eq!(settings_file("group.json"), config_dir().join("group.json"));
        assert!(app_data_root().is_absolute(), "{:?}", app_data_root());
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_the_root_is_the_roaming_appdata_the_csharp_apps_use() {
        // %APPDATA% and the shell's known folder agree on an ordinary account.
        let from_env = PathBuf::from(std::env::var_os("APPDATA").expect("APPDATA is set"));
        assert_eq!(app_data_root(), from_env);
    }

    #[test]
    fn unix_roots_follow_the_platform_conventions() {
        let home = || Some(PathBuf::from("/Users/me"));
        assert_eq!(unix_root(true, home(), None), Path::new("/Users/me/Library/Application Support"));
        // macOS has no XDG convention: the variable is not consulted
        assert_eq!(unix_root(true, home(), Some("/x".into())), Path::new("/Users/me/Library/Application Support"));
        assert_eq!(unix_root(false, home(), None), Path::new("/Users/me/.config"));
        assert_eq!(unix_root(false, home(), Some("/xdg".into())), Path::new("/xdg"));
        assert_eq!(unix_root(false, home(), Some("".into())), Path::new("/Users/me/.config"));
        assert_eq!(unix_root(false, home(), Some("relative".into())), Path::new("/Users/me/.config"));
        assert_eq!(unix_root(false, None, None), Path::new("."));
        assert_eq!(unix_root(true, Some("".into()), None), Path::new("."));
    }

    #[test]
    fn log_files_are_named_by_day() {
        let day = LocalDate { year: 2026, month: 3, day: 7 };
        assert_eq!(daily_log_name("sender", day), "sender-20260307.log");
        assert_eq!(daily_log_file("receiver", day), logs_dir().join("receiver-20260307.log"));
        let today = LocalDate::today();
        assert!(today.year >= 2025 && (1..=12).contains(&today.month) && (1..=31).contains(&today.day), "{today:?}");
    }

    #[test]
    fn ensure_dir_creates_nested_folders_and_reports_failure() {
        let base = std::env::temp_dir().join(format!("hdrcast-paths-{}", std::process::id()));
        let nested = base.join("a").join("b");
        assert!(ensure_dir(&nested));
        assert!(ensure_dir(&nested), "already there is fine");
        let file = base.join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(!ensure_dir(&file.join("below-a-file")));
        std::fs::remove_dir_all(&base).unwrap();
    }
}
