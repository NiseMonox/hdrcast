//! Owns the capture engine and the encoder ffmpeg for one sharing session (docs/design.md §2.5, §2.7).
//!
//! The capture engine is started first (it is the pipe server); once both pipes listen, ffmpeg is started on
//! them and its stdout, the MPEG-TS, is handed to the P2P fan-out. If either process exits the other is stopped
//! and, when asked to, the pair is started again after a pause; viewers stay connected across that.

use std::path::PathBuf;
use std::process::ChildStdout;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hdrcast_capture::CaptureStats;
use hdrcast_core::common::line_process::LineProcess;
use hdrcast_core::common::process_job::ProcessJob;
use hdrcast_media::sender::ffmpeg_progress::{FfmpegProgress, FfmpegProgressParser};

const RESTART_DELAY: Duration = Duration::from_millis(3000);
/// How long to wait for the capture engine's pipes before starting ffmpeg. The engine creates them only after
/// opening the GPU device and compiling the conversion shader, which on a cold start takes noticeably longer
/// than a fixed delay would allow, and ffmpeg exits at once when a pipe is not there yet.
const PIPE_WAIT: Duration = Duration::from_millis(15000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerState {
    Idle,
    Starting,
    Streaming,
    Stopping,
    Failed,
}

impl RunnerState {
    pub fn name(self) -> &'static str {
        match self {
            RunnerState::Idle => "idle",
            RunnerState::Starting => "starting",
            RunnerState::Streaming => "streaming",
            RunnerState::Stopping => "stopping",
            RunnerState::Failed => "failed",
        }
    }
}

/// Everything needed for one session; built from the settings plus the chosen display.
#[derive(Debug, Clone, Default)]
pub struct SendPlan {
    pub capture_exe: PathBuf,
    pub capture_argv: Vec<String>,
    pub ffmpeg_exe: PathBuf,
    pub ffmpeg_argv: Vec<String>,
    pub summary: String,
    /// Pipes the capture engine creates and ffmpeg opens; ffmpeg is not started until they listen.
    pub pipe_names: Vec<String>,
}

/// What the status display reads.
#[derive(Debug, Clone)]
pub struct RunnerSnapshot {
    pub state: RunnerState,
    pub detail: String,
    pub attempt: u32,
    pub uptime: Duration,
    pub capture: Option<CaptureStats>,
    pub progress: Option<FfmpegProgress>,
}

type LogFn = Arc<dyn Fn(&str) + Send + Sync>;
type TsFn = Arc<dyn Fn(ChildStdout) + Send + Sync>;

struct Gate {
    capture: Option<Arc<LineProcess>>,
    ffmpeg: Option<Arc<LineProcess>>,
    plan: Option<Arc<SendPlan>>,
    /// Bumped by every launch and every stop; callbacks of an older pair of processes see it and do nothing.
    generation: u64,
    /// The generation whose exit has been dealt with (a process can exit before its launch finished).
    exit_handled: u64,
    user_stopped: bool,
    auto_restart: bool,
    attempt: u32,
    started: Option<Instant>,
    state: RunnerState,
    detail: String,
    capture_stats: Option<CaptureStats>,
    progress: Option<FfmpegProgress>,
    parser: FfmpegProgressParser,
}

struct Shared {
    gate: Mutex<Gate>,
    job: Arc<ProcessJob>,
    log: LogFn,
    ts_started: TsFn,
}

#[derive(Clone)]
pub struct SenderRunner {
    shared: Arc<Shared>,
}

impl SenderRunner {
    /// `ts_started` receives the stdout of every (re)started ffmpeg: the stream for the fan-out.
    pub fn new(job: Arc<ProcessJob>, log: LogFn, ts_started: TsFn) -> Self {
        let gate = Gate {
            capture: None,
            ffmpeg: None,
            plan: None,
            generation: 0,
            exit_handled: 0,
            user_stopped: true,
            auto_restart: false,
            attempt: 0,
            started: None,
            state: RunnerState::Idle,
            detail: String::new(),
            capture_stats: None,
            progress: None,
            parser: FfmpegProgressParser::new(),
        };
        Self { shared: Arc::new(Shared { gate: Mutex::new(gate), job, log, ts_started }) }
    }

    pub fn snapshot(&self) -> RunnerSnapshot {
        let g = self.shared.gate.lock().unwrap();
        let running = matches!(g.state, RunnerState::Starting | RunnerState::Streaming);
        RunnerSnapshot {
            state: g.state,
            detail: g.detail.clone(),
            attempt: g.attempt,
            uptime: if running { g.started.map(|t| t.elapsed()).unwrap_or_default() } else { Duration::ZERO },
            capture: g.capture_stats,
            progress: g.progress.clone(),
        }
    }

    pub fn state(&self) -> RunnerState {
        self.shared.gate.lock().unwrap().state
    }

