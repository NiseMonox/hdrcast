//! Watching a friend's stream: a [`ViewerSession`] keeps a punched path to the host alive, mpv plays from the
//! local end of it, and (when the host allows it) the player window's mouse and keyboard go to the host.
//!
//! Every join is signed with this computer's device key, so a host can recognise it and agree on a
//! remote-control key with it (docs/design.md §8.10). Whether remote control is on is the host's rule for this
//! device: allowed means it is taken at once, "ask" means the user can ask for it from the page.
//!
//! A host that takes keyframe requests ("kf") is asked for a fresh keyframe when part of the stream is lost
//! (docs/design.md §8.12).
//!
//! While watching, the panel over the player (`overlay.rs`) changes the decoder and the sound at once and the delay
//! by joining again (the delay is agreed on when joining). Closing the player's window stops watching.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use hdrcast_core::common::srt_buffer_sizing;
use hdrcast_control::keys;
use hdrcast_core::group::keys::GroupKeys;
use hdrcast_media::keyframe;
use hdrcast_media::receiver::mpv_command;
use hdrcast_media::receiver::settings::{HwDecMode, ReceiverFrontend, ReceiverSettings};
use hdrcast_p2p::{LinkAddresses, SideUse, SimulatedLoss, ViewerSession, ViewerSessionOptions, ViewerSessionState};
use serde_json::{json, Value};

use crate::commands;
use crate::control_viewer::ViewerControlLink;
use crate::keyframe_viewer::KeyframeLink;
use crate::mpv_runner::{self, MpvRunner};
use crate::net_report::{self, End, NetReport};
use crate::overlay;
use crate::state::{self, AppState};

/// With this set (`<seconds>[:<packets>]`), the viewer's link loses that many packets of the stream for good every
/// so many seconds: for trying keyframes on request with host and viewer on one PC (docs/design.md §6).
pub const SIMULATE_LOSS_ENV: &str = "HDRCAST_SIMULATE_LOSS";

struct Session {
    viewer: ViewerSession,
    runner: MpvRunner,
    control: Option<Arc<ViewerControlLink>>,
    keyframes: Arc<KeyframeLink>,
    keys: GroupKeys,
    settings: ReceiverSettings,
    host_id: String,
    /// The host's name from the live list, until the session has learned it itself.
    host_name: String,
    /// mpv is started once, when the path first works; after that its own retry loop rides out reconnects.
    mpv_started: AtomicBool,
    net_report: Mutex<NetReport>,
}

