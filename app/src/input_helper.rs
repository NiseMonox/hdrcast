//! The input service's switch in Settings (docs/design.md §9.1): Windows ignores the viewer's input over
//! windows of programs that run as administrator, unless a small program with those rights types it. What the page
//! shows is read from the system every time, never remembered.

use std::path::PathBuf;

use hdrcast_control::elevated;
use serde_json::{json, Value};

/// The program (it is the service, too) that ships next to the app. Setting up copies it to the program files folder.
fn bundled() -> PathBuf {
    crate::state::current_exe().with_file_name(elevated::HELPER_EXE)
}

pub fn view() -> Value {
    let status = elevated::status();
    json!({
        "supported": status.supported,
        "installed": status.installed,
        "outdated": status.outdated,
        "running": status.running,
        // HdrCast runs as administrator itself
        "notNeeded": status.not_needed,
        // the program to set up is next to HdrCast.exe
        "bundled": bundled().is_file(),
    })
}

/// Sets the service up (`on`) or takes it away: Windows asks the user for permission, which takes as long as the user
/// takes. Hands back what holds afterwards.
#[cfg(windows)]
pub fn set(on: bool) -> Result<Value, String> {
    use elevated::setup;
    let result = if on {
        setup::install_with_prompt(&bundled(), setup::PROMPT_TIMEOUT)
    } else {
        setup::uninstall_with_prompt(&bundled(), setup::PROMPT_TIMEOUT)
    };
    result.map_err(|e| e.to_string())?;
    Ok(view())
}

#[cfg(not(windows))]
pub fn set(_on: bool) -> Result<Value, String> {
    Err("There is nothing to set up on this system.".to_string())
}
