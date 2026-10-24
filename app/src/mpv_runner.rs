//! Owns the mpv (and optional ffmpeg) child processes of a viewing session: launches them from an
//! [`MpvLaunchPlan`], attaches the IPC client, retries every 3 s while the stream is not there yet, and kills the
//! tree on stop. Callbacks fire on worker threads.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hdrcast_core::common::line_process::LineProcess;
use hdrcast_core::common::process_job::ProcessJob;
use hdrcast_media::receiver::mpv_command::MpvLaunchPlan;
use hdrcast_media::receiver::mpv_ipc::{observed, MpvIpcMessage, MpvIpcMessageKind, MpvStatusModel};
use hdrcast_media::receiver::mpv_log;
use hdrcast_media::receiver::settings::ReceiverSettings;
use hdrcast_media::receiver::ts_guard::{GuardEvent, GuardStats, TsGuard};

use crate::mpv_ipc::{Handlers, MpvIpcClient};
use hdrcast_control::keys::{self, CLOSE_MESSAGE, OVERLAY_MESSAGE};
use serde_json::{json, Value};

pub const RETRY_DELAY: Duration = Duration::from_millis(3000);
const RECENT_LINES: usize = 40;
/// The guard's freezes go into the log at most this often (each one ends at a keyframe, a second or two apart
/// on a bad line): the first one at once, then a summary of what happened meanwhile.
const GUARD_LOG_EVERY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
    Idle,
    Starting,
    WaitingForStream,
    Playing,
    Stopping,
}

