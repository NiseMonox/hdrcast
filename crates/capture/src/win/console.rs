//! Process-level set-up for the command-line modes: console, DPI awareness, Ctrl+C.

use std::sync::{Mutex, PoisonError};

use windows::core::BOOL;
use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::Storage::FileSystem::{GetFileType, FILE_TYPE_UNKNOWN};
use windows::Win32::System::Console::{
    AttachConsole, GetStdHandle, SetConsoleCtrlHandler, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use crate::engine::StopHandle;

pub(crate) fn prepare_cli() {
    ensure_console();
    // Per-monitor DPI awareness for the whole process, as the C# sender's manifest declared: output sizes
    // are then real pixels and DuplicateOutput1 accepts us. Fails harmlessly when the host's manifest
    // already chose (the capture code also asks per thread, see DpiAwareThread).
    // SAFETY: plain call with a system-defined constant.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

/// Makes stdout / stderr usable for the console verbs of a GUI-subsystem executable: if the parent already
/// redirected the standard handles (app -> capture child) nothing is done; otherwise attach to the parent's
/// console so `<exe> list-outputs` prints in the terminal. Must run before the first output.
fn ensure_console() {
    // SAFETY: plain calls; the standard handle is only inspected.
    unsafe {
        if let Ok(handle) = GetStdHandle(STD_OUTPUT_HANDLE) {
            if !handle.is_invalid() && GetFileType(handle) != FILE_TYPE_UNKNOWN {
                return;
            }
        }
        // no console available: output goes nowhere
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

pub(crate) fn local_time_of_day() -> (u32, u32, u32, u32) {
    // SAFETY: plain call returning a value.
    let t: SYSTEMTIME = unsafe { GetLocalTime() };
    (
        u32::from(t.wHour),
        u32::from(t.wMinute),
        u32::from(t.wSecond),
        u32::from(t.wMilliseconds),
    )
}

/// The engine the console control handler stops (a handler routine gets no context argument).
static STOP: Mutex<Option<StopHandle>> = Mutex::new(None);

unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> BOOL {
    if let Some(stop) = STOP.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
        stop.request_stop();
    }
    // Windows ends the process when a close / logoff / shutdown handler returns: give the loop a moment to
    // notice and flush the pipe.
    std::thread::sleep(std::time::Duration::from_millis(300));
    true.into()
}

pub(crate) fn install_stop_handler(stop: StopHandle) {
    *STOP.lock().unwrap_or_else(PoisonError::into_inner) = Some(stop);
    // SAFETY: `ctrl_handler` is a valid handler routine for the lifetime of the process.
    let _ = unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), true) };
}

pub(crate) fn remove_stop_handler() {
    // SAFETY: removes the routine registered in `install_stop_handler`.
    let _ = unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), false) };
    *STOP.lock().unwrap_or_else(PoisonError::into_inner) = None;
}
