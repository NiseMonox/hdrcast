//! Sharing this computer's screen with the group (docs/design.md §2.7, §8.10).
//!
//! One P2P host announces this computer for as long as it is available or live. The capture engine and the
//! encoder ([`SenderRunner`]) run only while needed: while live (a broadcast, which everyone in the group who is
//! not blocked may watch), or on request, while somebody the rules let in is watching. A join waits for the
//! rules (`access.rs`), for the user's answer when the rules say to ask, and for the stream to flow before it is
//! answered, so that the viewer's player finds the stream at once. Remote control is registered per session,
//! with the key agreed with the viewer's device and the rule of that device. With "keyframe on request" the
//! capture engine encodes, and viewers who lose part of the stream get a fresh keyframe (§2.8, §8.12). A
//! broadcast that everyone who watched has left ends by itself when the settings say so (§8.10).
//!
//! A viewer in control mode (§11) gets the low-latency stream instead: a capture engine of its own encodes for it
//! (`low_latency_host.rs`), started when the first such viewer is let in and stopped a little after the last one
//! left. It runs next to the buffered stream when both kinds of viewers are there.
//!
//! A viewer who controls this computer may pick which of its displays is shared (§9.2): that is done as if this
//! computer's user had picked it on the Share page.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use hdrcast_capture::{CaptureArgs, CaptureFormat, EncodeArgs, EngineEncoder, OutputInfo};
use hdrcast_control::displays::{self, Display};
use hdrcast_core::common::firewall::row as firewall_row;
use hdrcast_core::group::device;
use hdrcast_core::group::keys::GroupKeys;
use hdrcast_core::messages::JoinAnswer;
use hdrcast_media::keyframe;
use hdrcast_media::low_latency;
use hdrcast_media::sender::encoder_profiles;
use hdrcast_media::sender::ffmpeg_command::{self, AudioCodec, FfmpegPlan};
use hdrcast_media::sender::settings::{HdrMode, SenderSettings};
use hdrcast_p2p::{JoinTicket, P2pHost, P2pHostOptions, SideUse, SwitchableStream, ViewerInfo, ViewerState};
use serde_json::{json, Value};
use tauri::Emitter;

use crate::access::{AccessSettings, Connect, Control};
use crate::commands;
use crate::control_host::{ControlHub, HostDisplay};
use crate::keyframe_host::KeyframeHub;
use crate::low_latency_host::{LowLatencyHost, LowLatencyPlan};
use crate::net_report::{self, End, NetReport};
use crate::sender_runner::{self, RunnerState, SendPlan, SenderRunner};
use crate::state::{self, AppState, Inner};

/// The encoder keeps running this long after the last viewer left, for a viewer who comes straight back.
const IDLE_STOP: Duration = Duration::from_secs(20);
/// A broadcast that is to end when its viewers leave ends this long after the last one left. A viewer who comes
/// back later is let in on request (without asking when it watched lately), and a path that drops only counts as
/// leaving after 15 s of silence, by when the viewer has joined again.
const UNWATCHED_END: Duration = Duration::from_secs(2);
/// How long a request waits for this computer's user (the viewer waits a minute).
const KNOCK_LIMIT: Duration = Duration::from_secs(50);
/// Somebody let in (or watching) is let in again without asking for this long: reconnects after a hiccup.
const RECENTLY_LET_IN: Duration = Duration::from_secs(180);
/// Somebody refused is refused again without asking for this long (older versions keep retrying).
const RECENTLY_REFUSED: Duration = Duration::from_secs(120);
/// Somebody disconnected by this computer's user stays out this long, whatever the rules say: a viewer
/// reconnects by itself, and one that may connect without asking would otherwise be back at once.
const DISCONNECTED_FOR: Duration = Duration::from_secs(60);
/// How long a viewer let in on request waits for the stream before being told it cannot be served.
const STREAM_WAIT: Duration = Duration::from_secs(25);
/// A change of the stream's settings while it runs restarts capture and encoder this long after the last edit:
/// dragging the bitrate slider or trying out frame rates restarts once, not at every step.
const RESTART_SETTLE: Duration = Duration::from_millis(800);
/// Control mode's engine keeps running this long after the last viewer in control mode left (one that comes
/// straight back, or joins again after a hiccup, finds it running).
const LOW_LATENCY_IDLE_STOP: Duration = Duration::from_secs(10);

/// Who a viewer is, as far as the rules go.
#[derive(Debug, Clone)]
struct Who {
    /// "d:<device key>" for a signed join, "p:<peer id>" otherwise (anyone in the group could have written that).
    id: String,
    device_key: Option<String>,
    name: String,
}

impl Who {
    fn of(ticket: &JoinTicket) -> Who {
        let id = match &ticket.device_key {
            Some(key) => format!("d:{key}"),
            None => format!("p:{}", ticket.viewer_id),
        };
        Who { id, device_key: ticket.device_key.clone(), name: ticket.name.clone() }
    }
}

/// A join waiting for this computer's user.
struct Knock {
    ticket: JoinTicket,
    who: Who,
    since: Instant,
}

/// A viewer that was let in.
struct Admitted {
    who: Who,
    /// The session's remote-control key was agreed with the viewer's device: its device's rule applies. Without
    /// it the session uses the group's key, which everyone in the group knows, so only everyone else's rule can.
    device_bound: bool,
}

#[derive(Default)]
struct Activity {
    /// Viewers let in that wait for the stream to flow.
    admitting: usize,
    /// Since when nobody has been watching: while the encoder runs on request, or while live once everyone who
    /// watched has left (when the broadcast is to end then).
    idle_since: Option<Instant>,
    /// Somebody has watched since going live: before that, a broadcast nobody watches is waiting for its viewers.
    watched: bool,
}

impl Activity {
    /// Housekeeping's look at the stream at `now` (`idle`: it runs and nobody watches, connects or waits;
    /// `watching`: somebody's picture flows). True once nobody has been watching for long enough where that ends
    /// something: [`IDLE_STOP`] for the encoder that runs on request, [`UNWATCHED_END`] for a broadcast (`live`)
    /// that somebody watched, when the settings say it ends when its viewers leave (`ends_unwatched`).
    fn idle_long_enough(&mut self, now: Instant, idle: bool, watching: bool, live: bool, ends_unwatched: bool) -> bool {
        if live && watching {
            self.watched = true;
        }
        if !idle || self.admitting > 0 || (live && !(ends_unwatched && self.watched)) {
            self.idle_since = None;
            return false;
        }
        let since = *self.idle_since.get_or_insert(now);
        now.saturating_duration_since(since) >= if live { UNWATCHED_END } else { IDLE_STOP }
    }
}

struct Session {
    host: Arc<P2pHost>,
    ts: SwitchableStream,
    hub: Arc<ControlHub>,
    keyframes: Arc<KeyframeHub>,
    runner: SenderRunner,
    keys: GroupKeys,
    /// Broadcasting: the encoder runs regardless of viewers, and asking is skipped (only blocked devices stay out).
    live: AtomicBool,
    knocks: Mutex<Vec<Knock>>,
    admitted: Mutex<HashMap<String, Admitted>>,
    activity: Mutex<Activity>,
    recent_ok: Mutex<HashMap<String, Instant>>,
    recent_no: Mutex<HashMap<String, Instant>>,
    /// Who this computer's user disconnected, and when.
    disconnected: Mutex<HashMap<String, Instant>>,
    /// Held while the encoder is started or stopped on request, so a start and an idle stop never cross.
    starting: Mutex<()>,
    /// Why the stream could not start for the latest request (shown until the next start).
    problem: Mutex<Option<String>>,
    /// Viewers whose request for control the user has been told about.
    control_asks: Mutex<HashSet<String>>,
    /// Each viewer's network report in the log (by session id).
    net_reports: Mutex<HashMap<String, NetReport>>,
    /// The settings changed while streaming in a way only a restart applies: when to do it.
    restart_due: Mutex<Option<Instant>>,
    /// Control mode's own capture engine and sender (§11).
    low_latency: LowLatencyHost,
    /// Since when its engine has been running with nobody in control mode.
    low_latency_idle_since: Mutex<Option<Instant>>,
}

