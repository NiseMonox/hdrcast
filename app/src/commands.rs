//! What the page can ask for. Anything that may take a moment (a process to stop, a file dialog, Windows'
//! permission prompt) is an `async` command, which Tauri runs off the main thread.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use hdrcast_core::common::firewall::{self, row as firewall_row};
use hdrcast_core::common::tool_locator;
use hdrcast_core::group::invite;
use hdrcast_core::group::invite_flow::{self, ImportDecision};
use hdrcast_media::receiver::settings::{HwDecMode, ReceiverSettings};
use hdrcast_media::sender::settings::{HdrMode, SenderSettings};
use hdrcast_media::sender::{encoder_probe, encoder_profiles};
use serde::Deserialize;
use serde_json::{json, Value};
use tauri::{AppHandle, State};

use crate::state::{self, AppState, UiSettings};
use crate::{clipboard, status, window};

type Shared<'a> = State<'a, Arc<AppState>>;
type CmdResult<T = Value> = Result<T, String>;

#[tauri::command]
pub fn app_init(app: State<'_, Arc<AppState>>) -> Value {
    let startup = app.startup_invite.lock().unwrap().take();
    let status = status::snapshot(&app);
    let inner = app.inner.lock().unwrap();
    let platform = app.platform;
    let group = inner.group.current();
    let startup_invite = startup.map(|text| preview(group, &text)).filter(|p| p["ok"] == true && p["alreadyMember"] == false);
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "platform": state::platform_name(platform),
        "backdrop": app.backdrop,
        "defaultName": invite_flow::default_display_name(group),
        "ui": inner.ui,
        "sender": inner.sender,
        "receiver": inner.receiver,
        "group": state::group_view(group),
        "outputs": state::outputs_view(&inner.outputs),
        "audioDevices": audio_devices(),
        "encoders": encoder_profiles::for_platform(platform).iter()
            .map(|e| json!({ "id": e.id.number(), "name": e.display_name })).collect::<Vec<_>>(),
        "audioCodecs": [{ "id": 1, "name": "AAC" }, { "id": 2, "name": "Opus" }],
        "latencyPresets": ReceiverSettings::LATENCY_PRESETS.iter()
            .map(|p| json!({ "name": p.name, "latencyMs": p.latency_ms })).collect::<Vec<_>>(),
        "toneMappings": ReceiverSettings::TONE_MAPPING_ALGORITHMS,
        "hwDecModes": HwDecMode::ALL.iter()
            .map(|m| json!({ "id": m.number(), "name": m.display_name(platform) })).collect::<Vec<_>>(),
        "tools": state::tools_view(&inner.sender, &inner.receiver),
        "firewall": state::firewall_view(inner.firewall),
        "paths": { "config": hdrcast_core::common::app_paths::config_dir().to_string_lossy() },
        "caps": {
            "audioExclude": cfg!(windows),
            "clipboardWatch": cfg!(windows),
        },
        "logs": app.logs.history(),
        "status": status,
        "startupInvite": startup_invite,
    })
}

fn audio_devices() -> Value {
    Value::Array(
        hdrcast_capture::list_audio_devices().into_iter().map(|d| json!({ "id": d.id, "name": d.name })).collect(),
    )
}

#[derive(Deserialize)]
pub struct AllSettings {
    ui: UiSettings,
    sender: SenderSettings,
    receiver: ReceiverSettings,
}

/// Takes the page's settings, repairs what is out of range, saves the three files and hands back what was kept.
#[tauri::command(async)]
pub fn settings_save(app: Shared<'_>, handle: AppHandle, settings: AllSettings) -> CmdResult {
    let ui = settings.ui.normalized();
    let mut sender = settings.sender.normalized();
    let receiver = settings.receiver.normalized();
    let mut inner = app.inner.lock().unwrap();
    state::adopt_output(&mut sender, &inner.outputs);
    let theme_changed = ui.theme != inner.ui.theme;
    let policy_changed = ui.remote_control != inner.ui.remote_control;
    if ui != inner.ui {
        if let Err(e) = ui.save() {
            app.log(&format!("[app] could not save settings: {e}"));
        }
        inner.ui = ui;
    }
    if sender != inner.sender {
        if let Err(e) = sender.save(&SenderSettings::default_path()) {
            app.log(&format!("[app] could not save settings: {e}"));
        }
        inner.sender = sender;
    }
    if receiver != inner.receiver {
        if let Err(e) = receiver.save(&ReceiverSettings::default_path()) {
            app.log(&format!("[app] could not save settings: {e}"));
        }
        inner.receiver = receiver;
    }
    let reply = json!({
        "ui": inner.ui,
        "sender": inner.sender,
        "receiver": inner.receiver,
        "tools": state::tools_view(&inner.sender, &inner.receiver),
    });
    let (theme, policy) = (inner.ui.theme.clone(), inner.ui.remote_control);
    drop(inner);
    if theme_changed {
        window::apply_theme(&handle, &theme);
    }
    if policy_changed {
        crate::share::policy_changed(&app, policy);
    }
    Ok(reply)
}

