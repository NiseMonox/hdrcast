//! HdrCastInput: types for HdrCast into windows of programs that run as administrator (src/elevated/mod.rs). One
//! program, four jobs:
//!
//!   HdrCastInput service --user <SID>        the Windows service (run by Windows, not by hand): starts `serve` in
//!                                            that user's session as SYSTEM, and stops when it has left
//!   HdrCastInput serve [--user <SID>]        the input program: listens on the user's pipe and replays what HdrCast
//!                                            sends, until nobody has been connected for a while or the service
//!                                            asks it to leave (`--stop-event <name>`)
//!   HdrCastInput install --user <SID>        elevated: copies itself to the program files folder and registers the
//!                                            service for that user
//!   HdrCastInput uninstall                   elevated: removes both again
//!
//! `--dir <folder>` and `--service <name>` (install, uninstall, service) take the place of the usual folder and
//! service name; they exist for trying it out. The program has no window; what it does goes to the log named by
//! `setup::log_path`.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    std::process::exit(windows_main::run(std::env::args().skip(1).collect()));
}

#[cfg(not(windows))]
fn main() {
    eprintln!("HdrCastInput is for Windows only.");
    std::process::exit(1);
}

#[cfg(windows)]
mod windows_main {
    use std::path::PathBuf;
    use std::time::Duration;

    use hdrcast_control::elevated::server::{self, Left, StopEvent};
    use hdrcast_control::elevated::{pipe_name, service, setup, SERVICE_NAME};

    /// How long the input program stays after the last HdrCast went away (HdrCast starts the service again when it
    /// needs it).
    const STAY: Duration = Duration::from_secs(45);

    fn option(args: &[String], name: &str) -> Option<String> {
        args.iter().position(|a| a == name).and_then(|at| args.get(at + 1)).cloned()
    }

    pub fn run(args: Vec<String>) -> i32 {
        let dir = option(&args, "--dir").map_or_else(setup::install_dir, PathBuf::from);
        let service_name = option(&args, "--service").unwrap_or_else(|| SERVICE_NAME.to_string());
        match args.first().map(String::as_str) {
            Some("service") => {
                let Some(user) = option(&args, "--user") else {
                    setup::log("service: --user <SID> is missing");
                    return 1;
                };
                match service::run(&service_name, &user) {
                    Ok(()) => 0,
                    Err(e) => {
                        setup::log(&format!("service: {e}"));
                        5
                    }
                }
            }
            Some("serve") => serve(option(&args, "--user"), option(&args, "--stop-event")),
            Some("install") => {
                let Some(user) = option(&args, "--user") else {
                    setup::log("install: --user <SID> is missing");
                    return 1;
                };
                match setup::install(&user, &dir, &service_name) {
                    Ok(()) => {
                        setup::log(&format!("install: done ({})", dir.display()));
                        0
                    }
                    Err(e) => {
                        setup::log(&format!("install: {e}"));
                        2
                    }
                }
            }
            Some("uninstall") => match setup::uninstall(&dir, &service_name) {
                Ok(()) => {
                    setup::log("uninstall: done");
                    0
                }
                Err(e) => {
                    setup::log(&format!("uninstall: {e}"));
                    3
                }
            },
            _ => {
                setup::log("usage: HdrCastInput service --user <SID> | serve | install --user <SID> | uninstall");
                1
            }
        }
    }

    /// The input program. Started by the service it is told whose pipe to make (it runs as SYSTEM, so its own user
    /// is no help) and which event asks it to leave; started by hand it serves the user it runs as.
    fn serve(user: Option<String>, stop_event: Option<String>) -> i32 {
        server::become_dpi_aware();
        let sid = match user.map(Ok).unwrap_or_else(setup::user_sid) {
            Ok(sid) => sid,
            Err(e) => {
                setup::log(&format!("serve: {e}"));
                return 4;
            }
        };
        let stop = match stop_event.map(|name| StopEvent::open(&name)).transpose() {
            Ok(stop) => stop,
            Err(e) => {
                setup::log(&format!("serve: {e}"));
                return 4;
            }
        };
        let server = match server::Server::start(&pipe_name(&sid), &sid, server::direct_injector()) {
            Ok(server) => server,
            Err(e) => {
                // another one is listening already, or the pipe cannot be made
                setup::log(&format!("serve: {e}"));
                return 4;
            }
        };
        setup::log(&format!("serve: listening for {sid}, elevated: {}", setup::is_elevated()));
        let left = server::wait_until_done(&server, STAY, stop.as_ref());
        // clients that are still connected are let go of, and let go of what they hold
        server.stop();
        setup::log(match left {
            Left::Idle => "serve: nobody connected, leaving",
            Left::Asked => "serve: asked to leave, left",
        });
        0
    }
}
