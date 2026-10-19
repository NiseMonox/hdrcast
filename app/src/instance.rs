//! One HdrCast per profile. Started again while it runs (its shortcut clicked while it sits in the notification
//! area, the sign-in entry after the user opened it by hand), the new process hands its arguments to the running
//! one, which shows its window (and offers an invite it was given), and exits. Two copies would both announce
//! this computer to the group under the same device key.
//!
//! The running app holds a lock on `<settings>/instance.lock` (`LockFileEx` / `flock`); a later start leaves its
//! arguments in `open-request.json` next to it, which the running app picks up within half a second
//! (`take_request`, from the status loop). A separate profile (`HDRCAST_DATA_ROOT`) has a folder of its own, so two
//! profiles still run side by side.

use std::fs::{self, File, OpenOptions};
use std::path::Path;
use std::time::{Duration, Instant};

use hdrcast_core::common::app_paths;

const LOCK_FILE: &str = "instance.lock";
const REQUEST_FILE: &str = "open-request.json";

/// Held for as long as the app runs; the system lets go of the lock when the process ends, however it ends.
pub struct Instance {
    _lock: Option<File>,
}

/// The lock, or None when another HdrCast of this profile runs.
pub fn claim() -> Option<Instance> {
    claim_in(&app_paths::config_dir())
}

/// Asks the running HdrCast to show itself (with these arguments) and waits a moment for it to pick that up.
/// False when it did not within two seconds (it may be stuck).
pub fn ask_running(args: &[String]) -> bool {
    ask_in(&app_paths::config_dir(), args, Duration::from_secs(2))
}

/// The arguments of a later start, once (for the running app).
pub fn take_request() -> Option<Vec<String>> {
    take_in(&app_paths::config_dir())
}

/// When the lock cannot be taken for any other reason than another HdrCast (no settings folder, a file system
/// without locks) the app runs anyway: better twice than not at all.
fn claim_in(dir: &Path) -> Option<Instance> {
    if !app_paths::ensure_dir(dir) {
        return Some(Instance { _lock: None });
    }
    let Ok(file) = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(LOCK_FILE)) else {
        return Some(Instance { _lock: None });
    };
    match try_lock(&file) {
        Ok(true) => {
            // a request nobody picked up (its app had just ended) is not for this start
            let _ = fs::remove_file(dir.join(REQUEST_FILE));
            Some(Instance { _lock: Some(file) })
        }
        Ok(false) => None,
        Err(_) => Some(Instance { _lock: None }),
    }
}

/// An exclusive lock on the file, without waiting: false when another process holds it. The lock goes with the
/// handle, so the system lets go of it when the process ends.
#[cfg(windows)]
fn try_lock(file: &File) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{ERROR_LOCK_VIOLATION, HANDLE};
    use windows::Win32::Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY};
    use windows::Win32::System::IO::OVERLAPPED;
    let mut overlapped = OVERLAPPED::default();
    // SAFETY: the handle belongs to `file`, which outlives the call; locking the first byte changes nothing else.
    let locked = unsafe {
        LockFileEx(HANDLE(file.as_raw_handle()), LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY, None, 1, 0, &mut overlapped)
    };
    match locked {
        Ok(()) => Ok(true),
        Err(e) if e.code() == ERROR_LOCK_VIOLATION.to_hresult() => Ok(false),
        Err(e) => Err(std::io::Error::from_raw_os_error(e.code().0 & 0xffff)),
    }
}

#[cfg(unix)]
fn try_lock(file: &File) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: `file` is an open descriptor owned by the caller; flock only changes its lock state.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(not(any(windows, unix)))]
fn try_lock(_file: &File) -> std::io::Result<bool> {
    Ok(true)
}

fn ask_in(dir: &Path, args: &[String], patience: Duration) -> bool {
    let (temp, request) = (dir.join(format!("{REQUEST_FILE}.{}", std::process::id())), dir.join(REQUEST_FILE));
    let text = serde_json::to_string(args).unwrap_or_else(|_| "[]".into());
    // written beside it and renamed, so the running app never reads half of it
    if fs::write(&temp, text).is_err() || fs::rename(&temp, &request).is_err() {
        let _ = fs::remove_file(&temp);
        return false;
    }
    let begun = Instant::now();
    while begun.elapsed() < patience {
        if !request.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn take_in(dir: &Path) -> Option<Vec<String>> {
    let path = dir.join(REQUEST_FILE);
    let text = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    Some(serde_json::from_str(&text).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_start_hands_its_arguments_to_the_first() {
        let dir = std::env::temp_dir().join(format!("hdrcast-instance-test-{}", std::process::id()));
        let first = claim_in(&dir).expect("the first start gets the lock");
        assert!(first._lock.is_some());
        assert!(claim_in(&dir).is_none(), "a second start finds the first");
        assert!(take_in(&dir).is_none(), "nothing asked yet");

        // the second start waits for the first to pick its request up
        let asker_dir = dir.clone();
        let asker = std::thread::spawn(move || ask_in(&asker_dir, &["hdrcast:abc.def".to_string()], Duration::from_secs(5)));
        let begun = Instant::now();
        let mut got = None;
        while got.is_none() && begun.elapsed() < Duration::from_secs(5) {
            got = take_in(&dir);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(got, Some(vec!["hdrcast:abc.def".to_string()]));
        assert!(asker.join().unwrap(), "the asker saw it picked up");
        assert!(!ask_in(&dir, &[], Duration::from_millis(200)), "nobody picks it up: the asker gives up");

        // once the first has ended, the next start is the first again and drops what was left for the old one
        drop(first);
        let again = claim_in(&dir).expect("the lock is free again");
        assert!(take_in(&dir).is_none());
        drop(again);
        let _ = fs::remove_dir_all(&dir);
    }
}