impl PlayerState {
    pub fn name(self) -> &'static str {
        match self {
            PlayerState::Idle => "idle",
            PlayerState::Starting => "starting",
            PlayerState::WaitingForStream => "waiting",
            PlayerState::Playing => "playing",
            PlayerState::Stopping => "stopping",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlayerSnapshot {
    pub state: PlayerState,
    pub text: String,
    pub attempt: u32,
    pub uptime: Duration,
    pub status: MpvStatusModel,
    /// What the guard did, when the stream goes through it (the ffmpeg pipe).
    pub guard: Option<GuardStats>,
}

type LogFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Sees every IPC message of the current mpv (after the runner has applied it), e.g. for remote control.
pub type IpcTap = Arc<dyn Fn(&MpvIpcMessage) + Send + Sync>;
/// Called each time a (re)started mpv's IPC is connected.
pub type IpcReady = Arc<dyn Fn() + Send + Sync>;
/// Sees what the guard does (freezes, recoveries), on the pump thread: keyframes on request use it.
pub type GuardTap = Arc<dyn Fn(GuardEvent) + Send + Sync>;

struct Gate {
    generation: u64,
    user_stopped: bool,
    attempt: u32,
    mpv: Option<Arc<LineProcess>>,
    ffmpeg: Option<Arc<LineProcess>>,
    ipc: Option<MpvIpcClient>,
    plan: Option<Arc<MpvLaunchPlan>>,
    settings: Option<ReceiverSettings>,
    catching_up: bool,
    started: Option<Instant>,
    state: PlayerState,
    text: String,
    status: MpvStatusModel,
    recent: VecDeque<String>,
    guard: Option<GuardStats>,
}

/// Hears what the player's own bindings send (hdrcast_control::keys::player_bindings): the panel's key
/// ([`OVERLAY_MESSAGE`]) and the window's close button ([`CLOSE_MESSAGE`]). Called on the IPC thread.
pub type PlayerMessage = Arc<dyn Fn(&str) + Send + Sync>;

struct Shared {
    gate: Mutex<Gate>,
    job: Arc<ProcessJob>,
    log: LogFn,
    tap: Mutex<Option<IpcTap>>,
    ready: Mutex<Option<IpcReady>>,
    guard_tap: Mutex<Option<GuardTap>>,
    player_message: Mutex<Option<PlayerMessage>>,
}

#[derive(Clone)]
pub struct MpvRunner {
    shared: Arc<Shared>,
}

impl MpvRunner {
    pub fn new(job: Arc<ProcessJob>, log: LogFn) -> Self {
        let gate = Gate {
            generation: 0,
            user_stopped: true,
            attempt: 0,
            mpv: None,
            ffmpeg: None,
            ipc: None,
            plan: None,
            settings: None,
            catching_up: false,
            started: None,
            state: PlayerState::Idle,
            text: "Idle".to_string(),
            status: MpvStatusModel::new(),
            recent: VecDeque::new(),
            guard: None,
        };
        Self {
            shared: Arc::new(Shared {
                gate: Mutex::new(gate),
                job,
                log,
                tap: Mutex::new(None),
                ready: Mutex::new(None),
                guard_tap: Mutex::new(None),
                player_message: Mutex::new(None),
            }),
        }
    }

    /// Who hears the player's own bindings (the panel's key, the close button); replaces the previous one.
    pub fn on_player_message(&self, handler: Option<PlayerMessage>) {
        *self.shared.player_message.lock().unwrap() = handler;
    }

    /// The process id of the mpv that runs now, if any: its window is where the panel goes.
    pub fn mpv_pid(&self) -> Option<u32> {
        self.shared.gate.lock().unwrap().mpv.as_ref().and_then(|mpv| mpv.id())
    }

    /// Switches the running mpv to another decoder (`--hwdec` value) without a restart, and keeps it for the mpv
    /// that a reconnect starts. False when no mpv is connected to take it now.
    pub fn set_hwdec(&self, value: &str) -> bool {
        self.set_live("hwdec", value, json!(value))
    }

    /// Mutes or unmutes the running mpv, and the one a reconnect starts. False when no mpv is connected to take it now.
    pub fn set_mute(&self, muted: bool) -> bool {
        self.set_live("mute", if muted { "yes" } else { "no" }, json!(muted))
    }

    /// Sets an mpv option for the mpv that runs (as the property `name` = `now`) and for the next one (`--name=arg`).
    fn set_live(&self, name: &str, arg: &str, now: Value) -> bool {
        let ipc = {
            let mut g = self.shared.gate.lock().unwrap();
            if let Some(plan) = g.plan.as_deref() {
                let mut plan = plan.clone();
                set_option(&mut plan, name, arg);
                g.plan = Some(Arc::new(plan));
            }
            g.ipc.clone()
        };
        ipc.is_some_and(|ipc| ipc.is_connected() && ipc.set_property(name, now))
    }

    pub fn set_guard_tap(&self, tap: Option<GuardTap>) {
        *self.shared.guard_tap.lock().unwrap() = tap;
    }

    pub fn set_ipc_tap(&self, tap: Option<IpcTap>, ready: Option<IpcReady>) {
        *self.shared.tap.lock().unwrap() = tap;
        *self.shared.ready.lock().unwrap() = ready;
    }

    pub fn is_running(&self) -> bool {
        !self.shared.gate.lock().unwrap().user_stopped
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        let g = self.shared.gate.lock().unwrap();
        PlayerSnapshot {
            state: g.state,
            text: g.text.clone(),
            attempt: g.attempt,
            uptime: if g.user_stopped { Duration::ZERO } else { g.started.map(|t| t.elapsed()).unwrap_or_default() },
            status: g.status.clone(),
            guard: g.guard,
        }
    }

    /// False when a player is already running.
    pub fn start(&self, settings: ReceiverSettings, plan: MpvLaunchPlan) -> bool {
        {
            let mut g = self.shared.gate.lock().unwrap();
            if !g.user_stopped {
                return false;
            }
            g.settings = Some(settings);
            g.plan = Some(Arc::new(plan));
            g.user_stopped = false;
            g.attempt = 0;
            g.started = Some(Instant::now());
            g.status.reset();
        }
        Shared::launch(&self.shared);
        true
    }

    /// Stops the player and waits for it (seconds). Call it off the UI thread.
    pub fn stop(&self) {
        let (gen, mpv, ffmpeg, ipc) = {
            let mut g = self.shared.gate.lock().unwrap();
            if g.user_stopped && g.mpv.is_none() {
                return;
            }
            g.user_stopped = true;
            g.generation += 1;
            (g.generation, g.mpv.take(), g.ffmpeg.take(), g.ipc.take())
        };
        self.shared.set_state(PlayerState::Stopping, "Stopping…");
        // Ask mpv politely first so it tears down the swapchain cleanly, then kill whatever is left.
        if let (Some(ipc), Some(mpv)) = (&ipc, &mpv) {
            if ipc.is_connected() && ipc.quit() {
                mpv.wait_for_exit(Duration::from_millis(1500));
            }
        }
        if let Some(p) = &ffmpeg {
            p.stop();
        }
        if let Some(p) = &mpv {
            p.stop();
        }
        if let Some(ipc) = &ipc {
            ipc.dispose();
        }
        // Killing mpv takes seconds. If Play was pressed again in the meantime a newer generation owns the state
        // now, and resetting it here would blank the status of a session that is actually running.
        {
            let mut g = self.shared.gate.lock().unwrap();
            if gen != g.generation {
                return;
            }
            g.status.reset();
        }
        self.shared.set_state(PlayerState::Idle, "Idle");
    }

    pub fn toggle_fullscreen(&self) {
        match self.ipc() {
            Some(ipc) if ipc.is_connected() => {
                ipc.cycle_property("fullscreen");
            }
            _ => self.shared.emit("[ipc] not connected: cannot toggle fullscreen"),
        }
    }

    /// The IPC connection of the mpv that is running now, if any.
    pub fn ipc(&self) -> Option<MpvIpcClient> {
        self.shared.gate.lock().unwrap().ipc.clone()
    }
}

impl Shared {
    fn emit(&self, line: &str) {
        (self.log)(line);
    }

    fn set_state(&self, state: PlayerState, text: &str) {
        let mut g = self.gate.lock().unwrap();
        g.state = state;
        g.text = text.to_string();
    }

    fn launch(self: &Arc<Self>) {
        let (gen, plan, attempt) = {
            let mut g = self.gate.lock().unwrap();
            let Some(plan) = g.plan.clone() else { return };
            if g.user_stopped {
                return;
            }
            g.generation += 1;
            g.attempt += 1;
            g.catching_up = false;
            g.recent.clear();
            g.guard = None;
            (g.generation, plan, g.attempt)
        };
        let text = if attempt == 1 { "Starting mpv…".to_string() } else { format!("Reconnecting (attempt {attempt})…") };
        self.set_state(PlayerState::Starting, &text);

        let mut mpv = LineProcess::new(&plan.mpv_exe, &plan.mpv_args);
        mpv.job(Some(self.job.clone())).redirect_stdin(plan.uses_ffmpeg_pipe());
        let me = self.clone();
        mpv.on_stdout_line(move |line| me.on_mpv_line(gen, line));
        let me = self.clone();
        mpv.on_stderr_line(move |line| me.on_mpv_line(gen, line));
        let me = self.clone();
        mpv.on_exited(move |code| me.on_mpv_exited(gen, code));

        let mut ffmpeg = match (&plan.ffmpeg_exe, &plan.ffmpeg_args) {
            (Some(exe), Some(args)) => {
                let mut p = LineProcess::new(exe, args);
                p.job(Some(self.job.clone())).read_stdout_lines(false);
                let me = self.clone();
                p.on_stderr_line(move |line| me.emit(&format!("[ffmpeg] {line}")));
                let me = self.clone();
                // mpv sees the end of its input and exits by itself; its exit schedules the retry
                p.on_exited(move |code| {
                    if me.current(gen) {
                        me.emit(&format!("[ffmpeg] exited with code {code}"));
                    }
                });
                Some(p)
            }
            _ => None,
        };

        // The log folder is meant to be shareable, so the passphrase is masked in the logged command lines.
        self.emit(&format!("[run] {}", mpv.safe_command_line()));
        if let Some(p) = &ffmpeg {
            self.emit(&format!("[run] {}", p.safe_command_line()));
        }
        self.set_state(PlayerState::WaitingForStream, "Connecting…");

        let started = mpv.start().and_then(|()| match ffmpeg.as_mut() {
            Some(p) => p.start(),
            None => Ok(()),
        });
        if let Err(e) = started {
            self.emit(&format!("[run] failed to start: {e}"));
            mpv.stop();
            if let Some(p) = &ffmpeg {
                p.stop();
            }
            self.schedule_retry(gen, &format!("could not start: {e}"));
            return;
        }
        let mpv = Arc::new(mpv);
        let ffmpeg = ffmpeg.map(Arc::new);
        {
            let mut g = self.gate.lock().unwrap();
            if g.user_stopped || gen != g.generation {
                drop(g);
                if let Some(p) = &ffmpeg {
                    p.stop();
                }
                mpv.stop();
                return;
            }
            g.mpv = Some(mpv.clone());
            g.ffmpeg = ffmpeg.clone();
        }
        if let Some(p) = &ffmpeg {
            self.start_pump(gen, p, &mpv);
        }

        let me = self.clone();
        let me2 = self.clone();
        let me3 = self.clone();
        let me4 = self.clone();
        let mpv_alive = mpv.clone();
        let ipc = MpvIpcClient::start(
            &plan.ipc_endpoint,
            Handlers {
                log: Box::new(move |line| me.emit(&format!("[ipc] {line}"))),
                message: Box::new(move |msg| me2.on_ipc_message(gen, msg)),
                connected: Box::new(move || {
                    me3.emit("[ipc] observing mpv properties");
                    me3.bind_player_keys();
                    let ready = me3.ready.lock().unwrap().clone();
                    if let Some(ready) = ready {
                        ready();
                    }
                }),
            },
            move || me4.current(gen) && !mpv_alive.has_exited(),
        );
        let mut g = self.gate.lock().unwrap();
        if gen != g.generation || mpv.has_exited() {
            drop(g);
            ipc.dispose();
            return;
        }
        g.ipc = Some(ipc);
    }

    fn current(&self, gen: u64) -> bool {
        let g = self.gate.lock().unwrap();
        gen == g.generation && !g.user_stopped
    }

    /// The ffmpeg pipe: ffmpeg's stdout through the guard (docs/design.md §3.2) into mpv's stdin.
    fn start_pump(self: &Arc<Self>, gen: u64, ffmpeg: &LineProcess, mpv: &LineProcess) {
        let (Some(mut src), Some(mut dst)) = (ffmpeg.take_stdout(), mpv.take_stdin()) else { return };
        let me = self.clone();
        thread::Builder::new()
            .name("ffmpeg-to-mpv".into())
            .spawn(move || {
                let mut guard = TsGuard::new();
                let mut journal = GuardJournal::default();
                let mut buf = vec![0u8; 65536];
                let mut out = Vec::with_capacity(2 * buf.len());
                loop {
                    if !me.current(gen) {
                        break;
                    }
                    let n = match src.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(e) => {
                            if me.current(gen) {
                                me.emit(&format!("[pipe] ended: {e}"));
                            }
                            break;
                        }
                    };
                    out.clear();
                    guard.push(&buf[..n], &mut out);
                    let stats = guard.stats();
                    let events = guard.take_events();
                    if !events.is_empty() {
                        let tap = me.guard_tap.lock().unwrap().clone();
                        if let Some(tap) = tap {
                            for event in &events {
                                tap(*event);
                            }
                        }
                    }
                    if let Some(line) = journal.note(&events, &stats) {
                        me.emit(&line);
                    }
                    {
                        let mut g = me.gate.lock().unwrap();
                        if g.generation == gen {
                            g.guard = Some(stats);
                        }
                    }
                    if let Err(e) = dst.write_all(&out) {
                        if me.current(gen) {
                            me.emit(&format!("[pipe] ended: {e}"));
                        }
                        break;
                    }
                }
                guard.finish();
                if let Some(line) = journal.flush(&guard.stats()) {
                    me.emit(&line);
                }
                // dropping `dst` closes mpv's stdin, which ends its playback
            })
            .ok();
    }

    fn on_mpv_line(&self, gen: u64, line: &str) {
        {
            let mut g = self.gate.lock().unwrap();
            if gen != g.generation {
                return;
            }
            if g.recent.len() >= RECENT_LINES {
                g.recent.pop_front();
            }
            g.recent.push_back(line.to_string());
        }
        self.emit(&format!("[mpv] {line}"));
    }

    fn on_mpv_exited(self: &Arc<Self>, gen: u64, code: i32) {
        self.emit(&format!("[mpv] exited with code {code}"));
        let (ffmpeg, ipc, why) = {
            let mut g = self.gate.lock().unwrap();
            if g.user_stopped || gen != g.generation {
                return;
            }
            let reason = mpv_log::classify(g.recent.iter());
            g.mpv = None;
            g.status.reset();
            (g.ffmpeg.take(), g.ipc.take(), mpv_log::describe(reason))
        };
        if let Some(p) = &ffmpeg {
            p.stop();
        }
        if let Some(ipc) = &ipc {
            ipc.dispose();
        }
        self.schedule_retry(gen, why);
    }

    fn schedule_retry(self: &Arc<Self>, gen: u64, why: &str) {
        let attempt = {
            let g = self.gate.lock().unwrap();
            if g.user_stopped || gen != g.generation {
                return;
            }
            g.attempt
        };
        self.set_state(
            PlayerState::WaitingForStream,
            &format!("Waiting for stream… ({why}; retry in {} s, attempt {attempt})", RETRY_DELAY.as_secs()),
        );
        let me = self.clone();
        thread::Builder::new()
            .name("mpv-retry".into())
            .spawn(move || {
                thread::sleep(RETRY_DELAY);
                if me.current(gen) {
                    Shared::launch(&me);
                }
            })
            .ok();
    }

    fn on_ipc_message(&self, gen: u64, msg: &MpvIpcMessage) {
        let mut note = None;
        let mut speed = None;
        let mut loaded = false;
        let mut player_message = None;
        {
            let mut g = self.gate.lock().unwrap();
            if gen != g.generation {
                return;
            }
            match msg.kind {
                MpvIpcMessageKind::Event => match msg.event.as_deref() {
                    Some("property-change") => {
                        if g.status.apply(msg) {
                            let name = msg.name.as_deref().unwrap_or("");
                            if name == observed::DEMUXER_CACHE_DURATION {
                                speed = Self::catch_up(&mut g);
                            }
                            if name == observed::PAUSED_FOR_CACHE && g.state == PlayerState::Playing {
                                g.text = if g.status.paused_for_cache { "Playing (buffering…)" } else { "Playing" }.to_string();
                            }
                        }
                    }
                    Some("file-loaded") => {
                        g.state = PlayerState::Playing;
                        g.text = "Playing".to_string();
                        loaded = true;
                    }
                    Some("client-message") => {
                        let first = msg.raw.get("args").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str);
                        player_message = first.filter(|m| [OVERLAY_MESSAGE, CLOSE_MESSAGE].contains(m)).map(str::to_string);
                    }
                    Some("end-file") => note = Some(format!("[ipc] end-file: {}", msg.reason.as_deref().unwrap_or("?"))),
                    Some("shutdown") => note = Some("[ipc] mpv shutdown".to_string()),
                    _ => {}
                },
                MpvIpcMessageKind::Reply if !msg.is_success() => {
                    note = Some(format!("[ipc] command failed: {}", msg.error.as_deref().unwrap_or("?")));
                }
                _ => {}
            }
        }
        if let Some((value, line)) = speed {
            let ipc = self.gate.lock().unwrap().ipc.clone();
            if let Some(ipc) = ipc {
                ipc.set_property("speed", value);
            }
            self.emit(&line);
        }
        if let Some(line) = note {
            self.emit(&line);
        }
        if loaded {
            // again: at connection time the IPC client may not have been stored yet
            self.bind_player_keys();
        }
        if let Some(message) = player_message {
            let handler = self.player_message.lock().unwrap().clone();
            if let Some(handler) = handler {
                handler(&message);
            }
        }
        let tap = self.tap.lock().unwrap().clone();
        if let Some(tap) = tap {
            tap(msg);
        }
    }

