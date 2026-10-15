//! Which process tree to leave out of the desktop audio (design.md section 2.4), e.g. the voice chat, so friends
//! in the call do not hear themselves. Windows' process-loopback capture excludes exactly one process and
//! everything it started, and apps like Discord play their audio from a helper that the main process started,
//! so the target is the topmost process with the chosen name. Pure logic; unit-tested.

use std::collections::HashMap;

/// One row of a process snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEntry {
    pub pid: u32,
    pub parent_pid: u32,
    pub exe_name: String,
}

impl ProcessEntry {
    pub fn new(pid: u32, parent_pid: u32, exe_name: impl Into<String>) -> Self {
        Self {
            pid,
            parent_pid,
            exe_name: exe_name.into(),
        }
    }
}

pub const MAX_NAME_LENGTH: usize = 100;

/// Characters Windows does not allow in a file name. The names are Windows program names whatever the host
/// is, so the Windows set applies everywhere.
fn is_invalid_file_name_char(c: char) -> bool {
    (c as u32) < 32 || matches!(c, '"' | '<' | '>' | '|' | ':' | '*' | '?' | '\\' | '/')
}

/// What the user typed or picked, reduced to an executable file name (`discord` -> `discord.exe`,
/// `C:\Apps\Discord.exe` -> `Discord.exe`); `""` when nothing usable is left.
pub fn normalize_exe_name(name: &str) -> String {
    let s = name.trim().trim_matches('"').trim();
    if s.is_empty() {
        return String::new();
    }
    let s = match s.rfind(['\\', '/']) {
        Some(slash) => &s[slash + 1..],
        None => s,
    };
    if s.is_empty() || s.chars().any(is_invalid_file_name_char) {
        return String::new();
    }
    let mut s = s.to_owned();
    let has_exe = s.len() >= 4
        && s.is_char_boundary(s.len() - 4)
        && s[s.len() - 4..].eq_ignore_ascii_case(".exe");
    if !has_exe {
        s.push_str(".exe");
    }
    if s.encode_utf16().count() > MAX_NAME_LENGTH {
        return String::new();
    }
    s
}

fn named(p: &ProcessEntry, exe_name: &str) -> bool {
    // Ordinal, case-insensitive: Windows file names.
    p.exe_name.to_lowercase() == exe_name.to_lowercase()
}

/// The root of the largest tree of processes named `exe_name` (a process whose parent has another name, or
/// is gone), or 0 when none runs. Case-insensitive; a reused parent id with another name ends the walk.
pub fn find_root<'a>(processes: impl IntoIterator<Item = &'a ProcessEntry>, exe_name: &str) -> u32 {
    if exe_name.is_empty() {
        return 0;
    }
    let by_pid: HashMap<u32, &ProcessEntry> = processes.into_iter().map(|p| (p.pid, p)).collect();

    let mut members: HashMap<u32, u32> = HashMap::new();
    for p in by_pid.values().copied() {
        if !named(p, exe_name) {
            continue;
        }
        let mut top = p;
        // bounded: parent ids can form a loop after reuse
        for _ in 0..64 {
            if top.parent_pid == top.pid {
                break;
            }
            match by_pid.get(&top.parent_pid) {
                Some(parent) if named(parent, exe_name) => top = parent,
                _ => break,
            }
        }
        *members.entry(top.pid).or_insert(0) += 1;
    }

    let (mut best, mut best_count) = (0u32, 0u32);
    for (&pid, &count) in &members {
        if count > best_count || (count == best_count && pid < best) {
            best = pid;
            best_count = count;
        }
    }
    best
}
