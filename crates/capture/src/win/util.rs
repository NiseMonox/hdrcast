//! Small Win32 helpers shared by the Windows backend.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows::core::{Error, HRESULT, PWSTR};
use windows::Win32::Foundation::{HANDLE, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

/// The system's text for an error plus its HRESULT, e.g. `Access is denied. (HRESULT 0x80070005)`.
pub(crate) fn error_text(e: &Error) -> String {
    let message = e.message();
    let message = message.trim();
    if message.is_empty() {
        format!("HRESULT {}", hr_text(e.code()))
    } else {
        format!("{message} (HRESULT {})", hr_text(e.code()))
    }
}

/// Formats an HRESULT as 0x8xxxxxxx.
pub(crate) fn hr_text(hr: HRESULT) -> String {
    format!("0x{:08X}", hr.0 as u32)
}

/// A `windows` error as an `io::Error` that keeps the Win32 error code (so `kind()` and the text work).
pub(crate) fn io_error(e: Error) -> io::Error {
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((hr & 0xFFFF) as i32)
    } else {
        io::Error::other(error_text(&e))
    }
}

/// A fixed-size UTF-16 field (NUL-terminated or full) as a `String`.
pub(crate) fn wide_to_string(wide: &[u16]) -> String {
    let len = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..len])
}

/// `s` as a NUL-terminated UTF-16 string.
pub(crate) fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Takes a string the system allocated with `CoTaskMemAlloc` and frees it.
///
/// # Safety
/// `p` must be null or a NUL-terminated UTF-16 string allocated with `CoTaskMemAlloc` that nobody else frees.
pub(crate) unsafe fn take_co_string(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: per the contract `p` is a valid NUL-terminated string, and it is freed exactly once, here.
    unsafe {
        let s = String::from_utf16_lossy(p.as_wide());
        CoTaskMemFree(Some(p.as_ptr() as *const _));
        s
    }
}

/// An owned kernel handle (closed on drop).
pub(crate) struct Handle(OwnedHandle);

impl Handle {
    /// # Safety
    /// `handle` must be a valid kernel handle that the caller owns: it is closed when this is dropped.
    pub(crate) unsafe fn from_raw(handle: HANDLE) -> Self {
        // SAFETY: ownership of a valid handle is the caller's contract.
        Self(unsafe { OwnedHandle::from_raw_handle(handle.0) })
    }

    pub(crate) fn raw(&self) -> HANDLE {
        HANDLE(self.0.as_raw_handle())
    }
}

/// COM initialised for the multithreaded apartment on the current thread, until dropped.
///
/// Every COM object of the audio code is created and used on threads that entered the MTA through this: the
/// activation callback of the process loopback arrives on a system thread and would otherwise need a message
/// pump on an STA.
pub(crate) struct ComGuard {
    uninitialize: bool,
}

impl ComGuard {
    pub(crate) fn mta() -> Result<Self, String> {
        // SAFETY: plain call; balanced by CoUninitialize in Drop when it succeeded.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() {
            Ok(Self { uninitialize: true })
        } else if hr == RPC_E_CHANGED_MODE {
            // The thread already lives in an STA someone else set up: COM is usable, just not ours to tear down.
            Ok(Self {
                uninitialize: false,
            })
        } else {
            Err(format!(
                "COM initialisation failed (HRESULT {})",
                hr_text(hr)
            ))
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.uninitialize {
            // SAFETY: balances the successful CoInitializeEx of this thread; every COM pointer created under
            // the guard is declared after it and therefore released before this runs.
            unsafe { CoUninitialize() };
        }
    }
}

/// Runs `work` on a short-lived thread of its own inside the MTA, so callers can come from any thread
/// (an STA UI thread included) and COM pointers never cross a thread.
pub(crate) fn run_mta<T: Send>(
    work: impl FnOnce() -> Result<T, String> + Send,
) -> Result<T, String> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let _com = ComGuard::mta()?;
                work()
            })
            .join()
            .unwrap_or_else(|_| Err("the COM worker thread panicked".to_owned()))
    })
}

/// Makes the current thread per-monitor DPI aware (v2) until dropped.
///
/// DXGI reports output sizes in the caller's DPI space and `DuplicateOutput1` refuses callers that are not
/// per-monitor aware, so on a 150 % display a DPI-unaware caller would see 2560x1440 for a 3840x2160 output.
/// The C# sender got this from its manifest; a library cannot rely on its host's manifest.
pub(crate) struct DpiAwareThread {
    previous: DPI_AWARENESS_CONTEXT,
}

impl DpiAwareThread {
    pub(crate) fn enter() -> Self {
        // SAFETY: plain call with a system-defined constant; returns the previous context (null on failure).
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Self { previous }
    }
}

impl Drop for DpiAwareThread {
    fn drop(&mut self) {
        if !self.previous.0.is_null() {
            // SAFETY: restores the context this thread had before `enter`.
            unsafe { SetThreadDpiAwarenessContext(self.previous) };
        }
    }
}