#[derive(Default)]
pub struct Watch {
    session: Mutex<Option<Arc<Session>>>,
    /// Muted from the panel over the player: kept when joining again with a new delay, not after Stop.
    muted: AtomicBool,
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
    let (group, settings) = {
        let inner = app.inner.lock().unwrap();
        (inner.group.current().clone(), inner.receiver.clone())
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

    let addresses = std::env::var(SIMULATE_LOSS_ENV).ok().and_then(|spec| {
        let loss = SimulatedLoss::parse(&spec, Some(app.logger()));
        match &loss {
            Some(loss) => app.log(&format!("[test] {SIMULATE_LOSS_ENV}: {} on the way in", loss.describe())),
            None => app.log(&format!("[test] {SIMULATE_LOSS_ENV}={spec} is not <seconds>[:<packets>]: ignored")),
        }
        let loss: Arc<dyn hdrcast_p2p::PacketFilter> = Arc::new(loss?);
        Some(LinkAddresses { filter_v6: Some(loss.clone()), filter_v4: Some(loss), ..LinkAddresses::detect() })
    });
    let viewer = ViewerSession::new(
        ViewerSessionOptions {
            group,
            host_id: host_id.to_string(),
            latency_ms: settings.latency_ms,
            overhead_percent: settings.overhead_percent() as i32,
            // this viewer speaks the side channel: the host's rules say what remote control it gets
            request_control: true,
            // and asks for keyframes when it loses part of the stream (only hosts that take requests)
            request_keyframes: true,
            addresses,
            device: Some(app.device.clone()),
            map_router_port: state::router_mapping_wanted(),
            ..ViewerSessionOptions::default()
        },
        app.log_fn(),
    );
    let runner = MpvRunner::new(app.job.clone(), app.logger());
    let latency = Duration::from_millis(settings.latency_ms.max(0) as u64);
    let session = Arc::new_cyclic(|weak: &Weak<Session>| {
        let control = {
            let sender = weak.clone();
            Some(ViewerControlLink::start(
                Arc::new(move |payload| sender.upgrade().is_some_and(|s| s.viewer.send_side(payload))),
                app.logger(),
            ))
        };
        let sender = weak.clone();
        let keyframes = KeyframeLink::start(
            latency,
            Arc::new(move |payload| sender.upgrade().is_some_and(|s| s.viewer.send_side_for(SideUse::Keyframes, payload))),
            app.logger(),
        );
        Session {
            viewer,
            runner,
            control,
            keyframes,
            keys,
            settings,
            host_id: host_id.to_string(),
            host_name,
            mpv_started: AtomicBool::new(false),
            net_report: Mutex::new(NetReport::new(End::Viewer)),
        }
    });

    {
        // the side channel carries remote control and keyframe answers: they tell themselves apart by their first byte
        let (control, keyframes) = (session.control.clone(), session.keyframes.clone());
        session.viewer.set_side_handler(Arc::new(move |payload| {
            if keyframe::is_keyframe_datagram(payload) {
                keyframes.on_datagram(payload);
            } else if let Some(control) = &control {
                control.on_datagram(payload);
            }
        }));
        let keyframes = session.keyframes.clone();
        session.viewer.on_lasting_loss(Arc::new(move |missing| keyframes.lasting_loss(missing)));
        let keyframes = session.keyframes.clone();
        session.runner.set_guard_tap(Some(Arc::new(move |event| keyframes.guard_event(event))));
    }
    if let Some(control) = &session.control {
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
    let weak_app = Arc::downgrade(app);
    session.runner.on_player_message(Some(Arc::new(move |message| {
        let Some(app) = weak_app.upgrade() else { return };
        if message == keys::OVERLAY_MESSAGE {
            overlay::toggle(&app);
        } else if message == keys::CLOSE_MESSAGE {
            // not on this thread: stopping waits for the player, whose messages this thread reads
            std::thread::spawn(move || {
                app.log("[app] the player window was closed");
                stop(&app);
            });
        }
    })));
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
        session.keyframes.stop();
        return Err(vec![format!("Could not start: {e}")]);
    }
    *app.watch.session.lock().unwrap() = Some(session);
    // the button over the player (it shows once the player's window is there)
    overlay::start(app);
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
        match self.viewer.sid().filter(|_| connected && self.viewer.host_takes_keyframes()) {
            Some(sid) => self.keyframes.session(&sid, self.keys.keyframe_key(&sid)),
            None => self.keyframes.session_ended(),
        }
        if let Some(control) = &self.control {
            match self.viewer.sid().filter(|_| connected) {
                Some(sid) => {
                    // the key agreed with the host's device when it has one (0.3.1 on), else the group's
                    let key = self
                        .viewer
                        .host_offers_control()
                        .then(|| self.viewer.control_key(&sid).unwrap_or_else(|| self.keys.control_key(&sid)));
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
        // the decoder as it is set now: the panel may have changed it while the session was joining
        let s = &ReceiverSettings { hw_dec: app.inner.lock().unwrap().receiver.hw_dec, ..self.settings.clone() };
        let Some(mpv) = state::mpv_path(s) else {
            app.log("[app] mpv was not found");
            return;
        };
        let ffmpeg = receiver_ffmpeg(s);
        let endpoint = mpv_command::default_ipc_endpoint(std::process::id());
        let mut plan = match mpv_command::build(s, self.viewer.local_port(), &self.viewer.srt_passphrase(), &endpoint, &mpv, ffmpeg.as_deref(), app.platform) {
            Ok(plan) => plan,
            Err(e) => {
                app.log(&format!("[app] cannot build the mpv command: {e}"));
                return;
            }
        };
        if app.watch.muted.load(Ordering::SeqCst) {
            mpv_runner::set_option(&mut plan, "mute", "yes");
        }
        app.log(if plan.uses_ffmpeg_pipe() {
            "[app] ffmpeg receives the stream and HdrCast passes it on to mpv: pictures damaged on the way are held back"
        } else if s.frontend == ReceiverFrontend::MpvDirect {
            "[app] mpv receives the stream itself (as set): pictures damaged on the way are shown as they come"
        } else {
            "[app] ffmpeg was not found: mpv receives the stream itself, and pictures damaged on the way are shown as they come"
        });
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
    if end(app) {
        overlay::close(app);
        app.watch.muted.store(false, Ordering::SeqCst);
        app.log("[app] stopped watching");
    }
}

/// Ends the session, if there is one (true).
fn end(app: &AppState) -> bool {
    let Some(session) = app.watch.session.lock().unwrap().take() else { return false };
    if let Some(control) = &session.control {
        control.stop();
    }
    session.keyframes.stop();
    session.runner.set_guard_tap(None);
    session.runner.set_ipc_tap(None, None);
    session.runner.on_player_message(None);
    session.runner.stop();
    session.viewer.stop();
    true
}

/// Joins the same host again with the delay as it is set now: the delay is agreed on when joining, so a new one
/// takes a new session. The panel over the player stays open meanwhile.
pub fn rejoin(app: &Arc<AppState>) -> Result<(), Vec<String>> {
    let Some(session) = current(app) else { return Ok(()) };
    if session.settings.latency_ms == app.inner.lock().unwrap().receiver.latency_ms {
        return Ok(());
    }
    let host_id = session.host_id.clone();
    drop(session);
    end(app);
    app.log("[app] joining again with the new delay");
    let result = start(app, &host_id);
    if result.is_err() {
        overlay::close(app);
    }
    result
}

/// Switches the player to another decoder (`--hwdec`) without a restart; a player started later takes it too.
pub fn set_hwdec(app: &AppState, mode: HwDecMode) {
    let Some(session) = current(app) else { return };
    let value = mode.mpv_value(app.platform);
    if session.runner.set_hwdec(value) {
        app.log(&format!("[app] decoder: {value}, switched while playing"));
    } else if session.runner.is_running() {
        app.log(&format!("[app] decoder: {value}, from the next start of the player"));
    }
}

/// Mutes or unmutes the player (the panel over the player); a player started later (joining again) takes it too.
pub fn set_mute(app: &AppState, muted: bool) {
    app.watch.muted.store(muted, Ordering::SeqCst);
    if let Some(session) = current(app) {
        session.runner.set_mute(muted);
        app.log(if muted { "[app] sound off" } else { "[app] sound on" });
    }
}

/// The process of the player that runs now: the panel over the player looks for its window.
pub fn player_pid(app: &AppState) -> Option<u32> {
    current(app)?.runner.mpv_pid()
}

/// Runs with every status tick: the network report in the log.
pub fn housekeeping(app: &AppState) {
    let Some(session) = current(app) else { return };
    let Some(stats) = session.viewer.srt_stats() else { return };
    session.keyframes.set_rtt(Duration::from_secs_f64(stats.rtt_ms.max(0.0) / 1000.0));
    let line = session.net_report.lock().unwrap().line(&stats);
    if let Some(line) = line {
        app.log(&format!("[net] {line}"));
    }
}

pub fn toggle_fullscreen(app: &AppState) {
    if let Some(session) = current(app) {
        session.runner.toggle_fullscreen();
        overlay::focus_player(app);
    }
}

/// The live list was read again: a host that has started offering remote control since this session joined it
/// (its user allowed this computer meanwhile) gets the side channel now rather than at the next reconnect.
pub fn live_list_updated(app: &AppState, hosts: &[hdrcast_p2p::LiveEntry]) {
    let Some(session) = current(app) else { return };
    if hosts.iter().any(|h| h.record.host_id == session.host_id && h.remote_control()) {
        session.viewer.host_now_offers_control();
    }
}

/// Asks the host for remote control (true), or gives it back to just watch (false).
pub fn want_control(app: &AppState, want: bool) -> Result<(), String> {
    let session = current(app).ok_or("Not watching.")?;
    session.control.as_ref().ok_or("This host does not offer remote control.")?.want(want);
    Ok(())
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
        ViewerSessionState::Approval => "approval",
        ViewerSessionState::Refused => "refused",
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
            "fullscreen": st.fullscreen,
            "width": st.source.as_ref().and_then(|p| p.width),
            "height": st.source.as_ref().and_then(|p| p.height),
            // null: mpv receives the stream itself, nothing is guarded
            "guard": play.guard.map(|g| json!({
                "frozen": g.frozen,
                "losses": g.losses,
                "freezes": g.freezes,
                "heldBack": g.hidden + g.dropped,
            })),
        })
    });
    let control = match &session.control {
        Some(link) if viewer.state() == ViewerSessionState::Connected && viewer.host_offers_control() => link.state(),
        _ => "off",
    };
    json!({
        "active": true,
        "hostId": session.host_id,
        "hostName": if learned.is_empty() { session.host_name.clone() } else { learned },
        "hostDeviceCode": viewer.host_device_key().as_deref().map(hdrcast_core::group::device::code_of).unwrap_or_default(),
        "hostOnDemand": viewer.host_on_demand(),
        // agreed on when joining: a new one takes joining again
        "latencyMs": session.settings.latency_ms,
        "overlay": overlay::is_expanded(app),
        "muted": app.watch.muted.load(Ordering::SeqCst),
        "session": {
            "state": state_name,
            "status": viewer.status(),
            "path": viewer.path_description(),
            "net": viewer.srt_stats().map(|s| net_report::status(&s, End::Viewer)),
            "couldHelp": viewer.state() == ViewerSessionState::Waiting && viewer.this_side_could_help(),
        },
        "player": player,
        "control": control,
    })
}
