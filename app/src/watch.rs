//! Watching a friend's stream: a [`ViewerSession`] keeps a punched path to the host alive, mpv plays from the
//! local end of it, and (when asked for) the player window's mouse and keyboard go to the host.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use hdrcast_core::common::srt_buffer_sizing;
use hdrcast_core::group::keys::GroupKeys;
use hdrcast_media::receiver::mpv_command;
use hdrcast_media::receiver::settings::{ReceiverFrontend, ReceiverSettings};
use hdrcast_p2p::{ViewerSession, ViewerSessionOptions, ViewerSessionState};
use serde_json::{json, Value};

use crate::commands;
use crate::control_viewer::ViewerControlLink;
use crate::mpv_runner::MpvRunner;
use crate::state::{self, AppState};

struct Session {
    viewer: ViewerSession,
    runner: MpvRunner,
    control: Option<Arc<ViewerControlLink>>,
    keys: GroupKeys,
    settings: ReceiverSettings,
    host_id: String,
    /// The host's name from the live list, until the session has learned it itself.
    host_name: String,
    /// mpv is started once, when the path first works; after that its own retry loop rides out reconnects.
    mpv_started: AtomicBool,
}

#[derive(Default)]
pub struct Watch {
    session: Mutex<Option<Arc<Session>>>,
}

fn current(app: &AppState) -> Option<Arc<Session>> {
    app.watch.session.lock().unwrap().clone()
}

pub fn is_active(app: &AppState) -> bool {
    current(app).is_some()
}