#[tauri::command(async)]
pub fn outputs_refresh(app: Shared<'_>) -> CmdResult {
    let outputs = hdrcast_capture::enumerate_outputs().map_err(|e| format!("Cannot list the displays: {e}"))?;
    let mut inner = app.inner.lock().unwrap();
    app.log(&format!("[app] found {} display(s)", outputs.len()));
    inner.outputs = outputs;
    let outputs = inner.outputs.clone();
    state::adopt_output(&mut inner.sender, &outputs);
    Ok(state::outputs_view(&inner.outputs))
}

/// The programs that have audio right now, for the "leave out" box.
#[tauri::command(async)]
pub fn audio_apps(app: Shared<'_>) -> Vec<String> {
    let device = app.inner.lock().unwrap().sender.audio_device.clone();
    hdrcast_capture::list_audio_apps(&device)
}

/// Trial-encodes with every encoder this platform offers (up to a minute) and names the best one that works.
#[tauri::command(async)]
pub fn encoders_probe(app: Shared<'_>) -> CmdResult {
    let (ffmpeg, hdr) = {
        let inner = app.inner.lock().unwrap();
        let hdr = state::selected_output(&inner.sender, &inner.outputs).is_some_and(|o| o.hdr)
            && inner.sender.hdr_mode == HdrMode::Auto;
        (state::ffmpeg_path(&inner.sender), hdr)
    };
    let Some(ffmpeg) = ffmpeg else {
        return Ok(json!({ "recommended": null, "problems": [ffmpeg_missing(app.platform)] }));
    };
    app.log(&format!(
        "[probe] testing encoders with {}{}",
        ffmpeg.display(),
        if hdr { " (10-bit HDR)" } else { " (8-bit SDR)" }
    ));
    let logs = app.logs.clone();
    let results = encoder_probe::probe_all(
        &ffmpeg,
        hdr,
        app.platform,
        |r| {
            let name = encoder_profiles::info(r.encoder).ffmpeg_name;
            logs.write(&format!("[probe] {name}: {}", if r.works { "ok" } else { r.message.as_str() }));
        },
        &AtomicBool::new(false),
    );
    let best = encoder_probe::pick_best(&results, app.platform);
    match best {
        Some(encoder) => app.log(&format!("[probe] recommended: {}", encoder_profiles::info(encoder).display_name)),
        None => app.log("[probe] no working encoder found — check the ffmpeg build and GPU drivers"),
    }
    Ok(json!({ "recommended": best.map(|e| e.number()), "results": results }))
}

pub fn ffmpeg_missing(platform: tool_locator::Platform) -> String {
    match platform {
        tool_locator::Platform::Windows => "ffmpeg.exe was not found.\n\n\
            Easiest fix: run scripts\\get-tools.cmd (double-click it). It downloads a build with NVENC/AMF/QSV \
            and libsrt into the tools\\ folder, then checks it.\n\n\
            By hand: take the gyan.dev \"full\" or BtbN win64-gpl build and put ffmpeg.exe next to HdrCast.exe \
            (or in tools\\), or choose it in Settings."
            .to_string(),
        _ => "ffmpeg was not found.\n\nInstall it with Homebrew (brew install ffmpeg), or choose it in Settings. \
              It needs libsrt and, for sharing, the VideoToolbox HEVC encoder (Homebrew's build has both)."
            .to_string(),
    }
}

pub fn mpv_missing(platform: tool_locator::Platform) -> String {
    match platform {
        tool_locator::Platform::Windows => "mpv.exe was not found.\n\n\
            Easiest fix: run scripts\\get-tools.cmd viewer (double-click get-tools.cmd). It downloads an mpv \
            build with libsrt into the tools\\ folder, then checks it.\n\n\
            By hand: take a shinchiro or zhongfly mpv-winbuild and put mpv.exe next to HdrCast.exe \
            (or in tools\\), or choose it in Settings."
            .to_string(),
        _ => "mpv was not found.\n\nInstall it with Homebrew (brew install mpv), or choose it in Settings. \
              It needs libsrt (Homebrew's build has it)."
            .to_string(),
    }
}

