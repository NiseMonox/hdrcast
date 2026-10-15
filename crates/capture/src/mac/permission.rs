//! The screen-recording permission (TCC). ScreenCaptureKit delivers neither pictures nor the system audio
//! without it.

use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};

/// Where the user grants it; shown whenever capturing is refused.
pub(super) const HOW_TO_ALLOW: &str = "allow it in System Settings > Privacy & Security > Screen & System \
    Audio Recording (\"Screen Recording\" before macOS 15), then quit and restart HdrCast (when the engine \
    was started from a terminal, the entry to switch on is that terminal application, and it is the \
    terminal that has to be restarted)";

/// True when this process may record the screen. Never shows anything.
pub(super) fn screen_capture_allowed() -> bool {
    CGPreflightScreenCaptureAccess()
}

/// Checks the permission and, when it is missing, asks macOS for it.
///
/// The request makes the system show its prompt (only the first time for a given program) and puts the
/// program on the list in System Settings. Even when the user agrees, a grant only applies to processes
/// started afterwards, so a missing permission always ends this session with an explanation.
pub(super) fn ensure_screen_capture_access() -> Result<(), String> {
    if CGPreflightScreenCaptureAccess() {
        return Ok(());
    }
    if CGRequestScreenCaptureAccess() {
        return Ok(());
    }
    Err(format!(
        "macOS has not allowed this program to record the screen; {HOW_TO_ALLOW}"
    ))
}