/// Starts watching `host_id`. The error is the list of things to fix first, as sentences.
pub fn start(app: &Arc<AppState>, host_id: &str) -> Result<(), Vec<String>> {
    if is_active(app) {
        return Err(vec!["Already watching.".to_string()]);
    }
    let (group, settings, request_control) = {
        let inner = app.inner.lock().unwrap();
        (inner.group.current().clone(), inner.receiver.clone(), inner.ui.request_control)
    };
    let mut problems = settings.validate();
    if !group.is_configured() {
        problems.push("Paste the invite a friend sent you first.".to_string());
    }
    if state::mpv_path(&settings).is_none() {
        problems.push(commands::mpv_missing(app.platform));
    }
    if settings.frontend == ReceiverFrontend::FfmpegPipe && receiver_ffmpeg(&settings).is_none() {
        problems.push("ffmpeg was not found. The ffmpeg pipe frontend needs ffmpeg with libsrt.".to_string());
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let keys = group.keys().map_err(|_| vec!["There is no group yet.".to_string()])?;
    let host_name = crate::live::name_of(app, host_id).unwrap_or_default();

    let viewer = ViewerSession::new(
        ViewerSessionOptions {
            group,
            host_id: host_id.to_string(),
            latency_ms: settings.latency_ms,
            overhead_percent: settings.overhead_percent() as i32,
            request_control,
            map_router_port: state::router_mapping_wanted(),
            ..ViewerSessionOptions::default()
        },
        app.log_fn(),
    );
    let runner = MpvRunner::new(app.job.clone(), app.logger());
    let session = Arc::new_cyclic(|weak: &Weak<Session>| {
        let control = request_control.then(|| {
            let sender = weak.clone();
            ViewerControlLink::start(
                Arc::new(move |payload| sender.upgrade().is_some_and(|s| s.viewer.send_side(payload))),
                app.logger(),
            )
        });
        Session {
            viewer,
            runner,
            control,
            keys,
            settings,
            host_id: host_id.to_string(),
            host_name,
            mpv_started: AtomicBool::new(false),
        }
    });

    if let Some(control) = &session.control {
        let link = control.clone();
        session.viewer.set_side_handler(Arc::new(move |payload| link.on_datagram(payload)));
        let (tap, ready, weak) = (control.clone(), control.clone(), Arc::downgrade(&session));
        session.runner.set_ipc_tap(
            Some(Arc::new(move |msg| tap.on_ipc_message(msg))),
            Some(Arc::new(move || {
                if let Some(ipc) = weak.upgrade().and_then(|s| s.runner.ipc()) {
                    ready.ipc_ready(ipc);
                }
            })),
        );
    }
    let (weak_app, weak_session) = (Arc::downgrade(app), Arc::downgrade(&session));
    session.viewer.on_changed(move || {
        if let (Some(app), Some(session)) = (weak_app.upgrade(), weak_session.upgrade()) {
            session.on_changed(&app);
        }
    });

    let shown = if session.host_name.is_empty() { "the host" } else { session.host_name.as_str() };
    app.log(&format!("[app] watching {shown} (latency {} ms)", session.settings.latency_ms));
    if let Err(e) = session.viewer.start() {
        if let Some(control) = &session.control {
            control.stop();
        }
        return Err(vec![format!("Could not start: {e}")]);
    }
    *app.watch.session.lock().unwrap() = Some(session);
    Ok(())
}

fn receiver_ffmpeg(settings: &ReceiverSettings) -> Option<std::path::PathBuf> {
    use hdrcast_core::common::tool_locator;
    tool_locator::resolve(Some(&settings.ffmpeg_path), &tool_locator::exe_name(tool_locator::FFMPEG))
}

impl Session {
    /// The session's state changed (called on its threads).
    fn on_changed(&self, app: &AppState) {
        let connected = self.viewer.state() == ViewerSessionState::Connected;
        if let Some(control) = &self.control {
            match self.viewer.sid().filter(|_| connected) {
                Some(sid) => {
                    let key = self.viewer.host_offers_control().then(|| self.keys.control_key(&sid));
                    control.session(&sid, key);
                }
                None => control.session_ended(),
            }
        }
        if connected && !self.runner.is_running() && !self.mpv_started.swap(true, Ordering::SeqCst) {
            self.start_player(app);
        }
    }

    fn start_player(&self, app: &AppState) {
        let s = &self.settings;
        let Some(mpv) = state::mpv_path(s) else {
            app.log("[app] mpv was not found");
            return;
        };
        let ffmpeg = receiver_ffmpeg(s);
        let endpoint = mpv_command::default_ipc_endpoint(std::process::id());
        let plan = match mpv_command::build(s, self.viewer.local_port(), &self.viewer.srt_passphrase(), &endpoint, &mpv, ffmpeg.as_deref(), app.platform) {
            Ok(plan) => plan,
            Err(e) => {
                app.log(&format!("[app] cannot build the mpv command: {e}"));
                return;
            }
        };
        if let Ok(sizing) = srt_buffer_sizing::compute(s.latency_ms.max(0) as u32, s.expected_bitrate_mbps.max(0) as u64 * 1_000_000) {
            app.log(&format!(
                "[app] SRT buffer: {} packets in flight -> ffs={} rcvbuf={} bytes",
                sizing.packets, sizing.flow_window, sizing.buffer_bytes
            ));
        }
        self.runner.start(s.clone(), plan);
    }
}

pub fn stop(app: &AppState) {
    let Some(session) = app.watch.session.lock().unwrap().take() else { return };
    if let Some(control) = &session.control {
        control.stop();
    }
    session.runner.set_ipc_tap(None, None);
    session.runner.stop();
    session.viewer.stop();
    app.log("[app] stopped watching");
}

pub fn toggle_fullscreen(app: &AppState) {
    if let Some(session) = current(app) {
        session.runner.toggle_fullscreen();
    }
}

pub fn status(app: &AppState) -> Value {
    let Some(session) = current(app) else {
        return json!({ "active": false, "hostId": "", "hostName": "", "session": null, "player": null, "control": "off" });
    };
    let viewer = &session.viewer;
    let learned = viewer.host_name();
    let state_name = match viewer.state() {
        ViewerSessionState::Starting => "starting",
        ViewerSessionState::Joining => "joining",
        ViewerSessionState::Punching => "punching",
        ViewerSessionState::Connected => "connected",
        ViewerSessionState::Waiting => "waiting",
        ViewerSessionState::Stopped => "stopped",
    };
    let play = session.runner.snapshot();
    let st = &play.status;
    let output = match (&st.target, &st.source) {
        (None, _) => "-".to_string(),
        (Some(t), _) if t.is_hdr() => format!("HDR passthrough active ({})", t.gamma.as_deref().unwrap_or("?")),
        (Some(_), Some(s)) if s.is_hdr() => "SDR output: mpv is tone-mapping the HDR source".to_string(),
        _ => "SDR".to_string(),
    };
    let player = session.runner.is_running().then(|| {
        json!({
            "state": play.state.name(),
            "text": play.text,
            "attempt": play.attempt,
            "uptimeSec": play.uptime.as_secs_f64(),
            "cacheSeconds": st.cache_seconds,
            "cache": st.describe_cache(),
            "drops": st.frame_drops,
            "decoderDrops": st.decoder_frame_drops,
            "fps": st.fps,
            "bitrateMbps": st.video_bitrate_bps.map(|b| b / 1e6),
            "hwdec": st.hw_dec.clone().unwrap_or_default(),
            "source": st.source.as_ref().map(|p| p.describe()).unwrap_or_default(),
            "target": st.target.as_ref().map(|p| p.describe()).unwrap_or_default(),
            "output": output,
            "hdr": st.target.as_ref().is_some_and(|t| t.is_hdr()),
        })
    });
    let control = match &session.control {
        None => "off",
        Some(_) if viewer.state() == ViewerSessionState::Connected && !viewer.host_offers_control() => "unsupported",
        Some(link) => match link.state() {
            "off" => "requested",
            other => other,
        },
    };
    json!({
        "active": true,
        "hostId": session.host_id,
        "hostName": if learned.is_empty() { session.host_name.clone() } else { learned },
        "session": {
            "state": state_name,
            "status": viewer.status(),
            "path": viewer.path_description(),
            "couldHelp": viewer.state() == ViewerSessionState::Waiting && viewer.this_side_could_help(),
        },
        "player": player,
        "control": control,
    })
}
