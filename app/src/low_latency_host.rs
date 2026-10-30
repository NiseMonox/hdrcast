//! Control mode, host side (docs/design.md §11). A capture engine of its own encodes for remote control the way
//! Vibepollo does (NVENC with the ultra-low-latency tuning, a picture the moment it is captured: `--low-latency`),
//! and this reads its pictures and its sound straight from the pipes and sends them to every viewer in control mode
//! as sealed side datagrams: each picture cut into shards with Reed-Solomon parity and paced out quickly, the sound
//! in 5 ms packets. No ffmpeg, no SRT, no fan-out queue, nothing sent twice.
//!
//! A viewer that lost a picture asks for a keyframe. One encoder serves all of them, so the requests are pooled: a
//! keyframe already asked for, or one that went out after the loss, serves everybody. Viewers report every half
//! second (what they showed, lost, restored, their round trip); the answer echoes the report's time, so a viewer
//! knows its round trip, and the page shows the counts.
//!
//! The engine runs while somebody watches in control mode (the share session starts and stops it) and is
//! restarted when it exits unexpectedly. One that cannot encode here (no NVENC: exit code 7) makes control mode
//! unavailable until the next start of the app: the live record stops offering it.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ChildStdin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hdrcast_capture::{exit_codes, CaptureStats, EncoderEvent, FrameLayout, FrameRecord};
use hdrcast_core::common::line_process::LineProcess;
use hdrcast_core::common::process_job::ProcessJob;
use hdrcast_media::low_latency::audio::AudioPacketizer;
use hdrcast_media::low_latency::messages::{Answer, Counts, Outcome, Request};
use hdrcast_media::low_latency::seal::{Direction, Opener, Sealer};
use hdrcast_media::low_latency::video::{Packetizer, Picture, DEFAULT_FEC_PERCENT};
use hdrcast_media::low_latency::{self, ANSWER, AUDIO, REQUEST, VIDEO};
use serde_json::{json, Value};

use crate::sender_runner::RunnerState;

type LogFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Sends a run of datagrams to a viewer in control mode (`P2pHost::send_low_latency`); how many went.
pub type SendFn = Arc<dyn Fn(&str, &[&[u8]]) -> usize + Send + Sync>;

/// How long to wait for the engine's pipes (it opens the GPU and compiles its shader first).
const PIPE_WAIT: Duration = Duration::from_secs(15);
const RESTART_DELAY: Duration = Duration::from_millis(1000);
/// How fast a picture's datagrams leave: Sunshine paces at 80 % of a gigabit. A 4K keyframe of 500 KB takes 5 ms;
/// a usual picture well under one.
const PACE_BITS_PER_SECOND: f64 = 800e6;
/// Datagrams handed to the socket at once between two pauses of the pacing.
const PACE_BATCH: usize = 32;
/// A keyframe asked of the engine and not out yet after this long is asked for again.
const KEYFRAME_RETRY: Duration = Duration::from_millis(250);

/// What the engine is started with (built by the share session from the settings).
#[derive(Debug, Clone, PartialEq)]
pub struct LowLatencyPlan {
    /// What `capture_argv` was made from (`--low-latency` among the encoder's options).
    pub capture: hdrcast_capture::CaptureArgs,
    pub capture_exe: PathBuf,
    pub capture_argv: Vec<String>,
    pub video_pipe: String,
    /// `None` without sound.
    pub audio_pipe: Option<String>,
    /// "HEVC (NVENC, ultra-low latency) 30 Mbps up to 120 fps", for the log.
    pub summary: String,
}

/// One viewer in control mode.
struct Viewer {
    sid: String,
    name: String,
    sealer: Mutex<Sealer>,
    opener: Mutex<Opener>,
    report: Mutex<Report>,
}

#[derive(Default)]
struct Report {
    counts: Counts,
    rtt_us: u32,
    at: Option<Instant>,
    keyframe_requests: u32,
    pictures: u64,
    bytes: u64,
    /// For the rate on the page: bytes and time at the previous look.
    rate_mark: Option<(u64, Instant)>,
    mbps: f64,
}