    /// Starts a session and returns at once: waiting for the capture engine's pipes can take seconds.
    pub fn start(&self, plan: SendPlan, auto_restart: bool) {
        self.stop();
        {
            let mut g = self.shared.gate.lock().unwrap();
            g.plan = Some(Arc::new(plan));
            g.auto_restart = auto_restart;
            g.user_stopped = false;
            g.attempt = 0;
            g.capture_stats = None;
            g.progress = None;
        }
        self.shared.set_state(RunnerState::Starting, "Starting…");
        let shared = self.shared.clone();
        thread::Builder::new().name("sender-launch".into()).spawn(move || Shared::launch(&shared)).ok();
    }

    /// Stops both processes and waits for them (a few seconds at most). Call it off the UI thread.
    pub fn stop(&self) {
        let (capture, ffmpeg, was_idle) = {
            let mut g = self.shared.gate.lock().unwrap();
            g.user_stopped = true;
            g.generation += 1;
            (g.capture.take(), g.ffmpeg.take(), g.state == RunnerState::Idle)
        };
        if capture.is_none() && ffmpeg.is_none() {
            if !was_idle {
                self.shared.set_state(RunnerState::Idle, "Stopped");
            }
            return;
        }
        self.shared.set_state(RunnerState::Stopping, "Stopping…");
        // ffmpeg first, so the engine's next pipe write fails and it shuts down on its own. This is a hard
        // kill, so the muxer is not flushed; that is fine for a live stream and keeps the stop bounded in time.
        if let Some(p) = &ffmpeg {
            p.stop();
        }
        if let Some(p) = &capture {
            p.stop();
        }
        self.shared.set_state(RunnerState::Idle, "Stopped");
    }
}

enum PipeWait {
    Ready,
    /// The session was stopped or replaced meanwhile.
    Cancelled,
    EngineExited,
    TimedOut,
}

impl Shared {
    fn emit(&self, line: &str) {
        (self.log)(line);
    }

    fn set_state(&self, state: RunnerState, detail: &str) {
        {
            let mut g = self.gate.lock().unwrap();
            if g.state == state && g.detail == detail {
                return;
            }
            g.state = state;
            g.detail = detail.to_string();
        }
        self.emit(&format!("[state] {}: {detail}", state.name()));
    }

    fn current(&self, gen: u64) -> bool {
        let g = self.gate.lock().unwrap();
        gen == g.generation && !g.user_stopped
    }

    fn launch(self: &Arc<Self>) {
        let (plan, gen, attempt) = {
            let mut g = self.gate.lock().unwrap();
            let Some(plan) = g.plan.clone() else { return };
            if g.user_stopped {
                return;
            }
            g.generation += 1;
            g.attempt += 1;
            (plan, g.generation, g.attempt)
        };
        let text = if attempt == 1 { "Starting…".to_string() } else { format!("Restarting (attempt {attempt})…") };
        self.set_state(RunnerState::Starting, &text);

        let mut capture = LineProcess::new(&plan.capture_exe, &plan.capture_argv);
        capture.job(Some(self.job.clone()));
        let me = self.clone();
        capture.on_stdout_line(move |line| me.on_capture_stats(gen, line));
        let me = self.clone();
        capture.on_stderr_line(move |line| me.emit(&format!("[capture] {line}")));
        let me = self.clone();
        capture.on_exited(move |code| me.on_capture_exited(gen, code));

        let mut ffmpeg = LineProcess::new(&plan.ffmpeg_exe, &plan.ffmpeg_argv);
        // stdout is the stream itself; -progress shares stderr with ffmpeg's log messages
        ffmpeg.job(Some(self.job.clone())).read_stdout_lines(false);
        let me = self.clone();
        ffmpeg.on_stderr_line(move |line| {
            if FfmpegProgressParser::is_progress_line(line) {
                me.on_ffmpeg_progress(gen, line);
            } else {
                me.emit(&format!("[ffmpeg] {line}"));
            }
        });
        let me = self.clone();
        ffmpeg.on_exited(move |code| me.on_ffmpeg_exited(gen, code));

        // The logged command lines have the passphrase masked: the log folder is meant to be shareable.
        self.emit(&format!("[run] {}", capture.safe_command_line()));
        self.emit(&format!("[run] {}", ffmpeg.safe_command_line()));

        if let Err(e) = capture.start() {
            self.emit(&format!("[run] failed to start: {e}"));
            self.fail(gen, &format!("could not start: {e}"));
            return;
        }
        let capture = Arc::new(capture);
        if !self.adopt(gen, |g| g.capture = Some(capture.clone())) {
            capture.stop();
            return;
        }

        self.set_state(RunnerState::Starting, "Waiting for the capture engine…");
        match self.wait_for_pipes(&plan, &capture, gen) {
            PipeWait::Ready => {}
            PipeWait::Cancelled => return,
            // its exit callback reports why and restarts
            PipeWait::EngineExited => return,
            PipeWait::TimedOut => {
                let why = format!("the capture engine did not open its pipes within {} s", PIPE_WAIT.as_secs());
                self.emit(&format!("[run] {why}"));
                // stopping it runs its exit callback, which restarts the session when that is wanted
                capture.stop();
                return;
            }
        }

        if let Err(e) = ffmpeg.start() {
            self.emit(&format!("[run] failed to start: {e}"));
            self.gate.lock().unwrap().auto_restart = false;
            capture.stop();
            self.fail(gen, &format!("could not start: {e}"));
            return;
        }
        let ffmpeg = Arc::new(ffmpeg);
        if !self.adopt(gen, |g| {
            g.ffmpeg = Some(ffmpeg.clone());
            g.started = Some(Instant::now());
        }) {
            ffmpeg.stop();
            return;
        }
        if let Some(stdout) = ffmpeg.take_stdout() {
            (self.ts_started)(stdout);
        }
        if self.current(gen) {
            self.set_state(RunnerState::Streaming, "Streaming");
        }
    }

