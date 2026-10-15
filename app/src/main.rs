#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clipboard;
mod commands;
mod control_host;
mod control_viewer;
mod ipc_stream;
mod live;
mod logs;
mod mpv_ipc;
mod mpv_runner;
mod sender_runner;
mod share;
mod state;
mod status;
mod watch;
mod window;

use std::sync::Arc;
use std::time::Duration;

use hdrcast_core::common::firewall::{self, row as firewall_row};
use tauri::{Emitter, Manager};

use crate::logs::Logs;
use crate::state::AppState;

/// One executable, several modes (docs/design.md §2.1):
///   HdrCast                      the app
///   HdrCast hdrcast:…            the app, offering to join that invite
///   HdrCast capture <options>    the capture engine the app runs as a child (also list-outputs, audio-devices)
///   HdrCast firewall-allow       adds the Windows Firewall rule; started elevated by the app's Allow button
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
    let invite = args.first().filter(|a| a.to_ascii_lowercase().starts_with("hdrcast:")).map(|_| args.join(" "));
    run_app(invite);
}

fn run_app(startup_invite: Option<String>) {
    let logs = Arc::new(Logs::default());
    logs.tagged("app", &format!("HdrCast {} starting", env!("CARGO_PKG_VERSION")));
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
            let (_, backdrop) = window::create(&handle)?;
            let state = Arc::new(AppState::new(setup_logs.clone(), backdrop, startup_invite.clone()));
            window::apply_theme(&handle, &state.inner.lock().unwrap().ui.theme);
            app.manage(state.clone());
            setup_logs.tagged("app", &format!("settings: {}", hdrcast_core::common::app_paths::config_dir().display()));
            live::restart(&state);

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
                        let _ = handle.emit("status", status::snapshot(&state));
                    }
                    // While live or watching, the firewall verdict is part of what peers are told: keep it fresh.
                    if ticks % 75 == 0 && (share::is_active(&state) || watch::is_active(&state)) {
                        commands::refresh_firewall(&state);
                    }
                }
            })?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing while live or watching: say goodbye to the viewers and the signaling server first (a few
            // seconds), with the window already out of sight.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<Arc<AppState>>().inner().clone();
                if share::is_active(&state) || watch::is_active(&state) {
                    api.prevent_close();
                    let _ = window.hide();
                    let handle = window.app_handle().clone();
                    std::thread::spawn(move || {
                        watch::stop(&state);
                        share::stop(&state);
                        handle.exit(0);
                    });
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::app_init,
            commands::settings_save,
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
            commands::control_set,
            commands::watch_start,
            commands::watch_stop,
            commands::watch_fullscreen,
            commands::live_refresh,
        ])
        .run(tauri::generate_context!())
        .expect("error while running HdrCast");
}