#[derive(Default)]
struct Keyframes {
    /// The number of the last keyframe sent.
    last_sent: Option<u32>,
    /// A keyframe asked of the engine that has not come out yet, and when.
    asked_at: Option<Instant>,
}

struct Gate {
    generation: u64,
    user_stopped: bool,
    state: RunnerState,
    detail: String,
    plan: Option<Arc<LowLatencyPlan>>,
    engine: Option<Arc<LineProcess>>,
    layout: Option<FrameLayout>,
    capture: Option<CaptureStats>,
    started: Option<Instant>,
    attempt: u32,
    /// The engine said it cannot encode here: control mode is off until the app starts again.
    unavailable: bool,
    /// Pictures read from the engine since the last start, and the latest one's times.
    pictures: u64,
    age_us: u32,
    encode_us: u32,
}

struct Shared {
    job: Arc<ProcessJob>,
    log: LogFn,
    send: SendFn,
    gate: Mutex<Gate>,
    viewers: Mutex<HashMap<String, Arc<Viewer>>>,
    /// The engine's stdin: "keyframe", "bitrate <kbps> <vbv>".
    commands: Mutex<Option<ChildStdin>>,
    keyframes: Mutex<Keyframes>,
    /// The number the next picture gets: it goes on across restarts of the engine, so a viewer never sees the
    /// numbers go back.
    next_frame: AtomicU32,
    stopping: AtomicBool,
}

#[derive(Clone)]
pub struct LowLatencyHost {
    shared: Arc<Shared>,
}

impl LowLatencyHost {
    pub fn new(job: Arc<ProcessJob>, log: LogFn, send: SendFn) -> Self {
        let gate = Gate {
            generation: 0,
            user_stopped: true,
            state: RunnerState::Idle,
            detail: String::new(),
            plan: None,
            engine: None,
            layout: None,
            capture: None,
            started: None,
            attempt: 0,
            unavailable: false,
            pictures: 0,
            age_us: 0,
            encode_us: 0,
        };
        Self {
            shared: Arc::new(Shared {
                job,
                log,
                send,
                gate: Mutex::new(gate),
                viewers: Mutex::new(HashMap::new()),
                commands: Mutex::new(None),
                keyframes: Mutex::new(Keyframes::default()),
                next_frame: AtomicU32::new(0),
                stopping: AtomicBool::new(false),
            }),
        }
    }

    pub fn state(&self) -> RunnerState {
        self.shared.gate.lock().unwrap().state
    }

    /// The engine said it cannot encode here (no NVENC): control mode is not offered any more.
    pub fn unavailable(&self) -> bool {
        self.shared.gate.lock().unwrap().unavailable
    }

    /// Pictures read from the engine since it last started (0 while it starts).
    pub fn pictures(&self) -> u64 {
        self.shared.gate.lock().unwrap().pictures
    }

    /// The plan of the engine that runs (or ran last).
    pub fn plan(&self) -> Option<LowLatencyPlan> {
        self.shared.gate.lock().unwrap().plan.as_deref().cloned()
    }

    /// Starts the engine with `plan` (a running one is replaced) and returns at once.
    pub fn start(&self, plan: LowLatencyPlan) {
        self.stop();
        {
            let mut g = self.shared.gate.lock().unwrap();
            g.plan = Some(Arc::new(plan));
            g.user_stopped = false;
            g.attempt = 0;
            g.capture = None;
        }
        self.shared.stopping.store(false, Ordering::SeqCst);
        self.shared.set_state(RunnerState::Starting, "Starting…");
        let shared = self.shared.clone();
        thread::Builder::new().name("ll-launch".into()).spawn(move || Shared::launch(&shared)).ok();
    }

    /// Stops the engine and waits for it (a few seconds at most). Call it off the UI thread.
    pub fn stop(&self) {
        let engine = {
            let mut g = self.shared.gate.lock().unwrap();
            g.user_stopped = true;
            g.generation += 1;
            g.engine.take()
        };
        self.shared.stopping.store(true, Ordering::SeqCst);
        *self.shared.commands.lock().unwrap() = None;
        if let Some(engine) = engine {
            self.shared.set_state(RunnerState::Stopping, "Stopping…");
            engine.stop();
        }
        self.shared.set_state(RunnerState::Idle, "Stopped");
    }

