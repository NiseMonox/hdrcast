//! Owns the capture engine and the encoder ffmpeg for one sharing session (docs/design.md §2.5, §2.7).
//!
//! The capture engine is started first (it is the pipe server). It says which frames it is going to send (its
//! layout line: pixel format and size) before it creates the pipes; once both pipes listen, ffmpeg is started
//! on them for exactly those frames and its stdout, the MPEG-TS, is handed to the P2P fan-out. If either
//! process exits the other is stopped and, when asked to, the pair is started again after a pause; viewers
//! stay connected across that.
//!
//! When the plan says so the engine encodes itself (§2.8): ffmpeg then only adds the sound and copies the video,
//! and the engine's stdin takes commands (a keyframe now, another bitrate). An engine that cannot encode here
//! exits with `ENCODER_UNAVAILABLE`; the session goes on at once with ffmpeg encoding, as before.

use std::io::Write;
use std::path::PathBuf;
use std::process::{ChildStdin, ChildStdout};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hdrcast_capture::{exit_codes, CaptureFormat, CaptureStats, EncodeArgs, EncoderEvent, FrameLayout};
use hdrcast_core::common::line_process::LineProcess;
use hdrcast_core::common::process_job::ProcessJob;
use hdrcast_media::sender::encoder_profiles;
use hdrcast_media::sender::ffmpeg_command::{self, FfmpegPlan, VideoInput};
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
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SendPlan {
    pub capture_exe: PathBuf,
    pub capture_argv: Vec<String>,
    pub ffmpeg_exe: PathBuf,
    /// The encoder as planned from the display's state. Its pixel format, size and HDR flag give way to what
    /// the engine announces ([`FfmpegPlan::for_frames`]) before ffmpeg is started.
    pub ffmpeg: FfmpegPlan,
    /// Pipes the capture engine creates and ffmpeg opens; ffmpeg is not started until they listen.
    pub pipe_names: Vec<String>,
    /// The engine encodes itself with these options (`capture_argv` gets them appended, and ffmpeg copies the
    /// video). `None` = ffmpeg encodes.
    pub engine_encode: Option<EncodeArgs>,
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
    /// The frames the engine of the current attempt announced.
    pub layout: Option<FrameLayout>,
    /// The session was planned as HDR (the display was in HDR mode and HDR was not switched off).
    pub planned_hdr: bool,
    /// The capture engine encodes (and makes keyframes on request); false while ffmpeg does.
    pub in_engine: bool,
}

/// What the runner tells about its encoder, on one of its threads.
#[derive(Debug, Clone, PartialEq)]
pub enum EncoderNews {
    /// An encoder started: inside the capture engine (keyframes on request work) or in ffmpeg.
    Started { in_engine: bool },
    /// The engine's encoder said something (a keyframe went out, a bitrate was taken or not).
    Event(EncoderEvent),
}

type NewsFn = Arc<dyn Fn(&EncoderNews) + Send + Sync>;

/// "HEVC (NVIDIA NVENC) 40 Mbps 3840x2160@60 HDR10": what is being sent, for the log.
pub fn summary(plan: &FfmpegPlan) -> String {
    format!(
        "{} {} Mbps {}x{}@{} {}",
        encoder_profiles::info(plan.encoder.encoder).display_name,
        plan.encoder.bitrate_kbps / 1000,
        plan.width,
        plan.height,
        plan.fps,
        if plan.pixel_format == CaptureFormat::P010 { "HDR10" } else { "SDR" }
    )
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
    /// What the engine of the current generation announced; `None` until it did.
    layout: Option<FrameLayout>,
    /// The current generation's engine encodes.
    in_engine: bool,
    /// The engine said it cannot encode here: ffmpeg encodes for the rest of this session.
    engine_unavailable: bool,
}