#[derive(Default)]
pub struct Share {
    session: Mutex<Option<Arc<Session>>>,
    /// Broadcasts that ended because everyone who watched had left, for the page to say so once.
    ended_unwatched: AtomicU32,
}

fn current(app: &AppState) -> Option<Arc<Session>> {
    app.share.session.lock().unwrap().clone()
}

/// This computer is announced to the group (available or live).
pub fn is_hosting(app: &AppState) -> bool {
    current(app).is_some()
}

/// The screen is being captured, or capture is starting or stopping.
pub fn is_streaming(app: &AppState) -> bool {
    current(app).is_some_and(|s| matches!(s.runner.state(), RunnerState::Starting | RunnerState::Streaming | RunnerState::Stopping))
}

/// Builds the session plan from the settings and the chosen display, or says what is missing.
fn build_plan(app: &AppState, inner: &Inner) -> Result<SendPlan, Vec<String>> {
    let sender = &inner.sender;
    let mut problems = sender.validate_for(app.platform);
    if !inner.group.current().is_configured() {
        problems.push("There is no group yet. Create one (then send the invite to your friends), or paste an invite to join a friend's group.".to_string());
    }
    let output = state::selected_output(sender, &inner.outputs);
    if output.is_none() {
        problems.push("No display selected.".to_string());
    }
    let ffmpeg = state::ffmpeg_path(sender);
    if ffmpeg.is_none() {
        problems.push(commands::ffmpeg_missing(app.platform));
    }
    let (Some(output), Some(ffmpeg), true) = (output, ffmpeg, problems.is_empty()) else { return Err(problems) };

    let hdr = output.hdr && sender.hdr_mode == HdrMode::Auto;
    let (width, height) = sender.resolve_size(output.width, output.height);
    let format = sender.resolve_format(output.hdr);
    let pid = std::process::id();
    let video_pipe = format!("{}-{pid}", CaptureArgs::DEFAULT_VIDEO_PIPE);
    let audio_pipe = format!("{}-{pid}", CaptureArgs::DEFAULT_AUDIO_PIPE);

    // A display in HDR mode is captured with `--format auto`: the engine has the last word, because a Mac whose
    // system refuses HDR capture can still send SDR. The runner tells ffmpeg what the engine announces. SDR is
    // asked for by name: that the engine can always deliver.
    let capture_format = if format == CaptureFormat::P010 { CaptureFormat::Auto } else { format };
    let capture = sender.to_capture_args(output, width, height, capture_format, &video_pipe, &audio_pipe, pid);
    let mut capture_argv = vec!["capture".to_string()];
    capture_argv.extend(capture.to_argv());

    // The encoder's MPEG-TS goes to stdout (the plan's default), into the fan-out that serves every viewer.
    let plan = FfmpegPlan {
        video_pipe_name: video_pipe.clone(),
        audio_pipe_name: sender.audio.then(|| audio_pipe.clone()),
        pixel_format: format,
        width,
        height,
        fps: sender.fps.max(1) as u32,
        encoder: sender.to_encoder_settings(hdr, output.max_luminance.round().max(0.0) as u32),
        audio: if sender.audio { sender.audio_codec } else { AudioCodec::None },
        audio_bitrate_kbps: sender.audio_bitrate_kbps.max(0) as u32,
        ..FfmpegPlan::default()
    };
    // Refuse an unusable plan now, while the user is looking, rather than after the engine started.
    ffmpeg_command::build(&plan).map_err(|e| vec![e.to_string()])?;

    let mut pipe_names = vec![video_pipe];
    if sender.audio {
        pipe_names.push(audio_pipe);
    }
    // keyframe on request: the engine encodes with NVENC itself, ffmpeg adds the sound (§2.8)
    let engine_encode = if sender.encodes_in_engine() { encoder_profiles::engine_encode_args(&plan.encoder) } else { None };
    Ok(SendPlan { capture_exe: state::current_exe(), capture_argv, ffmpeg_exe: ffmpeg, ffmpeg: plan, pipe_names, engine_encode })
}

/// Control mode's engine (§11.2): the shared picture (display, size, HDR, cursor, sound) at control mode's own
/// frame rate and bitrate, encoded by NVENC inside the engine the way Vibepollo encodes (`--low-latency`).
fn build_low_latency_plan(inner: &Inner) -> Result<LowLatencyPlan, Vec<String>> {
    let sender = &inner.sender;
    let output = state::selected_output(sender, &inner.outputs).ok_or_else(|| vec!["No display selected.".to_string()])?;
    let (width, height) = sender.resolve_size(output.width, output.height);
    let format = sender.resolve_format(output.hdr);
    let capture_format = if format == CaptureFormat::P010 { CaptureFormat::Auto } else { format };
    let pid = std::process::id();
    let (video_pipe, audio_pipe) = (format!("hdrcast-ll-video-{pid}"), format!("hdrcast-ll-audio-{pid}"));
    let mut capture = sender.to_capture_args(output, width, height, capture_format, &video_pipe, &audio_pipe, pid);
    capture.fps = sender.control_fps.clamp(10, 240) as u32;
    let kbps = sender.control_bitrate_mbps.clamp(1, 500) as u32 * 1000;
    capture.encode = Some(EncodeArgs::low_latency(EngineEncoder::Nvenc, kbps, capture.fps));
    let mut capture_argv = vec!["capture".to_string()];
    capture_argv.extend(capture.to_argv());
    let summary = format!(
        "control mode: HEVC (NVENC, ultra-low latency) {} Mbps, up to {} fps, {}x{} {}",
        kbps / 1000,
        capture.fps,
        width,
        height,
        if format == CaptureFormat::P010 { "HDR10" } else { "SDR" }
    );
    Ok(LowLatencyPlan {
        audio_pipe: sender.audio.then_some(audio_pipe),
        capture,
        capture_exe: state::current_exe(),
        capture_argv,
        video_pipe,
        summary,
    })
}

/// The display both streams capture, as the capture engine names it (empty when none is selected): remote control's
/// pointer is confined to it.
fn shared_display(inner: &Inner) -> String {
    state::selected_output(&inner.sender, &inner.outputs).map(|o| o.device_name.clone()).unwrap_or_default()
}

/// Control mode can be offered: switched on, and an NVIDIA GPU to encode on (the engine drives NVENC itself; one
/// that found out it cannot made `low_latency` unavailable).
fn offers_control_mode(inner: &Inner, low_latency: Option<&LowLatencyHost>) -> bool {
    let nvidia = inner.outputs.iter().any(|o| o.adapter.to_ascii_uppercase().contains("NVIDIA"));
    inner.sender.offers_control_mode(nvidia) && !low_latency.is_some_and(LowLatencyHost::unavailable)
}

/// The interval of the keyframes nobody asks for.
fn gop_of(app: &AppState) -> Duration {
    Duration::from_secs_f64(app.inner.lock().unwrap().sender.gop_seconds.max(0.1))
}

// ---- the host session ------------------------------------------------------------------------------------------