    /// Asks the running engine for another bitrate (kbit/s, rate buffer in kbit) without a restart. False when no
    /// engine runs.
    pub fn set_bitrate(&self, kbps: u32, vbv_kbits: u32) -> bool {
        self.shared.command(&format!("bitrate {kbps} {vbv_kbits}"))
    }

    /// The running engine took a change without a restart (a bitrate): `plan` describes it now.
    pub fn adopt_plan(&self, plan: LowLatencyPlan) {
        let mut g = self.shared.gate.lock().unwrap();
        if g.plan.is_some() {
            g.plan = Some(Arc::new(plan));
        }
    }

    /// A viewer let in in control mode, with the remote-control key of its session.
    pub fn register(&self, sid: &str, name: &str, control_key: &[u8; 32]) {
        let key = low_latency::session_key(control_key);
        let viewer = Arc::new(Viewer {
            sid: sid.to_string(),
            name: name.to_string(),
            sealer: Mutex::new(Sealer::new(&key, Direction::ToViewer)),
            opener: Mutex::new(Opener::new(&key, Direction::ToHost)),
            report: Mutex::new(Report::default()),
        });
        self.shared.viewers.lock().unwrap().insert(sid.to_string(), viewer);
    }

    /// Forgets viewers that are gone (housekeeping passes the sessions that still exist).
    pub fn keep_only(&self, sids: &[String]) {
        self.shared.viewers.lock().unwrap().retain(|sid, _| sids.contains(sid));
    }

    /// A datagram of control mode from a viewer's session (on a network thread).
    pub fn on_datagram(&self, sid: &str, datagram: &[u8]) {
        let Some(viewer) = self.shared.viewers.lock().unwrap().get(sid).cloned() else { return };
        let request = {
            let mut opener = viewer.opener.lock().unwrap();
            match opener.open(datagram) {
                Some((REQUEST, message)) => Request::parse(message),
                _ => None,
            }
        };
        let received = Instant::now();
        let answer = match request {
            Some(Request::Keyframe { seq, lost }) => {
                viewer.report.lock().unwrap().keyframe_requests += 1;
                let outcome = self.shared.keyframe_for(lost);
                if outcome == Outcome::Coming {
                    (self.shared.log)(&format!("[ll] {} lost picture {lost}: a keyframe is coming", viewer.name));
                }
                Answer::Keyframe { seq, outcome }
            }
            Some(Request::Report { sent_us, rtt_us, counts }) => {
                let mut report = viewer.report.lock().unwrap();
                report.counts = counts;
                report.rtt_us = rtt_us;
                report.at = Some(received);
                Answer::Echo { sent_us, held_us: received.elapsed().as_micros() as u32 }
            }
            None => return,
        };
        let mut datagram = Vec::new();
        viewer.sealer.lock().unwrap().seal(ANSWER, &answer.to_bytes(), &mut datagram);
        (self.shared.send)(sid, &[&datagram]);
    }

    /// For the page: the engine.
    pub fn status(&self) -> Value {
        let g = self.shared.gate.lock().unwrap();
        let running = matches!(g.state, RunnerState::Starting | RunnerState::Streaming);
        json!({
            "state": g.state.name(),
            "detail": g.detail,
            "uptimeSec": if running { g.started.map_or(0.0, |t| t.elapsed().as_secs_f64()) } else { 0.0 },
            "fps": g.capture.map(|c| c.fps),
            "width": g.layout.map(|l| l.width),
            "height": g.layout.map(|l| l.height),
            "hdr": g.layout.map(|l| l.format == hdrcast_capture::CaptureFormat::P010),
            "encodeMs": f64::from(g.encode_us) / 1000.0,
            "hostMs": f64::from(g.age_us) / 1000.0,
            "unavailable": g.unavailable,
        })
    }