// ---- group and invites --------------------------------------------------------------------------------------------

fn save_group(app: &Arc<AppState>, config: hdrcast_core::group::config::GroupConfig) -> CmdResult {
    let mut inner = app.inner.lock().unwrap();
    let line = format!("[app] group: \"{}\" as {}", config.group_name, config.display_name);
    match inner.group.save(config) {
        Ok(()) => app.log(&line),
        Err(e) => {
            app.log(&format!("[app] could not save the group: {e}"));
            return Err(format!("Could not save the group: {e}"));
        }
    }
    let view = state::group_view(inner.group.current());
    drop(inner);
    crate::live::group_changed(app);
    Ok(view)
}

fn busy(app: &AppState) -> Result<(), String> {
    if crate::share::is_active(app) || crate::watch::is_active(app) {
        Err("Stop sharing and watching first.".to_string())
    } else {
        Ok(())
    }
}

#[tauri::command(async)]
pub fn group_create(app: Shared<'_>, group_name: String, display_name: String) -> CmdResult {
    busy(&app)?;
    let current = app.group();
    let config = invite_flow::create_group(&current, &group_name, &display_name).map_err(|e| e.to_string())?;
    save_group(app.inner(), config)
}

#[tauri::command(async)]
pub fn group_rename(app: Shared<'_>, display_name: String) -> CmdResult {
    busy(&app)?;
    let current = app.group();
    let invite = current.try_get_invite().ok_or(invite_flow::NO_GROUP_YET)?;
    save_group(app.inner(), current.adopt(&invite, &display_name))
}

#[tauri::command(async)]
pub fn group_detect_clipboard(app: Shared<'_>, on: bool) -> CmdResult {
    let mut config = app.group();
    config.detect_clipboard_invites = on;
    save_group(app.inner(), config)
}

/// What importing `text` would do, as the page's join sheet needs it.
fn preview(current: &hdrcast_core::group::config::GroupConfig, text: &str) -> Value {
    match invite_flow::decide_import(current, text) {
        ImportDecision::Invalid { message, .. } => json!({ "ok": false, "error": message }),
        ImportDecision::AlreadyMember { invite, .. } => json!({
            "ok": true, "alreadyMember": true, "groupName": invite.group_name(), "text": text,
        }),
        ImportDecision::Join { invite, name, .. } => json!({
            "ok": true,
            "alreadyMember": false,
            "groupName": invite.group_name(),
            "currentGroup": if current.is_configured() { Value::from(current.group_name.clone()) } else { Value::Null },
            "defaultName": name.initial,
            "fingerprint": invite.fingerprint(),
            "text": text,
        }),
    }
}

/// `text` = what the user pasted; without it, the clipboard is looked at.
#[tauri::command(async)]
pub fn invite_preview(app: Shared<'_>, text: Option<String>) -> CmdResult {
    let text = match text {
        Some(t) => t,
        None => {
            let clip = clipboard::read_text();
            if !invite_flow::clipboard_holds_invite(&clip) {
                return Ok(json!({ "ok": false, "error": "The clipboard holds no invite." }));
            }
            invite_flow::clip_clipboard_text(&clip).to_string()
        }
    };
    Ok(preview(&app.group(), &text))
}

#[tauri::command(async)]
pub fn invite_join(app: Shared<'_>, text: String, display_name: String) -> CmdResult {
    busy(&app)?;
    let current = app.group();
    let invite = invite::try_extract(&text).map_err(|e| e.describe())?;
    if display_name.trim().is_empty() {
        return Err("Enter your name.".to_string());
    }
    let result = save_group(app.inner(), invite_flow::join(&current, &invite, &display_name));
    app.inner.lock().unwrap().clipboard.finished();
    result
}

/// "No" to an invite found on the clipboard: do not offer that one again.
#[tauri::command(async)]
pub fn invite_dismiss(app: Shared<'_>, fingerprint: String) -> CmdResult {
    let mut config = app.group();
    config.dismissed_fingerprint = fingerprint;
    let result = save_group(app.inner(), config);
    app.inner.lock().unwrap().clipboard.finished();
    result
}

/// The message people paste into WeChat / LINE / Discord: a line of explanation plus the code.
fn invite_message(invite: &invite::Invite) -> String {
    format!(
        "HdrCast invite to \"{}\". Copy this whole message, then open HdrCast and it will offer to join. \
         Keep it among friends: anyone with it can watch.\r\n{}",
        invite.group_name(),
        invite::encode(invite)
    )
}