    /// Records a started process in the gate. False when the session was stopped, replaced, or already dealt
    /// with as exited while it was being started: the caller then stops what it started.
    fn adopt(&self, gen: u64, store: impl FnOnce(&mut Gate)) -> bool {
        let mut g = self.gate.lock().unwrap();
        if g.user_stopped || gen != g.generation || g.exit_handled == gen {
            return false;
        }
        store(&mut g);
        true
    }

    /// Polls for the engine's pipes. Never by looking at the pipe as a file: on Windows, querying the attributes
    /// of `\\.\pipe\name` is itself a client connection; it takes the only instance, ffmpeg then gets
    /// ERROR_PIPE_BUSY ("Error opening input: Invalid argument") and the engine's writes fail.
    fn wait_for_pipes(&self, plan: &SendPlan, capture: &LineProcess, gen: u64) -> PipeWait {
        if plan.pipe_names.is_empty() {
            return PipeWait::Ready;
        }
        let begun = Instant::now();
        while begun.elapsed() < PIPE_WAIT {
            if !self.current(gen) {
                return PipeWait::Cancelled;
            }
            if capture.has_exited() {
                return PipeWait::EngineExited;
            }
            if plan.pipe_names.iter().all(|name| hdrcast_capture::pipe_listening(name)) {
                return PipeWait::Ready;
            }
            thread::sleep(Duration::from_millis(50));
        }
        PipeWait::TimedOut
    }

    fn on_capture_stats(&self, gen: u64, line: &str) {
        match CaptureStats::try_parse(line) {
            Some(stats) => {
                let mut g = self.gate.lock().unwrap();
                if gen == g.generation {
                    g.capture_stats = Some(stats);
                }
            }
            None => self.emit(&format!("[capture] {line}")),
        }
    }

    fn on_ffmpeg_progress(&self, gen: u64, line: &str) {
        let mut g = self.gate.lock().unwrap();
        if gen != g.generation {
            return;
        }
        if let Some(block) = g.parser.feed(line) {
            g.progress = Some(block);
        }
    }

    fn on_capture_exited(self: &Arc<Self>, gen: u64, code: i32) {
        if gen != self.gate.lock().unwrap().generation {
            return;
        }
        let reason = describe_capture_exit(code);
        self.emit(&format!("[capture] exited: {reason}"));
        self.stop_peer_and_maybe_restart(gen, &format!("capture engine stopped: {reason}"));
    }

    fn on_ffmpeg_exited(self: &Arc<Self>, gen: u64, code: i32) {
        if gen != self.gate.lock().unwrap().generation {
            return;
        }
        self.emit(&format!("[ffmpeg] exited with code {code}"));
        self.stop_peer_and_maybe_restart(gen, &format!("ffmpeg stopped (exit code {code})"));
    }

    fn stop_peer_and_maybe_restart(self: &Arc<Self>, gen: u64, reason: &str) {
        let (capture, ffmpeg, restart) = {
            let mut g = self.gate.lock().unwrap();
            if gen != g.generation || g.user_stopped || g.exit_handled == gen {
                return;
            }
            g.exit_handled = gen;
            (g.capture.take(), g.ffmpeg.take(), g.auto_restart)
        };
        if let Some(p) = &ffmpeg {
            p.stop();
        }
        if let Some(p) = &capture {
            p.stop();
        }
        if !restart {
            self.fail(gen, reason);
            return;
        }
        self.set_state(RunnerState::Starting, &format!("{reason} — restarting in {} s", RESTART_DELAY.as_secs()));
        let me = self.clone();
        thread::Builder::new()
            .name("sender-restart".into())
            .spawn(move || {
                thread::sleep(RESTART_DELAY);
                // A stop + start in the meantime made a new generation: this restart is not theirs.
                if me.current(gen) {
                    Shared::launch(&me);
                }
            })
            .ok();
    }

    fn fail(&self, gen: u64, detail: &str) {
        {
            let mut g = self.gate.lock().unwrap();
            if gen != g.generation || g.user_stopped {
                return;
            }
            g.progress = None;
        }
        self.set_state(RunnerState::Failed, detail);
    }
}

fn describe_capture_exit(code: i32) -> String {
    match code {
        0 => "finished".to_string(),
        2 => "bad arguments".to_string(),
        3 => "GPU / display error".to_string(),
        4 => "the pipe consumer went away (ffmpeg stopped)".to_string(),
        5 => "desktop duplication lost".to_string(),
        6 => "ffmpeg never connected to the capture pipe".to_string(),
        other => format!("exit code {other}"),
    }
}
