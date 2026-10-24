//! What the page can ask for. Anything that may take a moment (a process to stop, a file dialog, Windows'
//! permission prompt) is an `async` command, which Tauri runs off the main thread.

use std::sync::atomic::{AtomicBool, Ordering};
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
use tauri::{AppHandle, Emitter, State};

use crate::access::AccessSettings;
use crate::state::{self, AppState, UiSettings};
use crate::{autostart, clipboard, status, tray, window};

type Shared<'a> = State<'a, Arc<AppState>>;
type CmdResult<T = Value> = Result<T, String>;

#[tauri::command]
pub fn app_init(app: State<'_, Arc<AppState>>) -> Value {
    let startup = app.startup_invite.lock().unwrap().take();
    let start_hidden = app.start_hidden.swap(false, Ordering::SeqCst);
    let status = status::snapshot(&app);
    let autostart = autostart::read().view();
    let inner = app.inner.lock().unwrap();
    let platform = app.platform;
    let group = inner.group.current();
    let startup_invite = startup.and_then(|text| offer_of(group, &text));
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "platform": state::platform_name(platform),
        "backdrop": app.backdrop,
        // started at sign-in: the page leaves the window hidden
        "startHidden": start_hidden,
        "autostart": autostart,
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
        "latencyPresets": latency_presets(),
        "toneMappings": ReceiverSettings::TONE_MAPPING_ALGORITHMS,
        "hwDecModes": hw_dec_modes(platform),
        "tools": state::tools_view(&inner.sender, &inner.receiver),
        "firewall": state::firewall_view(inner.firewall),
        "access": state::access_view(&inner.access, inner.access_rev),
        "device": app.device.code(),
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

fn latency_presets() -> Value {
    ReceiverSettings::LATENCY_PRESETS.iter().map(|p| json!({ "name": p.name, "latencyMs": p.latency_ms })).collect()
}

fn hw_dec_modes(platform: tool_locator::Platform) -> Value {
    HwDecMode::ALL.iter().map(|m| json!({ "id": m.number(), "name": m.display_name(platform) })).collect()
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
    let sender_changed = sender != inner.sender;
    let receiver_changed = receiver != inner.receiver;
    let hw_dec_changed = receiver.hw_dec != inner.receiver.hw_dec;
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
    let theme = inner.ui.theme.clone();
    let receiver = inner.receiver.clone();
    drop(inner);
    if theme_changed {
        window::apply_theme(&handle, &theme);
    }
    if sender_changed {
        crate::share::settings_changed(&app);
    }
    if receiver_changed {
        // the panel over the player shows them too
        let _ = handle.emit_to(crate::overlay::LABEL, "receiver", &receiver);
    }
    if hw_dec_changed {
        crate::watch::set_hwdec(&app, receiver.hw_dec);
    }
    Ok(reply)
}

/// Changes the receiver settings from elsewhere than the settings page (the panel over the player), saves them and
/// tells both pages.
fn edit_receiver(app: &AppState, handle: &AppHandle, edit: impl FnOnce(&mut ReceiverSettings)) -> ReceiverSettings {
    let mut inner = app.inner.lock().unwrap();
    let mut receiver = inner.receiver.clone();
    edit(&mut receiver);
    let receiver = receiver.normalized();
    if receiver != inner.receiver {
        if let Err(e) = receiver.save(&ReceiverSettings::default_path()) {
            app.log(&format!("[app] could not save settings: {e}"));
        }
        inner.receiver = receiver.clone();
    }
    drop(inner);
    let _ = handle.emit("receiver", &receiver);
    receiver
}

// ---- starting at sign-in, the notification area ------------------------------------------------------------------

/// What the system will do at the next sign-in, read from the system (Task Manager may have changed it).
#[tauri::command(async)]
pub fn autostart_get() -> Value {
    autostart::read().view()
}

#[tauri::command(async)]
pub fn autostart_set(app: Shared<'_>, on: bool) -> CmdResult {
    let now = autostart::set(on).inspect_err(|e| app.log(&format!("[app] {e}")))?;
    app.log(if now.on {
        "[app] HdrCast will start when you sign in (in the background)"
    } else {
        "[app] HdrCast will no longer start when you sign in"
    });
    Ok(now.view())
}

/// The words of the icon's menu and tooltip, in the page's language.
#[tauri::command(async)]
pub fn tray_texts(handle: AppHandle, texts: tray::Texts) {
    tray::set_texts(&handle, texts);
}

// ---- who may connect ---------------------------------------------------------------------------------------------

#[tauri::command(async)]
pub fn access_get(app: Shared<'_>) -> Value {
    let inner = app.inner.lock().unwrap();
    state::access_view(&inner.access, inner.access_rev)
}

/// Takes the page's rules, repairs them, saves `access.json`, and applies them at once (availability, the
/// remote control of who is watching, requests waiting for an answer). Hands back what was kept.
#[tauri::command(async)]
pub fn access_save(app: Shared<'_>, access: AccessSettings) -> CmdResult {
    let access = access.normalized();
    let view = {
        let mut inner = app.inner.lock().unwrap();
        if access != inner.access {
            if let Err(e) = access.save() {
                app.log(&format!("[app] could not save the access rules: {e}"));
                return Err(format!("Could not save: {e}"));
            }
            inner.access = access;
        }
        state::access_view(&inner.access, inner.access_rev)
    };
    crate::share::rules_changed(app.inner());
    Ok(view)
}

/// The answer to somebody who asks to watch: `remember` lets that device in from now on without asking.
#[tauri::command(async)]
pub fn knock_answer(app: Shared<'_>, sid: String, allow: bool, remember: bool) -> CmdResult {
    crate::share::answer_knock(app.inner(), &sid, allow, remember)?;
    let inner = app.inner.lock().unwrap();
    Ok(state::access_view(&inner.access, inner.access_rev))
}

/// Disconnects one viewer.
#[tauri::command(async)]
pub fn share_kick(app: Shared<'_>, sid: String) -> CmdResult<()> {
    crate::share::kick(&app, &sid)
}

/// Asks the host being watched for remote control, or gives it back.
#[tauri::command(async)]
pub fn watch_control(app: Shared<'_>, want: bool) -> CmdResult<()> {
    crate::watch::want_control(&app, want)
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
        _ => "ffmpeg was not found.\n\nInstall it with Homebrew (brew install ffmpeg-full), or choose it in Settings. \
              It needs libsrt and, for sharing, the VideoToolbox HEVC encoder: Homebrew's ffmpeg-full has both, \
              its plain ffmpeg has no libsrt."
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
              Homebrew's mpv has no libsrt, so on a Mac it plays through ffmpeg (Player input \"Automatic\", the \
              default), which needs an ffmpeg with libsrt (brew install ffmpeg-full)."
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
    crate::share::group_changed(app);
    Ok(view)
}

/// Being available to the group does not count: that moves to the new group by itself.
fn busy(app: &AppState) -> Result<(), String> {
    if crate::share::is_streaming(app) || crate::watch::is_active(app) {
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

/// An invite HdrCast was started with, as the join sheet takes it: None when it is no invite or one of the
/// group this computer is in already.
fn offer_of(current: &hdrcast_core::group::config::GroupConfig, text: &str) -> Option<Value> {
    Some(preview(current, text)).filter(|p| p["ok"] == true && p["alreadyMember"] == false)
}

/// The same for an invite a later start of HdrCast handed over (`instance.rs`).
pub fn invite_offer(app: &AppState, text: &str) -> Option<Value> {
    offer_of(&app.group(), text)
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
    outcome(crate::share::go_live(app.inner()))
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

/// The decoder, from the panel over the player: saved, and switched in the running player at once.
#[tauri::command(async)]
pub fn watch_set_hwdec(app: Shared<'_>, handle: AppHandle, hw_dec: i64) -> CmdResult {
    let mode = HwDecMode::from_number(hw_dec).ok_or("There is no such decoder.")?;
    let receiver = edit_receiver(&app, &handle, |r| r.hw_dec = mode);
    crate::watch::set_hwdec(&app, mode);
    Ok(json!(receiver))
}

/// The delay, from the panel over the player: saved, and taken by joining the host again.
#[tauri::command(async)]
pub fn watch_set_latency(app: Shared<'_>, handle: AppHandle, latency_ms: i32) -> Value {
    edit_receiver(&app, &handle, |r| r.latency_ms = latency_ms);
    refresh_firewall(&app);
    outcome(crate::watch::rejoin(app.inner()))
}

// ---- the panel over the player ------------------------------------------------------------------------------------

/// The sound, from the panel over the player: off or on at once (and for the player a new delay starts).
#[tauri::command(async)]
pub fn watch_set_mute(app: Shared<'_>, muted: bool) {
    crate::watch::set_mute(&app, muted);
}

// ---- the panel over the player ------------------------------------------------------------------------------------
// Async, all of them: laying the panel out waits for the main thread.

/// What the panel needs to draw itself; the rest comes with every status.
#[tauri::command(async)]
pub fn overlay_init(app: Shared<'_>) -> Value {
    let status = status::snapshot(&app);
    let inner = app.inner.lock().unwrap();
    json!({
        "platform": state::platform_name(app.platform),
        "ui": inner.ui,
        "receiver": inner.receiver,
        "latencyPresets": latency_presets(),
        "hwDecModes": hw_dec_modes(app.platform),
        "expanded": crate::overlay::is_expanded(&app),
        "status": status,
    })
}

/// The panel's button on the Watch page: opens or folds it.
#[tauri::command(async)]
pub fn overlay_toggle(app: Shared<'_>) {
    crate::overlay::toggle(&app);
}

/// The round button opens the panel; the panel's fold button and Esc fold it, and the keyboard goes back to the
/// player then.
#[tauri::command(async)]
pub fn overlay_expand(app: Shared<'_>, expanded: bool) {
    crate::overlay::set_expanded(&app, expanded);
    if !expanded {
        crate::overlay::focus_player(&app);
    }
}

/// The panel's page measured itself (CSS pixels).
#[tauri::command(async)]
pub fn overlay_fit(app: Shared<'_>, height: f64) {
    crate::overlay::fit(&app, height);
}

/// Asks the signaling server for the live list now instead of at the next 10 s tick.
#[tauri::command(async)]
pub fn live_refresh(app: Shared<'_>) {
    crate::live::restart(app.inner());
}
