//! "Start HdrCast when I sign in" (docs/design.md §2.7). The entry belongs to the system, not to a settings file:
//! Windows keeps it under the Run key (where Task Manager's Startup apps can also turn it off), macOS as a
//! LaunchAgent. What the page shows is therefore read from there every time, never remembered.
//!
//! The entry starts HdrCast with [`BACKGROUND_ARG`]: no window, only the icon in the notification area (the menu
//! bar), from where the computer is available to connect.

use std::path::Path;

use serde_json::{json, Value};

/// The option the entry starts HdrCast with.
pub const BACKGROUND_ARG: &str = "--background";

/// Whether this system has such an entry at all.
pub const SUPPORTED: bool = cfg!(any(windows, target_os = "macos"));

/// What the system does at the next sign-in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Autostart {
    /// This copy of HdrCast starts.
    pub on: bool,
    /// The entry starts another copy (say, the folder of an older version): its program.
    pub other: Option<String>,
    /// The entry is there but was turned off elsewhere (Task Manager's Startup apps).
    pub disabled: bool,
}

impl Autostart {
    pub fn view(&self) -> Value {
        json!({ "supported": SUPPORTED, "on": self.on, "other": self.other, "disabled": self.disabled })
    }
}

/// Reads the entry; no entry (or no such thing on this system) reads as off.
pub fn read() -> Autostart {
    imp::read(&crate::state::current_exe())
}

/// Adds the entry for this copy (replacing one for another copy) or removes it, then reads what holds.
pub fn set(on: bool) -> Result<Autostart, String> {
    if !SUPPORTED {
        return Err("Starting at sign-in is not available on this system.".to_string());
    }
    imp::set(&crate::state::current_exe(), on)?;
    Ok(read())
}

/// The command a Run entry holds: the program in quotes (a path with spaces needs them), then the option.
#[cfg_attr(not(windows), allow(dead_code))]
fn command_line(exe: &Path) -> String {
    format!("\"{}\" {BACKGROUND_ARG}", exe.display())
}

/// The program of a command line the way Windows reads it: the quoted part, or without quotes everything up to
/// ".exe" (Parsec writes its entry like that, spaces and all).
#[cfg_attr(not(windows), allow(dead_code))]
fn program_of(command: &str) -> String {
    let command = command.trim();
    if let Some(rest) = command.strip_prefix('"') {
        return rest.split('"').next().unwrap_or_default().to_string();
    }
    match command.to_ascii_lowercase().find(".exe") {
        Some(at) => command[..at + 4].to_string(),
        None => command.split(' ').next().unwrap_or_default().to_string(),
    }
}

/// Paths on Windows compare without regard to case or the kind of slash.
fn same_program(program: &str, exe: &Path) -> bool {
    if cfg!(windows) {
        let norm = |s: &str| s.replace('/', "\\").to_lowercase();
        norm(program) == norm(&exe.to_string_lossy())
    } else {
        Path::new(program) == exe
    }
}

/// Task Manager keeps 12 bytes per entry under StartupApproved: a 4-byte flag (2 or 6 = on, 3 or 7 = turned
/// off) and the time it was turned off. No value at all means on.
#[cfg_attr(not(windows), allow(dead_code))]
fn approved(flag_and_time: &[u8]) -> bool {
    flag_and_time.first().is_none_or(|flag| flag & 1 == 0)
}

// ---- macOS: ~/Library/LaunchAgents/app.hdrcast.login.plist ---------------------------------------------------

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const AGENT_LABEL: &str = "app.hdrcast.login";

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// The LaunchAgent: run once at login, in the user's graphical session only.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn launch_agent(exe: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{AGENT_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{}</string>
		<string>{BACKGROUND_ARG}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
	<key>ProcessType</key>
	<string>Interactive</string>
</dict>
</plist>
"#,
        xml_escape(&exe.to_string_lossy())
    )
}

/// The program a LaunchAgent runs: the first string of its ProgramArguments.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn agent_program(plist: &str) -> Option<String> {
    let after = &plist[plist.find("<key>ProgramArguments</key>")?..];
    let start = after.find("<string>")? + "<string>".len();
    let end = start + after[start..].find("</string>")?;
    Some(xml_unescape(after[start..end].trim()))
}