    /// The few bindings the player keeps (mpv's own are off, `--input-default-bindings=no`; docs/design.md §3.1).
    fn bind_player_keys(&self) {
        let ipc = self.gate.lock().unwrap().ipc.clone();
        let Some(ipc) = ipc.filter(|ipc| ipc.is_connected()) else { return };
        for (key, command) in keys::player_bindings() {
            ipc.command(&[json!("keybind"), json!(key), json!(command)]);
        }
    }

    /// Live catch-up: play at 1.05x while the demuxer cache exceeds buffer + 1.5 s, back to 1x at the target.
    /// Returns the speed to set and the log line for it.
    fn catch_up(g: &mut Gate) -> Option<(f64, String)> {
        let settings = g.settings.as_ref()?;
        if !settings.auto_catch_up || g.status.paused_for_cache {
            return None;
        }
        if !g.ipc.as_ref().is_some_and(|ipc| ipc.is_connected()) {
            return None;
        }
        let cache = g.status.cache_seconds?;
        let target = settings.buffer_seconds;
        if !g.catching_up && cache > target + 1.5 {
            g.catching_up = true;
            Some((1.05, format!("[ipc] cache {cache:.1} s > target: catching up at 1.05x")))
        } else if g.catching_up && cache <= target {
            g.catching_up = false;
            Some((1.0, "[ipc] back at 1.0x".to_string()))
        } else {
            None
        }
    }
}

