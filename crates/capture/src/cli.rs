//! The command-line modes that belong to the capture engine (design.md section 2.1):
//!
//! ```text
//! <exe> list-outputs [--json]    list the outputs (index, HDR state)
//! <exe> audio-devices            list the audio render endpoints
//! <exe> capture <options>        the capture engine driven by the app (or by hand)
//! ```

use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::args::CaptureArgs;
use crate::engine::{exit_codes, CaptureEngine};
use crate::log;
use crate::output::OutputInfo;
use crate::platform;

/// Runs the engine mode selected by `args` (the process arguments without the program name) and returns its
/// exit code, or `None` when `args` do not select one - the app's `main` calls this first and carries on with
/// its own start-up on `None`.
///
/// stdout carries the results (the JSON statistics lines of `capture`, the output list); logs and errors go
/// to stderr.
pub fn cli_main(args: &[String]) -> Option<i32> {
    let verb = args.first()?.to_lowercase();
    let rest = &args[1..];
    let mode: fn(&[String]) -> i32 = match verb.as_str() {
        "list-outputs" => list_outputs,
        "audio-devices" => list_audio_devices,
        "capture" => run_capture,
        _ => return None,
    };
    platform::prepare_cli();
    Some(mode(rest))
}

/// The help text of the engine modes, for the app's own `--help`.
pub fn usage() -> String {
    let exe = exe_name();
    format!(
        "  {exe} list-outputs [--json] list the DXGI outputs (index, HDR state)\n\
         \x20 {exe} audio-devices         list the WASAPI render endpoints\n\
         \n\
         {}",
        CaptureArgs::usage(&exe)
    )
}

/// File name of the running program, for the usage text.
fn exe_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "hdrcast".to_owned())
}

fn list_outputs(args: &[String]) -> i32 {
    let mut json = false;
    for a in args {
        if a == "--json" {
            json = true;
        } else {
            log::stderr_text(&format!("unknown option: {a}\n"));
            return exit_codes::BAD_ARGUMENTS;
        }
    }
    match platform::enumerate_outputs() {
        Ok(outputs) => {
            if json {
                log::stdout_line(&OutputInfo::to_json(&outputs));
            } else {
                log::stdout_text(&OutputInfo::to_table(&outputs));
            }
            exit_codes::OK
        }
        Err(message) => {
            log::stderr_text(&format!("list-outputs failed: {message}\n"));
            exit_codes::DEVICE_ERROR
        }
    }
}

fn list_audio_devices(_args: &[String]) -> i32 {
    for device in platform::list_audio_devices() {
        let default = if device.is_default { " (default)" } else { "" };
        log::stdout_line(&format!("{}\t{}{default}", device.id, device.name));
    }
    exit_codes::OK
}

fn run_capture(args: &[String]) -> i32 {
    let parsed = match CaptureArgs::parse(args) {
        Ok(parsed) => parsed,
        Err(e) => {
            log::stderr_text(&format!("{e}\n"));
            log::stderr_text(&CaptureArgs::usage(&exe_name()));
            return exit_codes::BAD_ARGUMENTS;
        }
    };

    if parsed.self_test {
        return platform::native().self_test(parsed.sdr_white);
    }

    let mut engine = CaptureEngine::new(parsed);
    // Ctrl+C / console close / the app killing the process tree: stop cleanly so the pipe is flushed.
    platform::install_stop_handler(engine.stop_handle());
    let code = match catch_unwind(AssertUnwindSafe(|| engine.run())) {
        Ok(code) => code,
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".to_owned());
            log::error(format!("capture: unhandled error: {message}"));
            exit_codes::DEVICE_ERROR
        }
    };
    platform::remove_stop_handler();
    code
}