/// Announces this computer to the group (not live yet). The error is the list of things to fix first.
fn start_hosting(app: &Arc<AppState>) -> Result<Arc<Session>, Vec<String>> {
    if let Some(session) = current(app) {
        return Ok(session);
    }
    let (group, bitrate_bps, max_viewers, ffmpeg) = {
        let inner = app.inner.lock().unwrap();
        let ffmpeg = state::ffmpeg_path(&inner.sender).map(|p| p.into_os_string()).unwrap_or_else(|| "ffmpeg".into());
        (inner.group.current().clone(), inner.sender.bitrate_bps(), inner.sender.max_viewers.max(1) as u32, ffmpeg)
    };
    let keys = group.keys().map_err(|_| vec!["There is no group yet.".to_string()])?;
    let weak_app: Weak<AppState> = Arc::downgrade(app);

    // Viewers come and go through the group; the encoder serves all of them, and a restarted encoder is spliced
    // into the same fan-out, so the host itself never has to restart.
    let ts = SwitchableStream::new();
    let session = Arc::new_cyclic(|me: &Weak<Session>| {
        let (firewall_app, rules_app, admit_app, admit_session, demand_session) =
            (weak_app.clone(), weak_app.clone(), weak_app.clone(), me.clone(), me.clone());
        let keyframes_app = weak_app.clone();
        let (control_app, control_session) = (weak_app.clone(), me.clone());
        let host = Arc::new(P2pHost::new(
            P2pHostOptions {
                group,
                ffmpeg_path: ffmpeg,
                bitrate_bps,
                max_viewers,
                job: Some(app.job.clone()),
                map_router_port: state::router_mapping_wanted(),
                inbound_allowed: Some(Arc::new(move || {
                    firewall_app.upgrade().and_then(|app| firewall_row::inbound_allowed(app.inner.lock().unwrap().firewall))
                })),
                offer_remote_control: Some(Arc::new(move || {
                    rules_app.upgrade().is_some_and(|app| app.inner.lock().unwrap().access.offers_control())
                })),
                device: Some(app.device.clone()),
                admit: Some(Arc::new(move |ticket| {
                    if let (Some(app), Some(session)) = (admit_app.upgrade(), admit_session.upgrade()) {
                        on_join(&app, &session, ticket);
                    }
                })),
                on_demand: Some(Arc::new(move || demand_session.upgrade().is_some_and(|s| !s.live.load(Ordering::Acquire)))),
                offer_keyframes: Some(Arc::new(move || keyframes_app.upgrade().is_some_and(|app| app.inner.lock().unwrap().sender.encodes_in_engine()))),
                offer_low_latency: Some(Arc::new(move || {
                    let (Some(app), Some(session)) = (control_app.upgrade(), control_session.upgrade()) else { return false };
                    let inner = app.inner.lock().unwrap();
                    offers_control_mode(&inner, Some(&session.low_latency))
                })),
                ..P2pHostOptions::default()
            },
            Box::new(ts.clone()),
            app.log_fn(),
        ));
        let sender = me.clone();
        let hub = ControlHub::start(
            Arc::new(move |sid, payload| sender.upgrade().is_some_and(|s| s.host.send_side(sid, payload))),
            app.logger(),
        );
        let attach = ts.clone();
        let runner = SenderRunner::new(app.job.clone(), app.logger(), Arc::new(move |stdout| attach.attach(Box::new(stdout))));
        let (sender, forcer, group_keys) = (me.clone(), me.clone(), keys.clone());
        let keyframes = KeyframeHub::start(
            gop_of(app),
            Arc::new(move |sid, payload| sender.upgrade().is_some_and(|s| s.host.send_side_for(sid, SideUse::Keyframes, payload))),
            Arc::new(move || forcer.upgrade().is_some_and(|s| s.runner.request_keyframe())),
            Arc::new(move |sid| group_keys.keyframe_key(sid)),
            app.logger(),
        );
        let sender = me.clone();
        let low_latency = LowLatencyHost::new(
            app.job.clone(),
            app.logger(),
            Arc::new(move |sid, payloads| sender.upgrade().map_or(0, |s| s.host.send_low_latency(sid, payloads.iter().copied()))),
        );
        Session {
            host,
            ts,
            hub,
            keyframes,
            runner,
            keys,
            live: AtomicBool::new(false),
            knocks: Mutex::new(Vec::new()),
            admitted: Mutex::new(HashMap::new()),
            activity: Mutex::new(Activity::default()),
            recent_ok: Mutex::new(HashMap::new()),
            recent_no: Mutex::new(HashMap::new()),
            disconnected: Mutex::new(HashMap::new()),
            starting: Mutex::new(()),
            problem: Mutex::new(None),
            control_asks: Mutex::new(HashSet::new()),
            net_reports: Mutex::new(HashMap::new()),
            restart_due: Mutex::new(None),
            low_latency,
            low_latency_idle_since: Mutex::new(None),
        }
    });
    // the side channel carries remote control, keyframe requests and control mode's requests: they tell themselves
    // apart by their first byte
    let (hub, keyframes, fast) = (session.hub.clone(), session.keyframes.clone(), session.low_latency.clone());
    session.host.set_side_handler(Arc::new(move |sid, payload| {
        if low_latency::is_low_latency_datagram(payload) {
            fast.on_datagram(sid, payload);
        } else if keyframe::is_keyframe_datagram(payload) {
            keyframes.on_datagram(sid, payload);
        } else {
            hub.on_datagram(sid, payload);
        }
    }));
    let keyframes = session.keyframes.clone();
    session.host.on_stream_event(move |sid, event| keyframes.stream_event(sid, event));
    let keyframes = session.keyframes.clone();
    session.runner.on_news(move |news| keyframes.encoder_news(news));
    // the viewer in control may share another display (§9.2)
    let (list_app, pick_app) = (weak_app.clone(), weak_app.clone());
    session.hub.on_display_picks(
        Arc::new(move || list_app.upgrade().map(|app| host_displays(&app.inner.lock().unwrap().outputs)).unwrap_or_default()),
        Arc::new(move |viewer, device_name| {
            let Some(app) = pick_app.upgrade() else { return };
            let (viewer, device_name) = (viewer.to_string(), device_name.to_string());
            // not on the network's thread: the settings are saved and the streams start again
            thread::spawn(move || share_display_for(&app, &viewer, &device_name));
        }),
    );

    if let Err(e) = session.host.start() {
        app.log(&format!("[app] cannot start the group session: {e}"));
        session.shut_down();
        return Err(vec![format!("Cannot start the group session: {e}")]);
    }
    app.log(&format!("[app] this computer is in the group as {} (device {})", app.group().display_name, app.device.code()));
    *app.share.session.lock().unwrap() = Some(session.clone());
    Ok(session)
}

impl Session {
    fn shut_down(&self) {
        self.runner.stop();
        self.low_latency.stop();
        self.hub.stop();
        self.keyframes.stop();
        self.host.stop();
        self.ts.close();
    }

    fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }

    /// Ends one viewer's session because this computer's user said so: it is kept out for a minute.
    fn disconnect(&self, sid: &str, reason: &str) {
        if let Some(admitted) = self.admitted.lock().unwrap().get(sid) {
            self.disconnected.lock().unwrap().insert(admitted.who.id.clone(), Instant::now());
            self.recent_ok.lock().unwrap().remove(&admitted.who.id);
        }
        self.host.kick(sid, reason);
    }
}

/// The rules say this computer is available to connect, and there is a group to be available to. Closing the
/// window then leaves HdrCast running in the notification area.
pub fn stays_available(app: &AppState) -> bool {
    let inner = app.inner.lock().unwrap();
    inner.access.available && inner.group.current().is_configured()
}

/// At start, and whenever the group or the settings change: announce this computer if it should be available.
pub fn autostart(app: &Arc<AppState>) {
    if stays_available(app) && current(app).is_none() {
        if let Err(problems) = start_hosting(app) {
            app.log(&format!("[app] not available to the group: {}", problems.join(" ")));
        }
    }
}

/// Takes this computer out of the group: stops the stream, says goodbye to viewers and the server. Takes a few
/// seconds; call it off the UI thread.
pub fn shut_down(app: &AppState) {
    let Some(session) = app.share.session.lock().unwrap().take() else { return };
    app.log("[app] leaving the group session");
    session.shut_down();
}

