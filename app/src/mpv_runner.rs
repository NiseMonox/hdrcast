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

use crate::mpv_ipc::{Handlers, MpvIpcClient};

pub const RETRY_DELAY: Duration = Duration::from_millis(3000);
const RECENT_LINES: usize = 40;

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
}

type LogFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Sees every IPC message of the current mpv (after the runner has applied it), e.g. for remote control.
pub type IpcTap = Arc<dyn Fn(&MpvIpcMessage) + Send + Sync>;
/// Called each time a (re)started mpv's IPC is connected.
pub type IpcReady = Arc<dyn Fn() + Send + Sync>;

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
}

struct Shared {
    gate: Mutex<Gate>,
    job: Arc<ProcessJob>,
    log: LogFn,
    tap: Mutex<Option<IpcTap>>,
    ready: Mutex<Option<IpcReady>>,
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
        };
        Self { shared: Arc::new(Shared { gate: Mutex::new(gate), job, log, tap: Mutex::new(None), ready: Mutex::new(None) }) }
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

    /// The ffmpeg pipe frontend: ffmpeg's stdout into mpv's stdin.
    fn start_pump(self: &Arc<Self>, gen: u64, ffmpeg: &LineProcess, mpv: &LineProcess) {
        let (Some(mut src), Some(mut dst)) = (ffmpeg.take_stdout(), mpv.take_stdin()) else { return };
        let me = self.clone();
        thread::Builder::new()
            .name("ffmpeg-to-mpv".into())
            .spawn(move || {
                let mut buf = vec![0u8; 65536];
                loop {
                    if !me.current(gen) {
                        break;
                    }
                    match src.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Err(e) = dst.write_all(&buf[..n]) {
                                if me.current(gen) {
                                    me.emit(&format!("[pipe] ended: {e}"));
                                }
                                break;
                            }
                        }
                        Err(e) => {
                            if me.current(gen) {
                                me.emit(&format!("[pipe] ended: {e}"));
                            }
                            break;
                        }
                    }
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
        let tap = self.tap.lock().unwrap().clone();
        if let Some(tap) = tap {
            tap(msg);
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
