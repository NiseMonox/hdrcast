//! The real `HdrCastInput` program, started the way the service starts it minus the service (`serve --user ...
//! --stop-event ...`): it listens on the user's pipe until the event asks it to leave. Ignored by default because
//! the program writes its log under C:\ProgramData\HdrCast, and because it needs the pipe to be free (no input
//! service running for this user at the moment).
//!
//!     cargo test -p hdrcast-control --test input_program -- --ignored

#![cfg(windows)]

use std::process::Command;
use std::time::{Duration, Instant};

use hdrcast_control::elevated::{pipe_name, setup, status};
use windows::core::PCWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !done() {
        assert!(Instant::now() < deadline, "gave up waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
#[ignore = "starts the real input program and writes its log under C:\\ProgramData\\HdrCast"]
fn the_input_program_listens_until_the_event_asks_it_to_leave() {
    let sid = setup::user_sid().unwrap();
    assert!(!status().running, "somebody listens on {} already: wait for it to leave (45 seconds) and try again", pipe_name(&sid));

    let name: Vec<u16> = format!(r"Local\HdrCastInputIntegration-{}", std::process::id()).encode_utf16().chain(Some(0)).collect();
    // SAFETY: a NUL-terminated name; the default access list lets a program of the same user open it.
    let event = unsafe { CreateEventW(None, true, false, PCWSTR(name.as_ptr())) }.unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_HdrCastInput"))
        .args(["serve", "--user", &sid, "--stop-event", &String::from_utf16_lossy(&name[..name.len() - 1])])
        .spawn()
        .unwrap();

    wait_for("the program to listen", || status().running);
    // SAFETY: the event made above.
    unsafe { SetEvent(event) }.unwrap();
    wait_for("the program to leave", || child.try_wait().unwrap().is_some());
    assert!(child.wait().unwrap().success(), "it leaves with code 0 when it is asked to");
    assert!(!status().running);
    // SAFETY: closed once.
    unsafe {
        let _ = CloseHandle(event);
    }
}

#[test]
#[ignore = "starts the real input program, which writes its log under C:\\ProgramData\\HdrCast"]
fn the_program_started_by_hand_as_the_service_says_it_must_be_run_by_windows() {
    let status = Command::new(env!("CARGO_BIN_EXE_HdrCastInput")).args(["service", "--user", "S-1-5-21-1-2-3-1001"]).status().unwrap();
    assert_eq!(status.code(), Some(5), "5: not started by the service control manager");
}

#[test]
#[ignore = "starts the real input program, which writes its log under C:\\ProgramData\\HdrCast"]
fn a_program_that_is_given_nothing_to_do_says_how_it_is_used_and_leaves() {
    let status = Command::new(env!("CARGO_BIN_EXE_HdrCastInput")).status().unwrap();
    assert_eq!(status.code(), Some(1));
}