/// The group changed: the old one's session ends, the new one's starts (when available).
pub fn group_changed(app: &Arc<AppState>) {
    shut_down(app);
    autostart(app);
}

/// The sender settings changed: what the live record says and what new viewers get follows them.
pub fn settings_changed(app: &AppState) {
    let Some(session) = current(app) else { return };
    let (bitrate, max, ffmpeg) = {
        let inner = app.inner.lock().unwrap();
        (inner.sender.bitrate_bps(), inner.sender.max_viewers.max(1) as u32, state::ffmpeg_path(&inner.sender))
    };
    session.host.set_stream(bitrate, max);
    if let Some(ffmpeg) = ffmpeg {
        session.host.set_ffmpeg_path(ffmpeg.into_os_string());
    }
    session.keyframes.set_gop(gop_of(app));
    // "kf" in the live record follows the switch (it applies to the next stream that starts); "ll" too
    session.host.announce_now();
    apply_while_streaming(app, &session);
    apply_to_control_mode(app, &session);
}

/// This computer's displays as a viewer in control sees them (§9.2): the first eight, by their capture index.
fn host_displays(outputs: &[OutputInfo]) -> Vec<HostDisplay> {
    outputs
        .iter()
        .filter(|o| o.attached_to_desktop)
        .filter_map(|o| {
            let display = Display {
                id: u8::try_from(o.index).ok()?,
                name: if o.friendly_name.is_empty() { format!("Display {}", o.index + 1) } else { o.friendly_name.clone() },
                width: u16::try_from(o.width).unwrap_or(0),
                height: u16::try_from(o.height).unwrap_or(0),
                refresh_hz: o.refresh_rate.round().clamp(0.0, f64::from(u16::MAX)) as u16,
                hdr: o.hdr,
            };
            Some(HostDisplay { device_name: o.device_name.clone(), display })
        })
        .take(displays::MAX_DISPLAYS)
        .collect()
}

/// A viewer who controls this computer picked another display (§9.2): it is shared as if this computer's user had
/// picked it on the Share page. The setting is saved, the page shows it, and the streams that run start again on it.
fn share_display_for(app: &AppState, viewer: &str, device_name: &str) {
    let sender = {
        let mut inner = app.inner.lock().unwrap();
        let Some(output) = inner.outputs.iter().find(|o| o.device_name.eq_ignore_ascii_case(device_name)).cloned() else { return };
        if state::selected_output(&inner.sender, &inner.outputs).is_some_and(|o| o.device_name == output.device_name) {
            return;
        }
        inner.sender.output_index = output.index as i32;
        inner.sender.output_device_name = output.device_name.clone();
        inner.sender.output_friendly_name = output.friendly_name.clone();
        if let Err(e) = inner.sender.save(&SenderSettings::default_path()) {
            app.log(&format!("[app] could not save settings: {e}"));
        }
        let name = if output.friendly_name.is_empty() { output.device_name.as_str() } else { output.friendly_name.as_str() };
        app.log(&format!("[control] {viewer} switched the shared display to {name} ({}x{})", output.width, output.height));
        inner.sender.clone()
    };
    // the Share page keeps it (a later save of the page does not undo it)
    if let Some(handle) = app.handle.get() {
        let _ = handle.emit("sender", &sender);
    }
    settings_changed(app);
}

/// The settings changed: control mode's engine, when it runs, follows them (§11). A new bitrate goes to it without a
/// break; anything else of its picture starts it again (its viewers ask for a keyframe and go on). Switched off,
/// whoever is in control mode is disconnected (they may connect again to watch).
fn apply_to_control_mode(app: &AppState, session: &Arc<Session>) {
    let (offered, wanted, display) = {
        let inner = app.inner.lock().unwrap();
        (offers_control_mode(&inner, Some(&session.low_latency)), build_low_latency_plan(&inner), shared_display(&inner))
    };
    if !offered {
        for viewer in session.host.viewers().into_iter().filter(|v| v.low_latency) {
            session.host.kick(&viewer.sid, "the host switched control mode off");
        }
        return;
    }
    if !matches!(session.low_latency.state(), RunnerState::Starting | RunnerState::Streaming) {
        return;
    }
    let (Some(running), Ok(wanted)) = (session.low_latency.plan(), wanted) else { return };
    if running == wanted {
        return;
    }
    // the wanted plan at the running bitrate: equal when only the bitrate moved
    let mut at_running_rate = wanted.capture.clone();
    if let (Some(encode), Some(now)) = (at_running_rate.encode.as_mut(), running.capture.encode.as_ref()) {
        (encode.bitrate_kbps, encode.vbv_kbits) = (now.bitrate_kbps, now.vbv_kbits);
    }
    if at_running_rate == running.capture {
        if let Some(encode) = &wanted.capture.encode {
            if session.low_latency.set_bitrate(encode.bitrate_kbps, encode.vbv_kbits) {
                app.log(&format!("[app] control mode now at {} Mbps: the running encoder took it", encode.bitrate_kbps / 1000));
                session.low_latency.adopt_plan(wanted);
                return;
            }
        }
    }
    let (low_latency, session) = (session.low_latency.clone(), session.clone());
    app.log("[app] control mode's settings changed: starting its capture and encoder again");
    thread::spawn(move || {
        let _one_at_a_time = session.starting.lock().unwrap();
        // another display may be shared now
        session.hub.set_display(&display);
        low_latency.start(wanted);
    });
}

/// What a change of the settings means for the stream that runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanChange {
    Same,
    /// Only the bitrate moved, and the engine encodes: it takes the new rate (and its buffer) without a restart.
    Bitrate { kbps: u32, vbv_kbits: u32 },
    /// Anything else (or a bitrate ffmpeg encodes with): capture and encoder start again with the new plan.
    Restart,
}

fn plan_change(running: &SendPlan, wanted: &SendPlan) -> PlanChange {
    if running == wanted {
        return PlanChange::Same;
    }
    let (Some(now), Some(next)) = (&running.engine_encode, &wanted.engine_encode) else { return PlanChange::Restart };
    // the wanted plan at the running bitrate: the same as the running one when nothing else changed
    let mut at_running_rate = wanted.clone();
    at_running_rate.ffmpeg.encoder.bitrate_kbps = running.ffmpeg.encoder.bitrate_kbps;
    if let Some(encode) = &mut at_running_rate.engine_encode {
        (encode.bitrate_kbps, encode.vbv_kbits) = (now.bitrate_kbps, now.vbv_kbits);
    }
    if at_running_rate == *running {
        PlanChange::Bitrate { kbps: next.bitrate_kbps, vbv_kbits: next.vbv_kbits }
    } else {
        PlanChange::Restart
    }
}

/// The settings changed while the stream runs (docs/design.md §2.7): a new bitrate goes to an engine that encodes,
/// without a break; any other change of the stream restarts capture and encoder a moment later. The viewers stay
/// connected through a restart: the new encoder's stream joins the same fan-out, as after a crash.
fn apply_while_streaming(app: &AppState, session: &Session) {
    if !matches!(session.runner.state(), RunnerState::Starting | RunnerState::Streaming) {
        return; // the next start takes the settings as they are
    }
    let Some(running) = session.runner.plan() else { return };
    let wanted = {
        let inner = app.inner.lock().unwrap();
        build_plan(app, &inner)
    };
    let Ok(wanted) = wanted else { return }; // settings that cannot stream: the running stream goes on as it is
    let mut due = session.restart_due.lock().unwrap();
    match plan_change(&running, &wanted) {
        PlanChange::Same => *due = None, // changed back before the restart came
        PlanChange::Bitrate { kbps, vbv_kbits } if session.runner.set_bitrate(kbps, vbv_kbits) => {
            session.runner.adopt_plan(wanted);
            *due = None;
            app.log(&format!("[app] bitrate now {} Mbps: the running encoder took it, no restart", kbps / 1000));
        }
        _ => *due = Some(Instant::now() + RESTART_SETTLE),
    }
}