/// Puts `--name=value` in the plan's mpv arguments, in place of the one there or before the input (the last one).
pub fn set_option(plan: &mut MpvLaunchPlan, name: &str, value: &str) {
    let prefix = format!("--{name}=");
    let arg = format!("{prefix}{value}");
    match plan.mpv_args.iter_mut().find(|a| a.starts_with(&prefix)) {
        Some(existing) => *existing = arg,
        None => {
            let input = plan.mpv_args.len().saturating_sub(1);
            plan.mpv_args.insert(input, arg);
        }
    }
}

/// Turns the guard's freezes into log lines a person can read: the first one at once, then at most one line per
/// [`GUARD_LOG_EVERY`] summing up what happened since.
#[derive(Default)]
struct GuardJournal {
    last_line: Option<Instant>,
    /// Since the last line: freezes that ended, the frames they kept off the screen and how long (90 kHz ticks).
    freezes: u64,
    skipped: u64,
    ticks: u64,
    /// The guard's loss count at the last line.
    losses: u64,
}

impl GuardJournal {
    fn note(&mut self, events: &[GuardEvent], stats: &GuardStats) -> Option<String> {
        let mut froze = false;
        for event in events {
            match *event {
                GuardEvent::Froze => froze = true,
                GuardEvent::Recovered { skipped, ticks } => {
                    self.freezes += 1;
                    self.skipped += skipped;
                    self.ticks += ticks;
                }
            }
        }
        if froze && self.last_line.is_none() {
            self.last_line = Some(Instant::now());
            return Some(
                "[guard] part of the stream was lost on the way: the picture holds on the last good frame until the next keyframe"
                    .to_string(),
            );
        }
        let due = self.last_line.is_none_or(|at| at.elapsed() >= GUARD_LOG_EVERY);
        (due && self.freezes > 0).then(|| self.line(stats))
    }