struct Shared {
    gate: Mutex<Gate>,
    /// The stdin of the engine that encodes, where the commands go. A lock of its own: a write never holds the gate.
    commands: Mutex<Option<ChildStdin>>,
    job: Arc<ProcessJob>,
    log: LogFn,
    ts_started: TsFn,
    news: Mutex<Option<NewsFn>>,
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
            layout: None,
            in_engine: false,
            engine_unavailable: false,
        };
        Self { shared: Arc::new(Shared { gate: Mutex::new(gate), commands: Mutex::new(None), job, log, ts_started, news: Mutex::new(None) }) }
    }

    /// Who hears about the encoder ([`EncoderNews`]); replaces the previous listener.
    pub fn on_news(&self, listener: impl Fn(&EncoderNews) + Send + Sync + 'static) {
        *self.shared.news.lock().unwrap() = Some(Arc::new(listener));
    }

    /// Asks the engine for a keyframe now. False when no engine that encodes is running.
    pub fn request_keyframe(&self) -> bool {
        self.shared.command("keyframe")
    }

    /// Asks the engine to encode at `kbps` with a buffer of `vbv_kbits` from now on, without a restart (the answer
    /// comes as an [`EncoderNews::Event`]). False when no engine that encodes is running.
    pub fn set_bitrate(&self, kbps: u32, vbv_kbits: u32) -> bool {
        self.shared.command(&format!("bitrate {kbps} {vbv_kbits}"))
    }

    /// The plan of the session that runs (or last ran).
    pub fn plan(&self) -> Option<SendPlan> {
        self.shared.gate.lock().unwrap().plan.as_deref().cloned()
    }

    /// The running session took a change without a restart (a bitrate the engine applied): `plan` describes it now,
    /// and an automatic restart after a crash starts from it.
    pub fn adopt_plan(&self, plan: SendPlan) {
        let mut g = self.shared.gate.lock().unwrap();
        if g.plan.is_some() {
            g.plan = Some(Arc::new(plan));
        }
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
            layout: g.layout,
            planned_hdr: g.plan.as_ref().is_some_and(|p| p.ffmpeg.encoder.hdr),
            in_engine: running && g.in_engine,
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
            g.engine_unavailable = false;
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
        *self.shared.commands.lock().unwrap() = None;
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

enum EngineWait {
    /// The engine announced its frames and both pipes listen.
    Ready(FrameLayout),
    /// The session was stopped or replaced meanwhile.
    Cancelled,
    EngineExited,
    /// Carries what was missing, for the log.
    TimedOut(String),
}

impl Shared {
    fn emit(&self, line: &str) {
        (self.log)(line);
    }

    fn tell(&self, news: &EncoderNews) {
        let listener = self.news.lock().unwrap().clone();
        if let Some(listener) = listener {
            listener(news);
        }
    }

    /// One line to the engine's stdin; false when there is no engine that takes commands.
    fn command(&self, line: &str) -> bool {
        let mut commands = self.commands.lock().unwrap();
        let Some(stdin) = commands.as_mut() else { return false };
        if stdin.write_all(format!("{line}\n").as_bytes()).and_then(|()| stdin.flush()).is_ok() {
            return true;
        }
        *commands = None; // the engine is gone; its exit is dealt with elsewhere
        false
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
        let (plan, gen, attempt, in_engine) = {
            let mut g = self.gate.lock().unwrap();
            let Some(plan) = g.plan.clone() else { return };
            if g.user_stopped {
                return;
            }
            g.generation += 1;
            g.attempt += 1;
            g.layout = None;
            g.in_engine = plan.engine_encode.is_some() && !g.engine_unavailable;
            (plan, g.generation, g.attempt, g.in_engine)
        };
        let text = if attempt == 1 { "Starting…".to_string() } else { format!("Restarting (attempt {attempt})…") };
        self.set_state(RunnerState::Starting, &text);

        let mut argv = plan.capture_argv.clone();
        if let (true, Some(encode)) = (in_engine, &plan.engine_encode) {
            argv.extend(encode.to_argv());
        }
        let mut capture = LineProcess::new(&plan.capture_exe, &argv);
        // the engine that encodes takes its commands on stdin
        capture.job(Some(self.job.clone())).redirect_stdin(in_engine);
        let me = self.clone();
        capture.on_stdout_line(move |line| me.on_capture_line(gen, line));
        let me = self.clone();
        capture.on_stderr_line(move |line| me.emit(&format!("[capture] {line}")));
        let me = self.clone();
        capture.on_exited(move |code| me.on_capture_exited(gen, code));

        // The logged command lines have the passphrase masked: the log folder is meant to be shareable.
        self.emit(&format!("[run] {}", capture.safe_command_line()));

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
        // commands from now on go to this engine (none when ffmpeg encodes: its stdin was not taken)
        *self.commands.lock().unwrap() = capture.take_stdin();

        self.set_state(RunnerState::Starting, "Waiting for the capture engine…");
        let layout = match self.wait_for_engine(&plan, &capture, gen) {
            EngineWait::Ready(layout) => layout,
            EngineWait::Cancelled => return,
            // its exit callback reports why and restarts
            EngineWait::EngineExited => return,
            EngineWait::TimedOut(why) => {
                self.emit(&format!("[run] {why}"));
                // stopping it runs its exit callback, which restarts the session when that is wanted
                capture.stop();
                return;
            }
        };

        // ffmpeg reads what the engine announced, not what was planned: with `--format auto` the engine chose,
        // and a Mac whose system refuses HDR capture sends SDR from a display in HDR mode.
        let input = if in_engine { VideoInput::EncodedTs } else { VideoInput::Raw };
        let encode = plan.ffmpeg.with_video_input(input).for_frames(layout);
        let argv = match ffmpeg_command::build(&encode) {
            Ok(argv) => argv,
            Err(e) => {
                self.emit(&format!("[run] cannot encode {} {}x{}: {e}", layout.format, layout.width, layout.height));
                self.give_up(gen, &capture, &format!("cannot encode these frames: {e}"));
                return;
            }
        };
        self.emit(&format!(
            "[run] sending {}{}",
            summary(&encode),
            if in_engine { " (encoded by HdrCast itself: keyframes on request)" } else { "" }
        ));
        if plan.ffmpeg.encoder.hdr && !encode.encoder.hdr {
            self.emit(
                "[run] the display is in HDR mode, but it could not be captured as HDR here (the capture lines above say why): sending SDR",
            );
        }

        let mut ffmpeg = LineProcess::new(&plan.ffmpeg_exe, &argv);
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
        self.emit(&format!("[run] {}", ffmpeg.safe_command_line()));

        if let Err(e) = ffmpeg.start() {
            self.emit(&format!("[run] failed to start: {e}"));
            self.give_up(gen, &capture, &format!("could not start: {e}"));
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
        self.tell(&EncoderNews::Started { in_engine });
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

    /// Polls for the engine's layout line and its pipes; the engine prints the line before it creates the
    /// pipes. Never looks at a pipe as a file: on Windows, querying the attributes of `\\.\pipe\name` is itself
    /// a client connection; it takes the only instance, ffmpeg then gets ERROR_PIPE_BUSY ("Error opening
    /// input: Invalid argument") and the engine's writes fail.
    fn wait_for_engine(&self, plan: &SendPlan, capture: &LineProcess, gen: u64) -> EngineWait {
        let begun = Instant::now();
        while begun.elapsed() < PIPE_WAIT {
            if !self.current(gen) {
                return EngineWait::Cancelled;
            }
            if capture.has_exited() {
                return EngineWait::EngineExited;
            }
            let layout = self.gate.lock().unwrap().layout;
            if let Some(layout) = layout {
                if plan.pipe_names.iter().all(|name| hdrcast_capture::pipe_listening(name)) {
                    return EngineWait::Ready(layout);
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        let missing = if self.gate.lock().unwrap().layout.is_some() {
            "did not open its pipes"
        } else {
            "did not say which frames it sends"
        };
        EngineWait::TimedOut(format!("the capture engine {missing} within {} s", PIPE_WAIT.as_secs()))
    }

    /// One line of the engine's stdout: its layout (once), then statistics every second.
    fn on_capture_line(&self, gen: u64, line: &str) {
        if let Some(layout) = FrameLayout::try_parse(line) {
            let mut g = self.gate.lock().unwrap();
            if gen == g.generation {
                g.layout = Some(layout);
            }
            return;
        }
        if let Some(event) = EncoderEvent::try_parse(line) {
            if gen == self.gate.lock().unwrap().generation {
                self.tell(&EncoderNews::Event(event));
            }
            return;
        }
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

    /// Ends the session after the engine of `gen` was started, for a reason a restart would not change. The
    /// engine's exit is marked as dealt with first: its exit callback (on another thread, after the kill) would
    /// otherwise put its own reason over `why`.
    fn give_up(&self, gen: u64, capture: &LineProcess, why: &str) {
        {
            let mut g = self.gate.lock().unwrap();
            // A stop or a newer launch has taken this engine over (and stops it).
            if gen != g.generation || g.user_stopped {
                return;
            }
            g.exit_handled = gen;
            g.capture = None;
        }
        capture.stop();
        self.fail(gen, why);
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
        {
            let mut g = self.gate.lock().unwrap();
            if gen != g.generation {
                return;
            }
            *self.commands.lock().unwrap() = None;
            if code == exit_codes::ENCODER_UNAVAILABLE && g.in_engine && !g.user_stopped {
                // not a failure of the session: ffmpeg encodes from here on, at once
                g.engine_unavailable = true;
                drop(g);
                self.emit("[capture] exited: it cannot encode here (the lines above say why); ffmpeg encodes instead, keyframes on request are off");
                self.stop_peer_and_restart_now(gen);
                return;
            }
        }
        let reason = describe_capture_exit(code);
        self.emit(&format!("[capture] exited: {reason}"));
        self.stop_peer_and_maybe_restart(gen, &format!("capture engine stopped: {reason}"));
    }

    /// The engine could not encode: the same session again at once, with ffmpeg encoding.
    fn stop_peer_and_restart_now(self: &Arc<Self>, gen: u64) {
        let ffmpeg = {
            let mut g = self.gate.lock().unwrap();
            if gen != g.generation || g.user_stopped || g.exit_handled == gen {
                return;
            }
            g.exit_handled = gen;
            g.capture = None;
            g.ffmpeg.take()
        };
        if let Some(p) = &ffmpeg {
            p.stop();
        }
        let me = self.clone();
        thread::Builder::new().name("sender-restart".into()).spawn(move || Shared::launch(&me)).ok();
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
        7 => "the encoder inside the engine failed".to_string(),
        other => format!("exit code {other}"),
    }
}