/// The access rules changed: availability, the remote control of who is watching, and waiting requests follow.
pub fn rules_changed(app: &Arc<AppState>) {
    let access = app.inner.lock().unwrap().access.clone();
    let Some(session) = current(app) else {
        autostart(app);
        return;
    };
    if !access.available && !session.is_live() {
        // no longer available: whoever watches on request is disconnected (a broadcast goes on until stopped)
        shut_down(app);
        return;
    }
    let mut now_blocked = Vec::new();
    for (sid, admitted) in session.admitted.lock().unwrap().iter() {
        session.hub.set_policy(sid, control_rule(&access, admitted).policy());
        if access.rule_for(admitted.who.device_key.as_deref()).connect == Connect::Block {
            now_blocked.push(sid.clone());
        }
    }
    for sid in now_blocked {
        session.host.kick(&sid, "the host no longer lets you watch");
    }
    // requests that the new rules decide by themselves
    let decided: Vec<Knock> = {
        let mut knocks = session.knocks.lock().unwrap();
        let (decided, waiting) =
            std::mem::take(&mut *knocks).into_iter().partition(|k| access.rule_for(k.who.device_key.as_deref()).connect != Connect::Ask);
        *knocks = waiting;
        decided
    };
    for knock in decided {
        if access.rule_for(knock.who.device_key.as_deref()).connect == Connect::Allow {
            admit_in_background(app, &session, knock.ticket, knock.who);
        } else {
            session.host.refuse(&knock.ticket.sid, JoinAnswer::REASON_DENIED);
        }
    }
    session.host.announce_now();
}

fn control_rule(access: &AccessSettings, admitted: &Admitted) -> Control {
    let device = admitted.who.device_key.as_deref().filter(|_| admitted.device_bound);
    access.rule_for(device).control
}

// ---- joins -----------------------------------------------------------------------------------------------------

/// A join passed the P2P checks (on the host's network thread: decide quickly, do the slow parts elsewhere).
fn on_join(app: &Arc<AppState>, session: &Arc<Session>, ticket: JoinTicket) {
    let who = Who::of(&ticket);
    let rule = {
        let mut inner = app.inner.lock().unwrap();
        if let Some(key) = &who.device_key {
            if inner.access.touch(key, &who.name) {
                let _ = inner.access.save();
            }
        }
        inner.access.rule_for(who.device_key.as_deref())
    };
    app.note_seen(&who.device_key, &who.name);
    let recently = |map: &Mutex<HashMap<String, Instant>>, within: Duration| {
        map.lock().unwrap().get(&who.id).is_some_and(|at| at.elapsed() < within)
    };
    if recently(&session.disconnected, DISCONNECTED_FOR) {
        app.log(&format!("[app] {} was disconnected a moment ago: refused", who.name));
        session.host.refuse(&ticket.sid, JoinAnswer::REASON_DENIED);
        return;
    }
    let verdict = match rule.connect {
        Connect::Block => Some(Err("may not connect to this computer")),
        Connect::Allow => Some(Ok(())),
        Connect::Ask if session.is_live() => Some(Ok(())),
        Connect::Ask if recently(&session.recent_ok, RECENTLY_LET_IN) => Some(Ok(())),
        Connect::Ask if recently(&session.recent_no, RECENTLY_REFUSED) => Some(Err("was turned down a moment ago")),
        Connect::Ask => None,
    };
    match verdict {
        Some(Ok(())) => admit_in_background(app, session, ticket, who),
        Some(Err(why)) => {
            app.log(&format!("[app] {} {why}: refused", who.name));
            session.host.refuse(&ticket.sid, JoinAnswer::REASON_DENIED);
        }
        None => {
            app.log(&format!("[app] {} asks to watch this computer: waiting for your answer", who.name));
            let superseded: Vec<Knock> = {
                let mut knocks = session.knocks.lock().unwrap();
                // a viewer that asks again (it re-joined) replaces its earlier request
                let (older, others): (Vec<Knock>, Vec<Knock>) = std::mem::take(&mut *knocks).into_iter().partition(|k| k.who.id == who.id);
                *knocks = others;
                knocks.push(Knock { ticket, who, since: Instant::now() });
                older
            };
            for knock in superseded {
                session.host.refuse(&knock.ticket.sid, JoinAnswer::REASON_STOPPED);
            }
            app.request_attention();
        }
    }
}

/// Starts the stream if needed, then lets the viewer in and registers its remote control. On a thread of its own.
fn admit_in_background(app: &Arc<AppState>, session: &Arc<Session>, ticket: JoinTicket, who: Who) {
    session.activity.lock().unwrap().admitting += 1;
    let (app, worker) = (app.clone(), session.clone());
    let spawned = thread::Builder::new().name("share-admit".into()).spawn(move || {
        let session = worker;
        // control mode has a stream of its own (§11); everybody else gets the buffered one
        let ready = if ticket.low_latency { ensure_low_latency(&app, &session) } else { ensure_streaming(&app, &session) };
        let outcome = ready.and_then(|()| session.host.admit(&ticket.sid));
        match outcome {
            Ok(device_key) => {
                let admitted = Admitted { who: who.clone(), device_bound: device_key.is_some() };
                let control = control_rule(&app.inner.lock().unwrap().access, &admitted);
                let key = device_key.unwrap_or_else(|| session.keys.control_key(&ticket.sid));
                session.hub.register(&ticket.sid, &who.name, &key, control.policy());
                if ticket.low_latency {
                    session.low_latency.register(&ticket.sid, &who.name, &key);
                } else {
                    session.keyframes.register(&ticket.sid, &who.name);
                }
                session.admitted.lock().unwrap().insert(ticket.sid.clone(), admitted);
                session.recent_ok.lock().unwrap().insert(who.id.clone(), Instant::now());
            }
            Err(why) => {
                app.log(&format!("[app] could not let {} in: {why}", who.name));
                // a no from inside admit() has been answered already; refusing again only finds nothing waiting
                session.host.refuse(&ticket.sid, JoinAnswer::REASON_BUSY);
            }
        }
        session.activity.lock().unwrap().admitting -= 1;
    });
    if spawned.is_err() {
        session.activity.lock().unwrap().admitting -= 1;
    }
}