    /// For the page: one viewer in control mode; `None` for a viewer that watches.
    pub fn viewer_status(&self, sid: &str) -> Option<Value> {
        let viewer = self.shared.viewers.lock().unwrap().get(sid).cloned()?;
        let mut r = viewer.report.lock().unwrap();
        let now = Instant::now();
        if let Some((bytes, at)) = r.rate_mark {
            let seconds = now.saturating_duration_since(at).as_secs_f64();
            if seconds >= 0.5 {
                r.mbps = (r.bytes - bytes) as f64 * 8.0 / seconds / 1e6;
                r.rate_mark = Some((r.bytes, now));
            }
        } else {
            r.rate_mark = Some((r.bytes, now));
        }
        Some(json!({
            "rttMs": (r.rtt_us > 0).then(|| f64::from(r.rtt_us) / 1000.0),
            "shown": r.counts.shown,
            "lost": r.counts.lost,
            "restored": r.counts.restored_shards,
            "keyframeRequests": r.keyframe_requests,
            "mbps": r.mbps,
            "reportAgeSec": r.at.map(|at| at.elapsed().as_secs_f64()),
        }))
    }
}

impl Shared {
    fn emit(&self, line: &str) {
        (self.log)(line);
    }

    fn set_state(&self, state: RunnerState, detail: &str) {
        let changed = {
            let mut g = self.gate.lock().unwrap();
            let changed = g.state != state || g.detail != detail;
            g.state = state;
            g.detail = detail.to_string();
            changed
        };
        if changed {
            self.emit(&format!("[ll] {}: {detail}", state.name()));
        }
    }

    fn current(&self, gen: u64) -> bool {
        let g = self.gate.lock().unwrap();
        g.generation == gen && !g.user_stopped
    }

    fn command(&self, line: &str) -> bool {
        let mut commands = self.commands.lock().unwrap();
        let Some(stdin) = commands.as_mut() else { return false };
        if stdin.write_all(format!("{line}\n").as_bytes()).and_then(|()| stdin.flush()).is_ok() {
            return true;
        }
        *commands = None;
        false
    }

    /// What is done about a viewer that lost picture `lost`.
    fn keyframe_for(&self, lost: u32) -> Outcome {
        let mut k = self.keyframes.lock().unwrap();
        if k.last_sent.is_some_and(|sent| (sent.wrapping_sub(lost) as i32) > 0) {
            return Outcome::AlreadySent;
        }
        if k.asked_at.is_some_and(|at| at.elapsed() < KEYFRAME_RETRY) {
            return Outcome::Coming;
        }
        if self.command("keyframe") {
            k.asked_at = Some(Instant::now());
            Outcome::Coming
        } else {
            Outcome::Cannot
        }
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
            g.layout = None;
            g.pictures = 0;
            (g.generation, plan, g.attempt)
        };
        self.set_state(RunnerState::Starting, if attempt == 1 { "Starting…" } else { "Restarting…" });
        let mut engine = LineProcess::new(&plan.capture_exe, &plan.capture_argv);
        engine.job(Some(self.job.clone())).redirect_stdin(true);
        let me = self.clone();
        engine.on_stdout_line(move |line| me.on_engine_line(gen, line));
        let me = self.clone();
        engine.on_stderr_line(move |line| me.emit(&format!("[ll-capture] {line}")));
        let me = self.clone();
        engine.on_exited(move |code| me.on_engine_exited(gen, code));
        self.emit(&format!("[ll] {} — {}", plan.summary, engine.safe_command_line()));
        if let Err(e) = engine.start() {
            self.set_state(RunnerState::Failed, &format!("could not start: {e}"));
            return;
        }
        let engine = Arc::new(engine);
        {
            let mut g = self.gate.lock().unwrap();
            if g.generation != gen || g.user_stopped {
                drop(g);
                engine.stop();
                return;
            }
            g.engine = Some(engine.clone());
        }
        *self.commands.lock().unwrap() = engine.take_stdin();

