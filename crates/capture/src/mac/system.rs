//! What this Mac can do: every ScreenCaptureKit feature newer than the framework itself is looked up at run
//! time (never assumed from the SDK the program was built with).

use std::ffi::CStr;

use objc2::runtime::AnyClass;
use objc2::{available, sel, ClassType};
use objc2_screen_capture_kit::SCStreamConfiguration;

/// ScreenCaptureKit exists (macOS 12.3). Checked by class name before any of its classes is touched:
/// asking the bindings for a class that is not there panics.
pub(super) fn screen_capture_kit_available() -> bool {
    AnyClass::get(c"SCStream").is_some() && AnyClass::get(c"SCStreamConfiguration").is_some()
}

/// True when instances of `SCStreamConfiguration` have the setter `selector`.
fn configuration_has(selector: objc2::runtime::Sel) -> bool {
    screen_capture_kit_available() && SCStreamConfiguration::class().responds_to(selector)
}

/// The stream can carry the system audio (`capturesAudio` and friends, macOS 13).
pub(super) fn audio_capture_supported() -> bool {
    configuration_has(sel!(setCapturesAudio:))
        && configuration_has(sel!(setExcludesCurrentProcessAudio:))
}

/// `preservesAspectRatio` (macOS 14).
pub(super) fn aspect_ratio_switch_supported() -> bool {
    configuration_has(sel!(setPreservesAspectRatio:))
}

/// `+[SCStreamConfiguration streamConfigurationWithPreset:]` (macOS 15).
pub(super) fn hdr_preset_supported() -> bool {
    screen_capture_kit_available()
        && SCStreamConfiguration::class()
            .class_method(sel!(streamConfigurationWithPreset:))
            .is_some()
}

/// Why this system cannot capture HDR, or `None` when it can: `captureDynamicRange` came with macOS 15 and,
/// in the words of its header, "is only supported with Apple Silicon Mac, setting this property on Intel
/// Mac will have no effect".
pub(super) fn hdr_capture_obstacle() -> Option<&'static str> {
    if !available!(macos = 15.0) || !configuration_has(sel!(setCaptureDynamicRange:)) {
        return Some("HDR capture needs macOS 15 or later");
    }
    if !apple_silicon() {
        return Some("HDR capture needs a Mac with Apple silicon");
    }
    None
}

/// True on Apple silicon hardware, also for an Intel build running under Rosetta.
pub(super) fn apple_silicon() -> bool {
    cfg!(target_arch = "aarch64") || sysctl_i32(c"hw.optional.arm64") == Some(1)
}

/// The macOS version, e.g. `15.1`.
pub(super) fn os_version() -> String {
    sysctl_string(c"kern.osproductversion").unwrap_or_else(|| "unknown version".to_owned())
}

fn sysctl_i32(name: &CStr) -> Option<i32> {
    let mut value: i32 = 0;
    let mut size = std::mem::size_of::<i32>();
    // SAFETY: `name` is NUL-terminated; `value` / `size` describe a writable buffer of exactly `size` bytes;
    // no new value is set (null, 0).
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut value as *mut i32).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (result == 0 && size == std::mem::size_of::<i32>()).then_some(value)
}

fn sysctl_string(name: &CStr) -> Option<String> {
    let mut buffer = [0u8; 64];
    let mut size = buffer.len();
    // SAFETY: as in `sysctl_i32`; the kernel writes at most `size` bytes and stores the length it wrote.
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || size > buffer.len() {
        return None;
    }
    let text = &buffer[..size];
    let text = text.split(|byte| *byte == 0).next().unwrap_or(text);
    std::str::from_utf8(text)
        .ok()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}