    /// What is left to say when the stream ends.
    fn flush(&mut self, stats: &GuardStats) -> Option<String> {
        (self.freezes > 0).then(|| self.line(stats))
    }

    fn line(&mut self, stats: &GuardStats) -> String {
        let line = format!(
            "[guard] {} {} until a keyframe, {:.1} s in all ({} frames held back); {} losses in the stream",
            self.freezes,
            if self.freezes == 1 { "freeze" } else { "freezes" },
            self.ticks as f64 / 90_000.0,
            self.skipped,
            stats.losses.saturating_sub(self.losses),
        );
        self.last_line = Some(Instant::now());
        self.losses = stats.losses;
        (self.freezes, self.skipped, self.ticks) = (0, 0, 0);
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_option_replaces_its_old_value_or_goes_before_the_input() {
        let mut plan = MpvLaunchPlan {
            mpv_exe: "mpv".into(),
            mpv_args: ["--vo=gpu-next", "--hwdec=d3d11va", "--title=HdrCast", "-"].map(String::from).to_vec(),
            ipc_endpoint: String::new(),
            url: String::new(),
            ffmpeg_exe: None,
            ffmpeg_args: None,
        };
        set_option(&mut plan, "hwdec", "no");
        set_option(&mut plan, "mute", "yes");
        set_option(&mut plan, "mute", "no");
        assert_eq!(plan.mpv_args, ["--vo=gpu-next", "--hwdec=no", "--title=HdrCast", "--mute=no", "-"]);
    }
}
