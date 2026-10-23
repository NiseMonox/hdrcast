//! Sharing this computer's screen with the group (docs/design.md §2.7, §8.10).
//!
//! One P2P host announces this computer for as long as it is available or live. The capture engine and the
//! encoder ([`SenderRunner`]) run only while needed: while live (a broadcast, which everyone in the group who is
//! not blocked may watch), or on request, while somebody the rules let in is watching. A join waits for the
//! rules (`access.rs`), for the user's answer when the rules say to ask, and for the stream to flow before it is
//! answered, so that the viewer's player finds the stream at once. Remote control is registered per session,
//! with the key agreed with the viewer's device and the rule of that device.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use hdrcast_capture::{CaptureArgs, CaptureFormat};
use hdrcast_core::common::firewall::row as firewall_row;
use hdrcast_core::group::device;
use hdrcast_core::group::keys::GroupKeys;
use hdrcast_core::messages::JoinAnswer;
use hdrcast_media::sender::ffmpeg_command::{self, AudioCodec, FfmpegPlan};
use hdrcast_media::sender::settings::HdrMode;
use hdrcast_p2p::{JoinTicket, P2pHost, P2pHostOptions, SwitchableStream, ViewerState};
use serde_json::{json, Value};

use crate::access::{AccessSettings, Connect, Control};
use crate::commands;
use crate::control_host::ControlHub;
use crate::net_report::{self, End, NetReport};
use crate::sender_runner::{self, RunnerState, SendPlan, SenderRunner};
use crate::state::{self, AppState, Inner};

/// The encoder keeps running this long after the last viewer left, for a viewer who comes straight back.
const IDLE_STOP: Duration = Duration::from_secs(20);
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
    /// Since when nobody has been watching (while the encoder runs on request).
    idle_since: Option<Instant>,
}

struct Session {
    host: Arc<P2pHost>,
    ts: SwitchableStream,
    hub: Arc<ControlHub>,
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
}

#[derive(Default)]
pub struct Share {
    session: Mutex<Option<Arc<Session>>>,
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
    Ok(SendPlan { capture_exe: state::current_exe(), capture_argv, ffmpeg_exe: ffmpeg, ffmpeg: plan, pipe_names })
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
        Session {
            host,
            ts,
            hub,
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
        }
    });
    let hub = session.hub.clone();
    session.host.set_side_handler(Arc::new(move |sid, payload| hub.on_datagram(sid, payload)));

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
        self.hub.stop();
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
        let outcome = ensure_streaming(&app, &session).and_then(|()| session.host.admit(&ticket.sid));
        match outcome {
            Ok(device_key) => {
                let admitted = Admitted { who: who.clone(), device_bound: device_key.is_some() };
                let control = control_rule(&app.inner.lock().unwrap().access, &admitted);
                let key = device_key.unwrap_or_else(|| session.keys.control_key(&ticket.sid));
                session.hub.register(&ticket.sid, &who.name, &key, control.policy());
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

/// Starts capture and encoder from the current settings.
fn start_runner(app: &AppState, session: &Session) -> Result<(), Vec<String>> {
    let (plan, display, bitrate, max) = {
        let inner = app.inner.lock().unwrap();
        let plan = build_plan(app, &inner)?;
        let display = state::selected_output(&inner.sender, &inner.outputs).map(|o| o.device_name.clone()).unwrap_or_default();
        (plan, display, inner.sender.bitrate_bps(), inner.sender.max_viewers.max(1) as u32)
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
    }
    session.host.announce_now();
    app.log("[app] stopped sharing; still available to the group");
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

/// Runs with every status tick: requests that waited too long, an encoder that gave up, an encoder serving
/// nobody, and what the remote control and the "recently let in" memory should forget.
pub fn housekeeping(app: &Arc<AppState>) {
    let Some(session) = current(app) else { return };
    let viewers = session.host.viewers();
    let sids: Vec<String> = viewers.iter().map(|v| v.sid.clone()).collect();
    session.hub.keep_only(&sids);
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

    // On request: once nobody has been watching for a while, the encoder stops.
    let idle = !session.is_live()
        && matches!(state, RunnerState::Starting | RunnerState::Streaming)
        && viewers.is_empty()
        && session.knocks.lock().unwrap().is_empty();
    let stop_now = {
        let mut activity = session.activity.lock().unwrap();
        if !idle || activity.admitting > 0 {
            activity.idle_since = None;
            false
        } else {
            let since = *activity.idle_since.get_or_insert_with(Instant::now);
            since.elapsed() >= IDLE_STOP
        }
    };
    if stop_now {
        session.activity.lock().unwrap().idle_since = None;
        let (app, session) = (app.clone(), session.clone());
        thread::spawn(move || {
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
    let Some(session) = current(app) else {
        return json!({
            "mode": "off", "state": "idle", "detail": "", "attempt": 0, "uptimeSec": 0,
            "capture": null, "encode": null, "video": null, "p2p": null, "knocks": [], "problem": null, "device": device,
        });
    };
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
    })
}
