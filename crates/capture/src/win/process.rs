//! Other processes: the parent watch (`--parent-pid`) and the process snapshot behind "leave one app out".

use windows::Win32::Foundation::WAIT_TIMEOUT;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};

use super::util::{error_text, wide_to_string, Handle};
use crate::app_audio::ProcessEntry;
use crate::platform::ProcessWatch;

/// A SYNCHRONIZE handle on a process: asking whether it ended is one cheap wait, and the PID cannot be
/// recycled while the handle is open.
pub(crate) struct ProcessHandleWatch {
    handle: Handle,
}

impl ProcessHandleWatch {
    pub(crate) fn open(pid: u32) -> Option<Self> {
        // SAFETY: plain call; on success the handle is ours to close.
        let handle = unsafe {
            OpenProcess(PROCESS_SYNCHRONIZE, false, pid)
                .ok()
                .map(|h| Handle::from_raw(h))?
        };
        Some(Self { handle })
    }
}

impl ProcessWatch for ProcessHandleWatch {
    fn is_gone(&self) -> bool {
        // SAFETY: the handle is live for as long as `self` is.
        unsafe { WaitForSingleObject(self.handle.raw(), 0) != WAIT_TIMEOUT }
    }
}

/// Process id, parent id and exe name of every running process (Toolhelp32: fast, no WMI).
pub(crate) fn take_snapshot() -> Result<Vec<ProcessEntry>, String> {
    // SAFETY: plain call; on success the snapshot handle is ours to close.
    let snapshot = unsafe {
        CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            .map(|h| Handle::from_raw(h))
            .map_err(|e| error_text(&e))?
    };
    let mut list = Vec::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: `snapshot` is a live snapshot handle and `entry` is a properly sized PROCESSENTRY32W.
    let mut more = unsafe { Process32FirstW(snapshot.raw(), &mut entry).is_ok() };
    while more {
        list.push(ProcessEntry::new(
            entry.th32ProcessID,
            entry.th32ParentProcessID,
            wide_to_string(&entry.szExeFile),
        ));
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot.raw(), &mut entry).is_ok() };
    }
    Ok(list)
}
