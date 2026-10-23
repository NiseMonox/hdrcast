#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod access;
mod autostart;
mod clipboard;
mod commands;
mod control_host;
mod control_viewer;
mod instance;
mod ipc_stream;
mod live;
mod logs;
mod mpv_ipc;
mod mpv_runner;
mod net_report;
mod sender_runner;
mod share;
mod state;
mod status;
mod tray;
mod watch;
mod window;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hdrcast_core::common::firewall::{self, row as firewall_row};
use tauri::{AppHandle, Emitter, Manager};

use crate::logs::Logs;
use crate::state::AppState;

/// One executable, several modes (docs/design.md §2.1):
///   HdrCast                      the app
///   HdrCast --background         the app without its window, in the notification area (how it starts at sign-in)
///   HdrCast hdrcast:…            the app, offering to join that invite
///   HdrCast capture <options>    the capture engine the app runs as a child (also list-outputs, audio-devices)
///   HdrCast firewall-allow       adds the Windows Firewall rule; started elevated by the app's Allow button
/// Started while it already runs, the app hands its arguments to the running one and exits (`instance.rs`).
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = hdrcast_capture::cli_main(&args) {
        std::process::exit(code);
    }
    if args.first().is_some_and(|a| a.eq_ignore_ascii_case(firewall::ALLOW_VERB)) {
        let outcome = firewall::run_allow_verb(state::APP_NAME, firewall_row::RULE_DESCRIPTION);
        Logs::default().tagged("app", &outcome.log_line);
        std::process::exit(outcome.exit_code);
    }
    let background = args.iter().any(|a| a == autostart::BACKGROUND_ARG);
    let Some(instance) = instance::claim() else {
        // The sign-in entry has nothing to add to a running HdrCast; anything else brings its window up.
        let line = if background {
            "started at sign-in, but HdrCast is running already"
        } else if instance::ask_running(&args) {
            "started again: the HdrCast that is running shows its window"
        } else {
            "started again, but the HdrCast that is running did not answer"
        };
        Logs::default().tagged("app", line);
        return;
    };
    let invite = args.first().filter(|a| a.to_ascii_lowercase().starts_with("hdrcast:")).map(|_| args.join(" "));
    run_app(invite, background, instance);
}

