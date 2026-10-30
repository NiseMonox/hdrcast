//! Control mode: this process's GPU work (the conversion shader, the encoder) goes ahead of other programs', as
//! Sunshine and Vibepollo set it up (`display_base.cpp`). Otherwise a game, or the desktop composing at 240 Hz, keeps
//! the conversion of a captured picture waiting for its turn on the GPU for several milliseconds.
//!
//! Two knobs: the process's scheduling class (`D3DKMTSetProcessSchedulingPriorityClass`, HIGH: Sunshine falls back from
//! REALTIME to HIGH on NVIDIA because REALTIME froze or crashed the driver there) and the device's GPU thread priority
//! (`IDXGIDevice::SetGPUThreadPriority`, the highest relative step, 7). Windows may refuse either to a process that does
//! not run as administrator; the log says what was taken.

use windows::core::{s, w, Interface};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Threading::GetCurrentProcess;

/// D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH.
const SCHEDULING_CLASS_HIGH: i32 = 4;
const GPU_THREAD_PRIORITY: i32 = 7;

type SetSchedulingClass = unsafe extern "system" fn(HANDLE, i32) -> i32;

/// Raises what can be raised; says what came of it, for the log.
pub(crate) fn raise(device: &ID3D11Device) -> String {
    let class = {
        // SAFETY: gdi32 is loaded in every GUI-capable process; a found export has the documented signature
        // (NTSTATUS D3DKMTSetProcessSchedulingPriorityClass(HANDLE, D3DKMT_SCHEDULINGPRIORITYCLASS)).
        let function = unsafe { GetModuleHandleW(w!("gdi32.dll")) }
            .ok()
            .and_then(|gdi| unsafe { GetProcAddress(gdi, s!("D3DKMTSetProcessSchedulingPriorityClass")) });
        match function {
            Some(function) => {
                // SAFETY: the transmute names the export's real signature; the pseudo handle needs no closing.
                let status = unsafe {
                    let set: SetSchedulingClass = std::mem::transmute(function);
                    set(GetCurrentProcess(), SCHEDULING_CLASS_HIGH)
                };
                if status == 0 {
                    "GPU scheduling class high".to_owned()
                } else {
                    format!("GPU scheduling class unchanged (0x{status:08x})")
                }
            }
            None => "GPU scheduling class unchanged (no D3DKMT export)".to_owned(),
        }
    };
    // SAFETY: plain COM calls on a live device.
    let thread = match unsafe { device.cast::<IDXGIDevice>().and_then(|d| d.SetGPUThreadPriority(GPU_THREAD_PRIORITY)) } {
        Ok(()) => format!("GPU thread priority {GPU_THREAD_PRIORITY}"),
        Err(e) => format!("GPU thread priority unchanged ({})", super::util::error_text(&e)),
    };
    format!("{class}, {thread}")
}