/// Makes sure the encoder runs and its stream flows. The error says why it could not, for the log and the page.
fn ensure_streaming(app: &Arc<AppState>, session: &Session) -> Result<(), String> {
    let _one_at_a_time = session.starting.lock().unwrap();
    if matches!(session.runner.state(), RunnerState::Idle | RunnerState::Failed | RunnerState::Stopping) {
        start_runner(app, session).map_err(|problems| {
            let text = problems.join("\n\n");
            *session.problem.lock().unwrap() = Some(text.clone());
            text
        })?;
    }
    let (begun, bytes) = (Instant::now(), session.host.stream_bytes());
    loop {
        match session.runner.state() {
            RunnerState::Streaming if session.host.stream_bytes() > bytes => return Ok(()),
            RunnerState::Failed => {
                let detail = session.runner.snapshot().detail;
                *session.problem.lock().unwrap() = Some(detail.clone());
                return Err(format!("the stream could not start: {detail}"));
            }
            RunnerState::Idle => return Err("the stream was stopped".to_string()),
            _ => {}
        }
        if begun.elapsed() > STREAM_WAIT {
            return Err(format!("the stream did not start within {} s", STREAM_WAIT.as_secs()));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Makes sure control mode's engine runs and its pictures come (§11). The error says why it could not.
fn ensure_low_latency(app: &Arc<AppState>, session: &Session) -> Result<(), String> {
    let _one_at_a_time = session.starting.lock().unwrap();
    if !matches!(session.low_latency.state(), RunnerState::Starting | RunnerState::Streaming) {
        let (plan, display) = {
            let inner = app.inner.lock().unwrap();
            if !offers_control_mode(&inner, Some(&session.low_latency)) {
                return Err("control mode is not offered on this computer".to_string());
            }
            (build_low_latency_plan(&inner), shared_display(&inner))
        };
        let plan = plan.map_err(|problems| problems.join(" "))?;
        // the pointer of whoever controls is confined to the display this engine captures, as with the buffered stream
        session.hub.set_display(&display);
        app.log(&format!("[app] {}", plan.summary));
        session.low_latency.start(plan);
    }
    let begun = Instant::now();
    loop {
        match session.low_latency.state() {
            RunnerState::Streaming if session.low_latency.pictures() > 0 => return Ok(()),
            RunnerState::Failed => return Err(format!("control mode could not start: {}", session.low_latency.status()["detail"].as_str().unwrap_or(""))),
            RunnerState::Idle => return Err("control mode was stopped".to_string()),
            _ => {}
        }
        if begun.elapsed() > STREAM_WAIT {
            return Err(format!("control mode's stream did not start within {} s", STREAM_WAIT.as_secs()));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Starts capture and encoder from the current settings.
fn start_runner(app: &AppState, session: &Session) -> Result<(), Vec<String>> {
    let (plan, display, bitrate, max) = {
        let inner = app.inner.lock().unwrap();
        let plan = build_plan(app, &inner)?;
        (plan, shared_display(&inner), inner.sender.bitrate_bps(), inner.sender.max_viewers.max(1) as u32)
    };
    *session.problem.lock().unwrap() = None;
    session.hub.set_display(&display);
    session.host.set_stream(bitrate, max);
    session.host.set_ffmpeg_path(plan.ffmpeg_exe.clone().into_os_string());
    app.log(&format!("[app] {}", sender_runner::summary(&plan.ffmpeg)));
    session.runner.start(plan, true);
    Ok(())
}

// ---- what the page asks for -------------------------------------------------------------------------------------

/// Goes live: a broadcast to the group. The error is the list of things to fix first, as sentences.
pub fn go_live(app: &Arc<AppState>) -> Result<(), Vec<String>> {
    let session = start_hosting(app)?;
    if session.is_live() {
        return Err(vec!["Already live.".to_string()]);
    }
    {
        let _one_at_a_time = session.starting.lock().unwrap();
        if !matches!(session.runner.state(), RunnerState::Starting | RunnerState::Streaming) {
            start_runner(app, &session)?;
        }
        // a new broadcast: it can end for want of viewers only once somebody has watched it
        {
            let mut activity = session.activity.lock().unwrap();
            activity.watched = false;
            activity.idle_since = None;
        }
        session.live.store(true, Ordering::Release);
    }
    session.host.announce_now();
    // while live, whoever waited for an answer may just watch (the blocked never get to wait)
    for knock in std::mem::take(&mut *session.knocks.lock().unwrap()) {
        admit_in_background(app, &session, knock.ticket, knock.who);
    }
    Ok(())
}

/// Ends a broadcast, or everything served on request: the stream stops and every viewer is disconnected. This
/// computer stays available when the rules say so. Takes a few seconds; call it off the UI thread.
pub fn stop(app: &AppState) {
    let Some(session) = current(app) else { return };
    let available = app.inner.lock().unwrap().access.available;
    session.live.store(false, Ordering::Release);
    *session.problem.lock().unwrap() = None; // seen: "OK" under a failed start lands here too
    if !available {
        shut_down(app);
        app.log("[app] stopped by user");
        return;
    }
    for knock in std::mem::take(&mut *session.knocks.lock().unwrap()) {
        session.host.refuse(&knock.ticket.sid, JoinAnswer::REASON_STOPPED);
    }
    for viewer in session.host.viewers() {
        session.disconnect(&viewer.sid, "the host stopped sharing");
    }
    {
        let _one_at_a_time = session.starting.lock().unwrap();
        session.runner.stop();
        session.low_latency.stop();
    }
    session.host.announce_now();
    app.log("[app] stopped sharing; still available to the group");
}

/// Ends a broadcast that everyone who watched has left, as the user's Stop would, but only while nobody came back
/// (so nobody is disconnected). This computer stays available when the rules say so. Takes a few seconds.
fn end_unwatched(app: &AppState, session: &Arc<Session>) {
    let _one_at_a_time = session.starting.lock().unwrap();
    // looked at again under the lock: somebody may have come back, or the user stopped (and maybe went live again)
    let unwatched = {
        let activity = session.activity.lock().unwrap();
        activity.watched && activity.admitting == 0
    };
    // viewers in control mode do not watch the broadcast: they have a stream of their own
    let still = unwatched
        && session.is_live()
        && session.host.viewers().iter().all(|v| v.low_latency)
        && current(app).is_some_and(|now| Arc::ptr_eq(&now, session));
    if !still {
        return;
    }
    session.live.store(false, Ordering::Release);
    app.share.ended_unwatched.fetch_add(1, Ordering::AcqRel);
    if !app.inner.lock().unwrap().access.available {
        app.log("[app] everyone who watched has left: the broadcast ended");
        shut_down(app);
        return;
    }
    session.runner.stop();
    session.host.announce_now();
    app.log("[app] everyone who watched has left: the broadcast ended; still available to the group");
}

/// Disconnects one viewer (and keeps it out for a minute).
pub fn kick(app: &AppState, sid: &str) -> Result<(), String> {
    let session = current(app).ok_or("Not sharing.")?;
    session.disconnect(sid, "the host disconnected you");
    Ok(())
}

/// The user's answer to a request to watch. `remember`: let that device in from now on without asking.
pub fn answer_knock(app: &Arc<AppState>, sid: &str, allow: bool, remember: bool) -> Result<(), String> {
    let session = current(app).ok_or("Not sharing.")?;
    let knock = {
        let mut knocks = session.knocks.lock().unwrap();
        let at = knocks.iter().position(|k| k.ticket.sid == sid).ok_or("That request is gone.")?;
        knocks.remove(at)
    };
    if remember {
        if let Some(key) = &knock.who.device_key {
            let mut inner = app.inner.lock().unwrap();
            let rule = inner.access.rule_for(Some(key));
            let control = if inner.access.device(key).is_some() { rule.control } else { inner.access.others.control };
            inner.access.upsert(key, &knock.who.name, if allow { Connect::Allow } else { Connect::Block }, control);
            inner.access_rev += 1;
            if let Err(e) = inner.access.save() {
                app.log(&format!("[app] could not save the access rules: {e}"));
            }
        }
    }
    if allow {
        app.log(&format!("[app] you let {} in", knock.who.name));
        admit_in_background(app, &session, knock.ticket, knock.who);
    } else {
        app.log(&format!("[app] you did not let {} in", knock.who.name));
        session.recent_no.lock().unwrap().insert(knock.who.id.clone(), Instant::now());
        session.host.refuse(&knock.ticket.sid, JoinAnswer::REASON_DENIED);
    }
    Ok(())
}

/// The user's answer to a viewer who asked for control (or taking it back).
pub fn control_set(app: &AppState, sid: &str, allow: bool) -> Result<(), String> {
    current(app).ok_or("Not sharing.")?.hub.decide(sid, allow)
}

/// Control mode's engine runs while somebody is in control mode, and [`LOW_LATENCY_IDLE_STOP`] longer; one that gave
/// up for good (no NVENC here) disconnects whoever is in control mode, and the live record stops offering it.
fn control_mode_housekeeping(app: &Arc<AppState>, session: &Arc<Session>, viewers: &[ViewerInfo]) {
    let in_control: Vec<String> = viewers.iter().filter(|v| v.low_latency).map(|v| v.sid.clone()).collect();
    session.low_latency.keep_only(&in_control);
    let state = session.low_latency.state();
    if state == RunnerState::Failed && !in_control.is_empty() {
        for sid in &in_control {
            session.host.kick(sid, "the host could not keep control mode's stream going");
        }
        session.host.announce_now();
        return;
    }
    let running = matches!(state, RunnerState::Starting | RunnerState::Streaming);
    let admitting = session.activity.lock().unwrap().admitting > 0;
    let mut idle = session.low_latency_idle_since.lock().unwrap();
    if !running || !in_control.is_empty() || admitting {
        *idle = None;
        return;
    }
    let since = *idle.get_or_insert_with(Instant::now);
    if since.elapsed() < LOW_LATENCY_IDLE_STOP {
        return;
    }
    *idle = None;
    let (app, session) = (app.clone(), session.clone());
    thread::spawn(move || {
        let _one_at_a_time = session.starting.lock().unwrap();
        // looked at again under the lock: somebody may have been let in meanwhile
        let still = session.activity.lock().unwrap().admitting == 0 && session.host.viewers().iter().all(|v| !v.low_latency);
        if still {
            app.log("[app] nobody is in control mode: stopping its stream until somebody connects");
            session.low_latency.stop();
        }
    });
}

/// Runs with every status tick: requests that waited too long, an encoder that gave up, an encoder serving
/// nobody, and what the remote control and the "recently let in" memory should forget.
pub fn housekeeping(app: &Arc<AppState>) {
    let Some(session) = current(app) else { return };
    let viewers = session.host.viewers();
    let sids: Vec<String> = viewers.iter().map(|v| v.sid.clone()).collect();
    session.hub.keep_only(&sids);
    session.keyframes.keep_only(&sids);
    control_mode_housekeeping(app, &session, &viewers);
    // from here on, the buffered stream: viewers in control mode have a stream of their own
    let viewers: Vec<_> = viewers.into_iter().filter(|v| !v.low_latency).collect();
    let restart = {
        let mut due = session.restart_due.lock().unwrap();
        if due.is_some_and(|at| Instant::now() >= at) {
            due.take()
        } else {
            None
        }
    };
    if restart.is_some() {
        let (app, session) = (app.clone(), session.clone());
        thread::spawn(move || {
            let _one_at_a_time = session.starting.lock().unwrap();
            // looked at again under the lock: the stream may have stopped meanwhile
            if matches!(session.runner.state(), RunnerState::Starting | RunnerState::Streaming) {
                app.log("[app] the stream's settings changed: starting capture and encoder again (viewers stay connected)");
                if let Err(problems) = start_runner(&app, &session) {
                    app.log(&format!("[app] the new settings cannot stream ({}); the stream goes on as it was", problems.join(" ")));
                }
            }
        });
    }
    // A viewer asks for control: like a request to watch, it reaches the user even with the window out of sight.
    let asking: HashSet<String> = sids.iter().filter(|sid| session.hub.state_of(sid) == "requested").cloned().collect();
    let newly_asked = {
        let mut told = session.control_asks.lock().unwrap();
        let newly = asking.iter().any(|sid| !told.contains(sid));
        *told = asking;
        newly
    };
    if newly_asked {
        app.request_attention();
    }
    {
        let mut admitted = session.admitted.lock().unwrap();
        admitted.retain(|sid, _| sids.contains(sid));
        let mut recent = session.recent_ok.lock().unwrap();
        for a in admitted.values() {
            recent.insert(a.who.id.clone(), Instant::now()); // watching now counts as just let in
        }
        recent.retain(|_, at| at.elapsed() < RECENTLY_LET_IN);
    }
    session.recent_no.lock().unwrap().retain(|_, at| at.elapsed() < RECENTLY_REFUSED);
    session.disconnected.lock().unwrap().retain(|_, at| at.elapsed() < DISCONNECTED_FOR);
    {
        let mut reports = session.net_reports.lock().unwrap();
        reports.retain(|sid, _| sids.contains(sid));
        for viewer in &viewers {
            let report = reports.entry(viewer.sid.clone()).or_insert_with(|| NetReport::new(End::Host));
            let flowing = viewer.srt.as_ref().is_some_and(|s| s.total.sent > 0);
            if let Some(line) = report.queue_line(viewer.queue_clears, flowing) {
                app.log(&format!("[net] {}: {line}", viewer.name));
            }
            let Some(stats) = &viewer.srt else { continue };
            if let Some(line) = report.line(stats) {
                app.log(&format!("[net] {}, {line}", viewer.name));
            }
        }
    }

    let expired: Vec<Knock> = {
        let mut knocks = session.knocks.lock().unwrap();
        let (old, fresh): (Vec<Knock>, Vec<Knock>) = std::mem::take(&mut *knocks).into_iter().partition(|k| k.since.elapsed() > KNOCK_LIMIT);
        *knocks = fresh;
        old
    };
    for knock in expired {
        app.log(&format!("[app] nobody answered {}'s request", knock.who.name));
        session.host.refuse(&knock.ticket.sid, JoinAnswer::REASON_TIMEOUT);
    }

    let state = session.runner.state();
    if state == RunnerState::Failed && (session.is_live() || !viewers.is_empty()) {
        // the encoder gave up for good: nobody gets a picture, so nobody is kept waiting for one
        let detail = session.runner.snapshot().detail;
        app.log(&format!("[app] the stream stopped for good ({detail}); disconnecting viewers"));
        *session.problem.lock().unwrap() = Some(detail);
        session.live.store(false, Ordering::Release);
        for viewer in &viewers {
            session.host.kick(&viewer.sid, "the host could not keep its stream going");
        }
        session.host.announce_now();
        if !app.inner.lock().unwrap().access.available {
            let app = app.clone();
            thread::spawn(move || shut_down(&app));
        }
        return;
    }

    // Once nobody has been watching for a while, the encoder stops when it runs on request, and a broadcast ends
    // when everyone who watched it has left and the settings say so.
    let live = session.is_live();
    let ends_unwatched = app.inner.lock().unwrap().sender.end_when_viewers_leave;
    let idle = matches!(state, RunnerState::Starting | RunnerState::Streaming)
        && viewers.is_empty()
        && session.knocks.lock().unwrap().is_empty();
    let watching = viewers.iter().any(|v| v.state == ViewerState::Watching);
    let stop_now = session.activity.lock().unwrap().idle_long_enough(Instant::now(), idle, watching, live, ends_unwatched);
    if stop_now {
        session.activity.lock().unwrap().idle_since = None;
        let (app, session) = (app.clone(), session.clone());
        thread::spawn(move || {
            if live {
                end_unwatched(&app, &session);
                return;
            }
            let _one_at_a_time = session.starting.lock().unwrap();
            // looked at again under the lock: somebody may have been let in meanwhile
            let still_idle = !session.is_live() && session.activity.lock().unwrap().admitting == 0 && session.host.viewers().is_empty();
            if still_idle {
                app.log("[app] nobody is watching: stopping the stream until somebody connects");
                session.runner.stop();
            }
        });
    }
}

// ---- for the page ---------------------------------------------------------------------------------------------

pub fn status(app: &AppState) -> Value {
    let device = app.device.code();
    let ended_unwatched = app.share.ended_unwatched.load(Ordering::Acquire);
    let Some(session) = current(app) else {
        return json!({
            "mode": "off", "state": "idle", "detail": "", "attempt": 0, "uptimeSec": 0,
            "capture": null, "encode": null, "video": null, "p2p": null, "knocks": [], "problem": null, "device": device,
            "endsInSec": null, "endedUnwatched": ended_unwatched,
        });
    };
    // live with nobody left of those who watched: housekeeping ends the broadcast when this runs out
    let ends_in = session
        .is_live()
        .then(|| session.activity.lock().unwrap().idle_since)
        .flatten()
        .map(|since| UNWATCHED_END.saturating_sub(since.elapsed()).as_secs_f64().ceil() as u64);
    let run = session.runner.snapshot();
    let host = &session.host;
    let admitted = session.admitted.lock().unwrap();
    let viewers: Vec<Value> = host
        .viewers()
        .into_iter()
        .map(|v| {
            let code = v.device_key.as_deref().map(device::code_of).unwrap_or_default();
            json!({
                "sid": v.sid,
                "name": v.name,
                "deviceCode": code,
                "verified": admitted.get(&v.sid).is_some_and(|a| a.device_bound),
                "state": if v.state == ViewerState::Watching { "watching" } else { "connecting" },
                "ipv6": v.ipv6,
                "path": v.path,
                "rttMs": v.rtt_ms,
                "latencyMs": v.latency_ms,
                "overheadPercent": v.overhead_percent,
                "net": v.srt.as_ref().map(|s| net_report::status(s, End::Host)),
                "control": session.hub.state_of(&v.sid),
                // control mode (§11): its own stream, and what the viewer reports of it
                "mode": if v.low_latency { "control" } else { "watch" },
                "ll": session.low_latency.viewer_status(&v.sid),
            })
        })
        .collect();
    drop(admitted);
    let knocks: Vec<Value> = session
        .knocks
        .lock()
        .unwrap()
        .iter()
        .map(|k| {
            json!({
                "sid": k.ticket.sid,
                "name": k.who.name,
                "deviceCode": k.who.device_key.as_deref().map(device::code_of).unwrap_or_default(),
                "verified": k.who.device_key.is_some(),
                "waitingSec": k.since.elapsed().as_secs(),
                "limitSec": KNOCK_LIMIT.as_secs(),
            })
        })
        .collect();
    json!({
        "mode": if session.is_live() { "live" } else { "available" },
        "state": run.state.name(),
        "detail": run.detail,
        "attempt": run.attempt,
        "uptimeSec": run.uptime.as_secs_f64(),
        "capture": run.capture.map(|c| json!({
            "fps": c.fps, "duplicated": c.duplicated, "dropped": c.dropped, "gpuMs": c.gpu_ms, "audioUnderrun": c.audio_underrun,
        })),
        "encode": run.progress.map(|p| json!({
            "fps": p.fps, "frame": p.frame, "bitrateKbps": p.bitrate_kbps, "speed": p.speed, "dropFrames": p.drop_frames,
            "inEngine": run.in_engine,
        })),
        // What is really being sent, as the engine announced it; the plan only says what was expected.
        "video": run.layout.map(|l| {
            let hdr = l.format == CaptureFormat::P010;
            json!({
                "hdr": hdr,
                "width": l.width,
                "height": l.height,
                "format": state::format_label(hdr, l.width, l.height),
                "hdrUnavailable": run.planned_hdr && !hdr,
            })
        }),
        "p2p": {
            "status": host.status(),
            "hasIpv6": host.has_ipv6(),
            "ipv4Open": host.ipv4_open(),
            "uploadMbps": host.upload_mbps(),
            "viewers": viewers,
        },
        "knocks": knocks,
        "problem": session.problem.lock().unwrap().clone(),
        "device": device,
        "endsInSec": ends_in,
        "endedUnwatched": ended_unwatched,
        "controlMode": {
            "offered": offers_control_mode(&app.inner.lock().unwrap(), Some(&session.low_latency)),
            "engine": session.low_latency.status(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hdrcast_capture::{EncodeArgs, EngineEncoder};

    /// A plan whose engine encodes at `kbps` (with the quarter second of VBV the settings give it).
    fn engine_plan(kbps: u32) -> SendPlan {
        let mut plan = SendPlan::default();
        plan.ffmpeg.encoder.bitrate_kbps = kbps;
        plan.engine_encode =
            Some(EncodeArgs { bitrate_kbps: kbps, vbv_kbits: kbps / 4, ..EncodeArgs::defaults(EngineEncoder::Nvenc, 60) });
        plan
    }

    #[test]
    fn a_new_bitrate_goes_to_an_engine_that_encodes_anything_else_restarts() {
        let running = engine_plan(40_000);
        assert!(matches!(plan_change(&running, &running.clone()), PlanChange::Same));
        assert!(matches!(
            plan_change(&running, &engine_plan(25_000)),
            PlanChange::Bitrate { kbps: 25_000, vbv_kbits: 6_250 }
        ));
        // the bitrate and anything else: a restart
        let mut more = engine_plan(25_000);
        more.ffmpeg.fps = 120;
        assert!(matches!(plan_change(&running, &more), PlanChange::Restart));
        // from the engine to ffmpeg (Keyframe on request switched off), and back
        let mut by_ffmpeg = running.clone();
        by_ffmpeg.engine_encode = None;
        assert!(matches!(plan_change(&running, &by_ffmpeg), PlanChange::Restart));
        assert!(matches!(plan_change(&by_ffmpeg, &running), PlanChange::Restart));
        // ffmpeg cannot change its bitrate while it runs
        let mut lower = by_ffmpeg.clone();
        lower.ffmpeg.encoder.bitrate_kbps = 25_000;
        assert!(matches!(plan_change(&by_ffmpeg, &lower), PlanChange::Restart));
    }

    #[test]
    fn a_broadcast_ends_once_everyone_who_watched_has_left_and_not_before() {
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        // live, with the switch on: nobody, somebody watching, somebody connecting (not idle, nobody's picture yet)
        let alone = |a: &mut Activity, s: u64| a.idle_long_enough(at(s), true, false, true, true);
        let watched = |a: &mut Activity, s: u64| a.idle_long_enough(at(s), false, true, true, true);
        let connecting = |a: &mut Activity, s: u64| a.idle_long_enough(at(s), false, false, true, true);

        // nobody has come yet: the broadcast waits for its viewers however long that takes
        let mut live = Activity::default();
        assert!(!alone(&mut live, 0));
        assert!(!alone(&mut live, 3600));
        // somebody who never got a picture does not count either
        assert!(!connecting(&mut live, 3601));
        assert!(!alone(&mut live, 3602));
        assert!(!alone(&mut live, 3700));
        // somebody watched and left: 2 s later it ends, unless somebody comes back first
        assert!(!watched(&mut live, 3701));
        assert!(!alone(&mut live, 3702));
        assert!(!alone(&mut live, 3703));
        assert!(!connecting(&mut live, 3703)); // back (joining again with a new delay, say): the wait starts over
        assert!(!alone(&mut live, 3704));
        assert!(!alone(&mut live, 3705));
        assert!(alone(&mut live, 3706));

        // a viewer being let in holds it, as on request
        let mut admitting = Activity { admitting: 1, watched: true, ..Activity::default() };
        assert!(!alone(&mut admitting, 0));
        assert!(!alone(&mut admitting, 60));

        // switched off: the broadcast goes on without viewers, as before
        let mut kept = Activity { watched: true, ..Activity::default() };
        assert!(!kept.idle_long_enough(at(0), true, false, true, false));
        assert!(!kept.idle_long_enough(at(600), true, false, true, false));

        // on request the encoder stops 20 s after the last viewer, whatever the switch says
        let mut on_request = Activity::default();
        assert!(!on_request.idle_long_enough(at(0), true, false, false, false));
        assert!(!on_request.idle_long_enough(at(2), true, false, false, true));
        assert!(!on_request.idle_long_enough(at(19), true, false, false, false));
        assert!(on_request.idle_long_enough(at(20), true, false, false, false));
    }
}