fn run_app(startup_invite: Option<String>, background: bool, instance: instance::Instance) {
    // held until the process ends (`run` below never returns)
    let _instance = instance;
    let logs = Arc::new(Logs::default());
    logs.tagged("app", &format!("HdrCast {} starting{}", env!("CARGO_PKG_VERSION"), if background { " in the background" } else { "" }));
    // A crash should at least leave its reason where the user (and whoever helps them) will look.
    let crash_logs = logs.clone();
    std::panic::set_hook(Box::new(move |info| crash_logs.tagged("crash", &info.to_string())));

    // The interface is drawn by the system's web view. Windows 11 has it; a Windows 10 that never got it would
    // otherwise show nothing at all.
    #[cfg(windows)]
    if tauri::webview_version().is_err() {
        logs.tagged("app", "WebView2 Runtime is not installed");
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("HdrCast")
            .set_description(
                "HdrCast needs the Microsoft Edge WebView2 Runtime, which this Windows does not have yet.\n\n\
                 Install it from https://developer.microsoft.com/microsoft-edge/webview2/ (the \"Evergreen Bootstrapper\"), \
                 then start HdrCast again.",
            )
            .show();
        return;
    }
    let setup_logs = logs.clone();

    tauri::Builder::default()
        .setup(move |app| {
            let handle = app.handle().clone();
            let (_, backdrop) = window::create(&handle, background)?;
            let state = Arc::new(AppState::new(setup_logs.clone(), backdrop, startup_invite.clone(), background));
            let _ = state.handle.set(handle.clone());
            window::apply_theme(&handle, &state.inner.lock().unwrap().ui.theme);
            app.manage(state.clone());
            if let Err(e) = tray::create(&handle) {
                setup_logs.tagged("app", &format!("no icon in the {} ({e}): closing the window quits", tray::PLACE));
                // without the icon a hidden window could not be opened at all
                state.start_hidden.store(false, Ordering::SeqCst);
            } else if background {
                setup_logs.tagged("app", &format!("started without a window: it opens from the icon in the {}", tray::PLACE));
            }
            setup_logs.tagged("app", &format!("settings: {}", hdrcast_core::common::app_paths::config_dir().display()));
            setup_logs.tagged("app", &format!("this computer's device code: {}", state.device.code()));
            live::restart(&state);
            // available to the group from the start, when the rules say so (it takes a moment: not on this thread)
            let starting = state.clone();
            std::thread::Builder::new().name("share-autostart".into()).spawn(move || share::autostart(&starting))?;

            // The page redraws from one snapshot; log lines go over in batches.
            std::thread::Builder::new().name("status".into()).spawn(move || {
                let mut ticks = 0u32;
                loop {
                    std::thread::sleep(Duration::from_millis(200));
                    let lines = state.logs.take_pending();
                    if !lines.is_empty() {
                        let _ = handle.emit("log", serde_json::json!({ "lines": lines }));
                    }
                    ticks += 1;
                    if ticks % 2 == 0 {
                        share::housekeeping(&state);
                        watch::housekeeping(&state);
                        let _ = handle.emit("status", status::snapshot(&state));
                        tray::show(&handle, tray_mode(&state));
                        if let Some(args) = instance::take_request() {
                            started_again(&state, &handle, &args);
                        }
                    }
                    // While in the group or watching, the firewall verdict is part of what peers are told: keep it fresh.
                    if ticks % 75 == 0 && (share::is_hosting(&state) || watch::is_active(&state)) {
                        commands::refresh_firewall(&state);
                    }
                }
            })?;
            Ok(())
        })
        .on_window_event(|window, event| {
            let tauri::WindowEvent::CloseRequested { api, .. } = event else { return };
            let Some(state) = window.try_state::<Arc<AppState>>().map(|s| s.inner().clone()) else { return };
            if share::stays_available(&state) && tray::exists(window.app_handle()) {
                // Closing the window does not take this computer out of the group: it waits in the notification area.
                api.prevent_close();
                let _ = window.hide();
                state.logs.tagged(
                    "app",
                    &format!("window closed: still available to connect from the icon in the {} (Quit is in its menu)", tray::PLACE),
                );
            } else if share::is_hosting(&state) || watch::is_active(&state) {
                api.prevent_close();
                quit(window.app_handle());
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::app_init,
            commands::settings_save,
            commands::autostart_get,
            commands::autostart_set,
            commands::tray_texts,
            commands::outputs_refresh,
            commands::audio_apps,
            commands::encoders_probe,
            commands::group_create,
            commands::group_rename,
            commands::group_detect_clipboard,
            commands::invite_preview,
            commands::invite_join,
            commands::invite_dismiss,
            commands::invite_copy,
            commands::clipboard_write,
            commands::app_focused,
            commands::firewall_allow,
            commands::tool_browse,
            commands::logs_open,
            commands::share_start,
            commands::share_stop,
            commands::share_kick,
            commands::control_set,
            commands::access_get,
            commands::access_save,
            commands::knock_answer,
            commands::watch_start,
            commands::watch_stop,
            commands::watch_fullscreen,
            commands::watch_control,
            commands::live_refresh,
        ])
        .build(tauri::generate_context!())
        .expect("error while starting HdrCast")
        .run(|app, event| match event {
            // Quitting from the system (the dock's Quit, Cmd+Q) while in the group or watching: goodbyes first.
            tauri::RunEvent::ExitRequested { code: None, api, .. } => {
                let busy = app.try_state::<Arc<AppState>>().is_some_and(|s| share::is_hosting(&s) || watch::is_active(&s));
                if busy {
                    api.prevent_exit();
                    quit(app);
                }
            }
            // macOS: the dock icon clicked (or the app opened again) while the window is closed to the menu bar.
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { has_visible_windows: false, .. } => window::reveal(app),
            _ => {}
        });
}

/// What the icon in the notification area shows.
fn tray_mode(state: &AppState) -> tray::Mode {
    if share::is_streaming(state) {
        tray::Mode::Sharing
    } else if share::is_hosting(state) {
        tray::Mode::Available
    } else {
        tray::Mode::Idle
    }
}

/// HdrCast was started again while it runs (`instance.rs`): the window comes up, with the invite it was given.
fn started_again(state: &AppState, handle: &AppHandle, args: &[String]) {
    state.logs.tagged("app", "HdrCast was started again: showing the window");
    if args.first().is_some_and(|a| a.to_ascii_lowercase().starts_with("hdrcast:")) {
        if let Some(offer) = commands::invite_offer(state, &args.join(" ")) {
            let _ = handle.emit("join-invite", offer);
        }
    }
    window::reveal(handle);
}

/// Leaves for good (Quit in the tray menu, or closing the window while in the group or watching when HdrCast
/// does not stay available): says goodbye to the viewers and the signaling server first, which takes a few
/// seconds, with the window and the icon already gone.
pub fn quit(app: &AppHandle) {
    static QUITTING: AtomicBool = AtomicBool::new(false);
    if QUITTING.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(window) = app.get_webview_window(window::LABEL) {
        let _ = window.hide();
    }
    tray::remove(app);
    let Some(state) = app.try_state::<Arc<AppState>>().map(|s| s.inner().clone()) else {
        app.exit(0);
        return;
    };
    state.logs.tagged("app", "quitting");
    let handle = app.clone();
    std::thread::spawn(move || {
        watch::stop(&state);
        share::shut_down(&state);
        handle.exit(0);
    });
}