        // the engine says which frames it sends, then creates its pipes and waits for us
        let begun = Instant::now();
        loop {
            if !self.current(gen) || engine.has_exited() {
                return;
            }
            let ready = self.gate.lock().unwrap().layout.is_some()
                && hdrcast_capture::pipe_listening(&plan.video_pipe)
                && plan.audio_pipe.as_deref().is_none_or(hdrcast_capture::pipe_listening);
            if ready {
                break;
            }
            if begun.elapsed() > PIPE_WAIT {
                self.emit(&format!("[ll] the capture engine did not open its pipes within {} s", PIPE_WAIT.as_secs()));
                engine.stop();
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let video = match File::open(hdrcast_capture::pipe_path(&plan.video_pipe)) {
            Ok(file) => file,
            Err(e) => {
                self.emit(&format!("[ll] cannot open the engine's video pipe: {e}"));
                engine.stop();
                return;
            }
        };
        let me = self.clone();
        thread::Builder::new().name("ll-video".into()).spawn(move || me.pump_video(gen, video)).ok();
        if let Some(audio_pipe) = &plan.audio_pipe {
            match File::open(hdrcast_capture::pipe_path(audio_pipe)) {
                Ok(audio) => {
                    let me = self.clone();
                    thread::Builder::new().name("ll-audio".into()).spawn(move || me.pump_audio(gen, audio)).ok();
                }
                Err(e) => self.emit(&format!("[ll] cannot open the engine's sound pipe (going on without sound): {e}")),
            }
        }
        {
            let mut g = self.gate.lock().unwrap();
            if g.generation == gen {
                g.started = Some(Instant::now());
            }
        }
        if self.current(gen) {
            self.set_state(RunnerState::Streaming, "Streaming");
        }
    }

    fn on_engine_line(&self, gen: u64, line: &str) {
        if let Some(layout) = FrameLayout::try_parse(line) {
            let mut g = self.gate.lock().unwrap();
            if g.generation == gen {
                g.layout = Some(layout);
            }
            return;
        }
        if let Some(event) = EncoderEvent::try_parse(line) {
            if let EncoderEvent::Bitrate { kbps, ok } = event {
                self.emit(&format!("[ll] bitrate {kbps} kbit/s: {}", if ok { "taken" } else { "refused" }));
            }
            return;
        }
        match CaptureStats::try_parse(line) {
            Some(stats) => {
                let mut g = self.gate.lock().unwrap();
                if g.generation == gen {
                    g.capture = Some(stats);
                }
            }
            None => self.emit(&format!("[ll-capture] {line}")),
        }
    }

    fn on_engine_exited(self: &Arc<Self>, gen: u64, code: i32) {
        let restart = {
            let mut g = self.gate.lock().unwrap();
            if g.generation != gen {
                return;
            }
            g.engine = None;
            if code == exit_codes::ENCODER_UNAVAILABLE {
                g.unavailable = true;
            }
            !g.user_stopped && code != exit_codes::ENCODER_UNAVAILABLE
        };
        *self.commands.lock().unwrap() = None;
        if code == exit_codes::ENCODER_UNAVAILABLE {
            self.emit("[ll] the capture engine cannot encode here (the lines above say why): control mode is off until HdrCast starts again");
            self.set_state(RunnerState::Failed, "NVENC is not available on this computer");
            return;
        }
        if !restart {
            return;
        }
        self.set_state(RunnerState::Starting, &format!("the capture engine stopped (exit code {code}): restarting"));
        let me = self.clone();
        thread::Builder::new()
            .name("ll-restart".into())
            .spawn(move || {
                thread::sleep(RESTART_DELAY);
                if me.current(gen) {
                    Shared::launch(&me);
                }
            })
            .ok();
    }

    fn viewers(&self) -> Vec<Arc<Viewer>> {
        self.viewers.lock().unwrap().values().cloned().collect()
    }

    /// Reads the engine's pictures and sends each one to every viewer in control mode.
    fn pump_video(&self, gen: u64, mut pipe: File) {
        let mut packetizer = Packetizer::new(DEFAULT_FEC_PERCENT);
        let mut shards: Vec<Vec<u8>> = Vec::new();
        let mut datagrams: Vec<Vec<u8>> = Vec::new();
        while self.current(gen) {
            let record = match FrameRecord::read(&mut pipe) {
                Ok(Some(record)) => record,
                Ok(None) => break,
                Err(e) => {
                    if self.current(gen) {
                        self.emit(&format!("[ll] the engine's video pipe: {e}"));
                    }
                    break;
                }
            };
            let read_at = Instant::now();
            let frame = self.next_frame.fetch_add(1, Ordering::Relaxed);
            {
                let mut g = self.gate.lock().unwrap();
                g.pictures += 1;
                g.age_us = record.age_us;
                g.encode_us = record.encode_us;
            }
            if record.keyframe {
                let mut k = self.keyframes.lock().unwrap();
                k.last_sent = Some(frame);
                k.asked_at = None;
            }
            shards.clear();
            let picture = Picture {
                frame,
                keyframe: record.keyframe,
                host_us: record.age_us.saturating_add(read_at.elapsed().as_micros() as u32),
                encode_us: record.encode_us,
                data: &record.data,
            };
            if let Err(e) = packetizer.split(&picture, |shard| shards.push(shard.to_vec())) {
                self.emit(&format!("[ll] {e}"));
                continue;
            }
            while datagrams.len() < shards.len() {
                datagrams.push(Vec::new());
            }
            for viewer in self.viewers() {
                {
                    let mut sealer = viewer.sealer.lock().unwrap();
                    for (shard, datagram) in shards.iter().zip(datagrams.iter_mut()) {
                        sealer.seal(VIDEO, shard, datagram);
                    }
                }
                let sent = self.paced_send(&viewer.sid, &datagrams[..shards.len()]);
                let mut report = viewer.report.lock().unwrap();
                report.pictures += 1;
                report.bytes += datagrams[..sent].iter().map(|d| d.len() as u64).sum::<u64>();
            }
        }
    }

    /// Sends a picture's datagrams in batches, pausing between them so that they leave at about
    /// [`PACE_BITS_PER_SECOND`] instead of all at once. Returns how many went.
    fn paced_send(&self, sid: &str, datagrams: &[Vec<u8>]) -> usize {
        let begun = Instant::now();
        let (mut sent, mut bytes) = (0, 0usize);
        let batches = datagrams.len().div_ceil(PACE_BATCH);
        for (n, batch) in datagrams.chunks(PACE_BATCH).enumerate() {
            let refs: Vec<&[u8]> = batch.iter().map(Vec::as_slice).collect();
            let went = (self.send)(sid, &refs);
            sent += went;
            if went < batch.len() {
                break;
            }
            bytes += batch.iter().map(Vec::len).sum::<usize>();
            if n + 1 < batches {
                let due = begun + Duration::from_secs_f64(bytes as f64 * 8.0 / PACE_BITS_PER_SECOND);
                let now = Instant::now();
                if due > now {
                    thread::sleep(due - now);
                }
            }
        }
        sent
    }

    /// Reads the engine's sound (f32 stereo at 48 kHz) and sends it in 5 ms packets.
    fn pump_audio(&self, gen: u64, mut pipe: File) {
        let mut packetizer = AudioPacketizer::new();
        let mut bytes = vec![0u8; 3840];
        let mut carried = 0usize;
        let mut samples: Vec<f32> = Vec::new();
        let mut datagram = Vec::new();
        while self.current(gen) {
            let n = match pipe.read(&mut bytes[carried..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let have = carried + n;
            let whole = have - have % 4;
            samples.clear();
            samples.extend(bytes[..whole].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
            bytes.copy_within(whole..have, 0);
            carried = have - whole;
            let viewers = self.viewers();
            if viewers.is_empty() {
                continue;
            }
            packetizer.push(&samples, |packet| {
                for viewer in &viewers {
                    viewer.sealer.lock().unwrap().seal(AUDIO, packet, &mut datagram);
                    if (self.send)(&viewer.sid, &[&datagram]) == 1 {
                        viewer.report.lock().unwrap().bytes += datagram.len() as u64;
                    }
                }
            });
        }
    }
}