#[tauri::command(async)]
pub fn invite_copy(app: Shared<'_>) -> CmdResult<()> {
    let invite = app.group().try_get_invite().ok_or(invite_flow::NO_GROUP_YET)?;
    clipboard::write_text(&invite_message(&invite)).map_err(|e| invite_flow::clipboard_failed(&e))
}

#[tauri::command(async)]
pub fn clipboard_write(text: String) -> CmdResult<()> {
    clipboard::write_text(&text).map_err(|e| invite_flow::clipboard_failed(&e))
}

/// The window came to the front: a friend may just have copied an invite in a chat and switched here.
#[tauri::command(async)]
pub fn app_focused(app: Shared<'_>, handle: AppHandle) {
    use tauri::Emitter;
    refresh_firewall(&app);
    let Some(sequence) = clipboard::sequence_number() else { return };
    let mut inner = app.inner.lock().unwrap();
    if inner.group.refresh_if_changed() {
        app.log("[app] group.json changed on disk: reloaded");
    }
    let group = inner.group.current().clone();
    let offer = inner.clipboard.check(&group, sequence, clipboard::read_text);
    drop(inner);
    if let Some(offer) = offer {
        let _ = handle.emit("invite", preview(&group, &offer.text));
    }
}

// ---- firewall, tools, folders -------------------------------------------------------------------------------------

/// Adds the Windows Firewall rule (Windows asks for administrator permission) and reports what holds now.
#[tauri::command(async)]
pub fn firewall_allow(app: Shared<'_>) -> CmdResult {
    let exe = state::current_exe();
    let (result, verdict) = firewall_row::allow_now(&exe);
    app.log(&firewall_row::allow_log_line(&exe, &result));
    app.inner.lock().unwrap().firewall = verdict;
    match result {
        Ok(()) => Ok(state::firewall_view(verdict)),
        Err(e) => Err(e.to_string()),
    }
}

/// Looks at Windows Firewall again (rules change behind the app's back: its own prompt, other tools).
pub fn refresh_firewall(app: &AppState) {
    let verdict = firewall::check(&state::current_exe());
    app.inner.lock().unwrap().firewall = verdict;
}

#[tauri::command]
pub async fn tool_browse(kind: String) -> CmdResult<Option<String>> {
    let exe = tool_locator::exe_name(if kind == "mpv" { tool_locator::MPV } else { tool_locator::FFMPEG });
    let mut dialog = rfd::AsyncFileDialog::new().set_title(format!("Choose {exe}"));
    if cfg!(windows) {
        dialog = dialog.add_filter(&exe, &["exe"]);
    }
    Ok(dialog.pick_file().await.map(|f| f.path().to_string_lossy().into_owned()))
}

/// Opens the folder with the settings and the logs in the system's file manager.
#[tauri::command(async)]
pub fn logs_open() -> CmdResult<()> {
    let dir = hdrcast_core::common::app_paths::logs_dir();
    hdrcast_core::common::app_paths::ensure_dir(&dir);
    let program = if cfg!(windows) {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program)
        .arg(&dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Could not open {}: {e}", dir.display()))
}

// ---- sharing and watching --------------------------------------------------------------------------------------

fn outcome(result: Result<(), Vec<String>>) -> Value {
    match result {
        Ok(()) => json!({ "ok": true }),
        Err(problems) => json!({ "ok": false, "problems": problems }),
    }
}

#[tauri::command(async)]
pub fn share_start(app: Shared<'_>) -> Value {
    refresh_firewall(&app);
    outcome(crate::share::start(app.inner()))
}

#[tauri::command(async)]
pub fn share_stop(app: Shared<'_>) {
    crate::share::stop(&app);
}

/// Answers a viewer's request for remote control, or takes control back.
#[tauri::command(async)]
pub fn control_set(app: Shared<'_>, sid: String, allow: bool) -> CmdResult<()> {
    crate::share::control_set(&app, &sid, allow)
}

#[tauri::command(async)]
pub fn watch_start(app: Shared<'_>, host_id: String) -> Value {
    refresh_firewall(&app);
    outcome(crate::watch::start(app.inner(), &host_id))
}

#[tauri::command(async)]
pub fn watch_stop(app: Shared<'_>) {
    crate::watch::stop(&app);
}

#[tauri::command(async)]
pub fn watch_fullscreen(app: Shared<'_>) {
    crate::watch::toggle_fullscreen(&app);
}

/// Asks the signaling server for the live list now instead of at the next 10 s tick.
#[tauri::command(async)]
pub fn live_refresh(app: Shared<'_>) {
    crate::live::restart(app.inner());
}