#[cfg(windows)]
mod imp {
    use std::path::Path;

    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, WIN32_ERROR};
    use windows::Win32::System::Registry::{
        RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_ROUTINE_FLAGS, REG_SZ, RRF_NOEXPAND,
        RRF_RT_REG_BINARY, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    };

    use super::{approved, command_line, program_of, same_program, Autostart};

    const RUN: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
    /// Where Task Manager's Startup apps remembers that an entry was turned off.
    const APPROVED: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run");
    const NAME: PCWSTR = w!("HdrCast");

    fn describe(e: WIN32_ERROR) -> String {
        format!("Could not change the sign-in entry: {}", windows::core::Error::from(e).message())
    }

    /// The value's bytes, or None when there is none.
    fn get(key: PCWSTR, flags: REG_ROUTINE_FLAGS) -> Result<Option<Vec<u8>>, WIN32_ERROR> {
        loop {
            let mut size = 0u32;
            // SAFETY: the key and value names are static NUL-terminated strings; asking for the size only.
            let e = unsafe { RegGetValueW(HKEY_CURRENT_USER, key, NAME, flags, None, None, Some(&mut size)) };
            if e == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            e.ok().map_err(|_| e)?;
            let mut data = vec![0u8; size as usize];
            // SAFETY: `data` has the `size` bytes the call may write, and `size` says so.
            let e = unsafe {
                RegGetValueW(HKEY_CURRENT_USER, key, NAME, flags, None, Some(data.as_mut_ptr().cast()), Some(&mut size))
            };
            match e {
                ERROR_MORE_DATA => continue, // it grew in between
                ERROR_FILE_NOT_FOUND => return Ok(None),
                e if e.is_err() => return Err(e),
                _ => {
                    data.truncate(size as usize);
                    return Ok(Some(data));
                }
            }
        }
    }

    fn get_string(key: PCWSTR) -> Option<String> {
        let bytes = get(key, RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND).ok()??;
        let wide: Vec<u16> = bytes.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        Some(String::from_utf16_lossy(&wide).trim_end_matches('\0').to_string())
    }

    fn delete(key: PCWSTR) -> Result<(), String> {
        // SAFETY: static NUL-terminated names.
        match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key, NAME) } {
            e if e.is_ok() || e == ERROR_FILE_NOT_FOUND => Ok(()),
            e => Err(describe(e)),
        }
    }

    pub fn read(exe: &Path) -> Autostart {
        let Some(command) = get_string(RUN) else { return Autostart::default() };
        let program = program_of(&command);
        if !same_program(&program, exe) {
            return Autostart { on: false, other: Some(program), disabled: false };
        }
        let disabled = get(APPROVED, RRF_RT_REG_BINARY).ok().flatten().is_some_and(|flag| !approved(&flag));
        Autostart { on: !disabled, other: None, disabled }
    }

    pub fn set(exe: &Path, on: bool) -> Result<(), String> {
        // A "turned off" mark Task Manager left would keep the new entry from running; without an entry it means nothing.
        delete(APPROVED)?;
        if !on {
            return delete(RUN);
        }
        let data: Vec<u8> = command_line(exe).encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
        // SAFETY: `data` is a NUL-terminated UTF-16 string of `data.len()` bytes that outlives the call.
        let e = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, RUN, NAME, REG_SZ.0, Some(data.as_ptr().cast()), data.len() as u32) };
        e.ok().map_err(|_| describe(e))
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::{Path, PathBuf};

    use super::{agent_program, launch_agent, same_program, Autostart, AGENT_LABEL};

    fn plist_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
        Some(PathBuf::from(home).join("Library").join("LaunchAgents").join(format!("{AGENT_LABEL}.plist")))
    }

    pub fn read(exe: &Path) -> Autostart {
        let Some(program) = plist_path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|text| agent_program(&text))
        else {
            return Autostart::default();
        };
        if same_program(&program, exe) {
            Autostart { on: true, other: None, disabled: false }
        } else {
            Autostart { on: false, other: Some(program), disabled: false }
        }
    }

    /// Writes or removes the file only. launchd reads it at the next login; loading it now would start a second
    /// HdrCast at once, and unloading it would end the one it started at login.
    pub fn set(exe: &Path, on: bool) -> Result<(), String> {
        let path = plist_path().ok_or("Cannot find the home folder.")?;
        if !on {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("Could not remove {}: {e}", path.display())),
                _ => Ok(()),
            };
        }
        // Opened straight from Downloads, an app from the internet runs from a temporary copy that is gone after a
        // restart: the entry would point at nothing.
        if exe.to_string_lossy().contains("/AppTranslocation/") {
            return Err("Move HdrCast to the Applications folder first: macOS runs this copy from a temporary place.".to_string());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
        }
        std::fs::write(&path, launch_agent(exe)).map_err(|e| format!("Could not write {}: {e}", path.display()))
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use std::path::Path;

    use super::Autostart;

    pub fn read(_exe: &Path) -> Autostart {
        Autostart::default()
    }

    pub fn set(_exe: &Path, _on: bool) -> Result<(), String> {
        Err("Starting at sign-in is not available on this system.".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_run_entry_quotes_the_program_and_reads_back() {
        let exe = Path::new(r"D:\HdrCast 0.3.1\HdrCast.exe");
        let command = command_line(exe);
        assert_eq!(command, r#""D:\HdrCast 0.3.1\HdrCast.exe" --background"#);
        assert_eq!(program_of(&command), r"D:\HdrCast 0.3.1\HdrCast.exe");
        // entries written by other programs, or by hand
        assert_eq!(program_of(r"C:\Program Files\Parsec\parsecd.exe app_silent=1"), r"C:\Program Files\Parsec\parsecd.exe");
        assert_eq!(program_of(r#""E:\steam\steam.exe" -silent"#), r"E:\steam\steam.exe");
        assert_eq!(program_of(r"C:\Tools\HdrCast.EXE"), r"C:\Tools\HdrCast.EXE");
        assert_eq!(program_of("  "), "");
    }

    #[cfg(windows)]
    #[test]
    fn programs_compare_like_windows_paths() {
        let exe = Path::new(r"D:\HdrCast-0.3.1\HdrCast.exe");
        assert!(same_program(r"d:\hdrcast-0.3.1\HDRCAST.exe", exe));
        assert!(same_program("D:/HdrCast-0.3.1/HdrCast.exe", exe));
        assert!(!same_program(r"D:\HdrCast-0.3.0\HdrCast.exe", exe));
    }

    #[test]
    fn task_managers_mark_turns_the_entry_off_only_when_odd() {
        assert!(approved(&[]));
        assert!(approved(&[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(approved(&[6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(!approved(&[3, 0, 0, 0, 0x10, 0x5c, 0x7d, 0x31, 0x8e, 0x41, 0xdb, 0x01]));
        assert!(!approved(&[7, 0, 0, 0]));
    }

    #[test]
    fn the_launch_agent_runs_this_program_in_the_background() {
        let exe = Path::new("/Applications/HdrCast & Co.app/Contents/MacOS/HdrCast");
        let plist = launch_agent(exe);
        assert!(plist.contains("<string>app.hdrcast.login</string>"));
        assert!(plist.contains("<string>/Applications/HdrCast &amp; Co.app/Contents/MacOS/HdrCast</string>"));
        assert!(plist.contains("<string>--background</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert_eq!(agent_program(&plist).as_deref(), Some("/Applications/HdrCast & Co.app/Contents/MacOS/HdrCast"));
        assert_eq!(agent_program("<plist><dict><key>Label</key><string>x</string></dict></plist>"), None);
    }

    #[test]
    fn the_view_says_what_the_page_needs() {
        let view = Autostart { on: false, other: Some(r"D:\old\HdrCast.exe".into()), disabled: false }.view();
        assert_eq!(view["supported"], SUPPORTED);
        assert_eq!(view["on"], false);
        assert_eq!(view["other"], r"D:\old\HdrCast.exe");
        assert_eq!(Autostart::default().view()["other"], Value::Null);
    }
}
