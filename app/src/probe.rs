//! The connection test (docs/design.md §5.1): joins a host the way watching does, but with a generous delay and no
//! player. ffmpeg takes the stream and drops it, while the viewer's link measures for a while how late SRT's
//! packets came against their own timestamps and how many went missing. [`srt_advice`] turns that into the delay,
//! the player's buffer and the resend headroom for this host, which the Watch page offers to apply.
//!
//! Only one test runs at a time, and never while watching: the host would see this computer join twice.
//! Starting to watch stops a test that runs.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hdrcast_core::common::line_process::LineProcess;
use hdrcast_media::receiver::mpv_command;
use hdrcast_media::receiver::settings::ReceiverSettings;
use hdrcast_p2p::srt_advice::{self, Advice, DecidedBy, Measured, Note, Verdict};
use hdrcast_p2p::{SrtStats, ViewerSession, ViewerSessionOptions, ViewerSessionState};
use serde_json::{json, Value};

use crate::state::{self, AppState};

/// The delay the test joins with: long enough that nothing it measures is cut short (a packet sent five times over
/// a 300 ms path still makes it).
const TEST_LATENCY_MS: u32 = 3000;
/// From the first packet until the measuring starts: SRT takes the round trip for 100 ms until it has measured
/// one, and asks for lost packets slowly until then.
const SETTLE: Duration = Duration::from_secs(3);
/// How long the measuring runs.
const MEASURE: Duration = Duration::from_secs(20);
/// How long the host may take to let the test in (its user may be asked first, or its stream may be starting).
const JOIN_WAIT: Duration = Duration::from_secs(90);
/// How long the first packet may take once the path works (ffmpeg is started again meanwhile if it gives up).
const FIRST_DATA_WAIT: Duration = Duration::from_secs(20);
const TICK: Duration = Duration::from_millis(100);

#[derive(Default)]
pub struct Probe {
    /// The test that runs, or the last one (its outcome stays on show until the next).
    run: Mutex<Option<Arc<Run>>>,
    /// The latest result for each host, by host id.
    results: Mutex<BTreeMap<String, Value>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Joining,
    Settling,
    Measuring,
    Done,
    Failed,
    Stopped,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Joining => "joining",
            Phase::Settling => "settling",
            Phase::Measuring => "measuring",
            Phase::Done => "done",
            Phase::Failed => "failed",
            Phase::Stopped => "stopped",
        }
    }

    fn ended(self) -> bool {
        matches!(self, Phase::Done | Phase::Failed | Phase::Stopped)
    }
}

struct Run {
    host_id: String,
    host_name: String,
    viewer: ViewerSession,
    settings: ReceiverSettings,
    stop: AtomicBool,
    ffmpeg: Mutex<Option<Arc<LineProcess>>>,
    ffmpeg_exited: Arc<AtomicBool>,
    view: Mutex<RunView>,
    /// The worker's thread ended: everything it started is gone.
    finished: AtomicBool,
}

struct RunView {
    phase: Phase,
    /// When the measuring started.
    measuring_since: Option<Instant>,
    error: String,
}

pub fn is_active(app: &AppState) -> bool {
    current(app).is_some_and(|run| !run.finished.load(Ordering::SeqCst))
}

fn current(app: &AppState) -> Option<Arc<Run>> {
    app.probe.run.lock().unwrap().clone()
}

/// Starts testing the connection to `host_id`.
pub fn start(app: &Arc<AppState>, host_id: &str) -> Result<(), String> {
    if crate::watch::is_active(app) {
        return Err("Stop watching first: the test joins the host itself.".to_string());
    }
    if is_active(app) {
        return Err("A connection test is running already.".to_string());
    }
    let (group, settings) = {
        let inner = app.inner.lock().unwrap();
        (inner.group.current().clone(), inner.receiver.clone())
    };
    if !group.is_configured() {
        return Err("Paste the invite a friend sent you first.".to_string());
    }
    if crate::watch::receiver_ffmpeg(&settings).is_none() {
        return Err("ffmpeg was not found. The connection test needs ffmpeg with libsrt.".to_string());
    }
    let host_name = crate::live::name_of(app, host_id).unwrap_or_default();
    let viewer = ViewerSession::new(
        ViewerSessionOptions {
            group,
            host_id: host_id.to_string(),
            latency_ms: TEST_LATENCY_MS as i32,
            // the headroom the viewer asks for now: with more, a keyframe leaves sooner, so the figure stays on the
            // safe side when the advice asks for more
            overhead_percent: settings.overhead_to_ask() as i32,
            // no remote control, no keyframe requests, no control mode: just the stream
            device: Some(app.device.clone()),
            map_router_port: state::router_mapping_wanted(),
            ..ViewerSessionOptions::default()
        },
        app.log_fn(),
    );
    let run = Arc::new(Run {
        host_id: host_id.to_string(),
        host_name,
        viewer,
        settings,
        stop: AtomicBool::new(false),
        ffmpeg: Mutex::new(None),
        ffmpeg_exited: Arc::new(AtomicBool::new(false)),
        view: Mutex::new(RunView { phase: Phase::Joining, measuring_since: None, error: String::new() }),
        finished: AtomicBool::new(false),
    });
    *app.probe.run.lock().unwrap() = Some(run.clone());
    let shown = run.shown_name();
    app.log(&format!(
        "[probe] testing the connection to {shown}: joining with a delay of {TEST_LATENCY_MS} ms, then {} s of measuring",
        MEASURE.as_secs()
    ));
    let worker_app = app.clone();
    let spawned = thread::Builder::new().name("connection test".to_string()).spawn(move || {
        let outcome = drive(&worker_app, &run);
        run.stop_everything();
        let phase = match outcome {
            Ok(result) => {
                worker_app.probe.results.lock().unwrap().insert(run.host_id.clone(), result);
                Phase::Done
            }
            Err(_) if run.stop.load(Ordering::SeqCst) => {
                worker_app.log("[probe] stopped");
                Phase::Stopped
            }
            Err(e) => {
                worker_app.log(&format!("[probe] the test of {} failed: {e}", run.shown_name()));
                run.view.lock().unwrap().error = e;
                Phase::Failed
            }
        };
        run.view.lock().unwrap().phase = phase;
        run.finished.store(true, Ordering::SeqCst);
    });
    if let Err(e) = spawned {
        *app.probe.run.lock().unwrap() = None;
        return Err(format!("Could not start the test: {e}"));
    }
    Ok(())
}

/// Stops a test that runs, and waits until it has let go of the host (a few seconds at most).
pub fn stop(app: &AppState) {
    let Some(run) = current(app) else { return };
    if run.finished.load(Ordering::SeqCst) {
        return;
    }
    run.stop.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !run.finished.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
}

impl Run {
    fn shown_name(&self) -> String {
        if self.host_name.is_empty() {
            let learned = self.viewer.host_name();
            if learned.is_empty() { "the host".to_string() } else { learned }
        } else {
            self.host_name.clone()
        }
    }

    fn set_phase(&self, phase: Phase) {
        let mut view = self.view.lock().unwrap();
        view.phase = phase;
        if phase == Phase::Measuring {
            view.measuring_since = Some(Instant::now());
        }
    }

    fn stopped(&self) -> Result<(), String> {
        if self.stop.load(Ordering::SeqCst) {
            Err("stopped".to_string())
        } else {
            Ok(())
        }
    }

    /// Waits up to `wait` for `done`, checking every tick whether the test was stopped.
    fn wait_for(&self, wait: Duration, mut done: impl FnMut() -> Result<bool, String>) -> Result<bool, String> {
        let deadline = Instant::now() + wait;
        loop {
            self.stopped()?;
            if done()? {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(TICK);
        }
    }

    fn packets(&self) -> u64 {
        self.viewer.srt_stats().map_or(0, |s| s.total.sent)
    }

    fn stop_everything(&self) {
        if let Some(ffmpeg) = self.ffmpeg.lock().unwrap().take() {
            ffmpeg.stop();
        }
        self.viewer.stop();
    }

    /// ffmpeg as the SRT receiver: it reads the stream from the link's local end and drops it.
    fn start_ffmpeg(&self, app: &AppState) -> Result<(), String> {
        if let Some(old) = self.ffmpeg.lock().unwrap().take() {
            old.stop();
        }
        let exe = crate::watch::receiver_ffmpeg(&self.settings).ok_or("ffmpeg was not found")?;
        let test = ReceiverSettings { latency_ms: TEST_LATENCY_MS as i32, ..self.settings.clone() };
        let url = mpv_command::build_url(&test, self.viewer.local_port(), &self.viewer.srt_passphrase()).map_err(|e| e.to_string())?;
        let mut ffmpeg = LineProcess::new(exe, mpv_command::ffmpeg_pipe_args(&url));
        ffmpeg.job(Some(app.job.clone())).read_stdout_lines(false);
        let log = app.logger();
        ffmpeg.on_stderr_line(move |line| log(&format!("[probe] ffmpeg: {line}")));
        self.ffmpeg_exited.store(false, Ordering::SeqCst);
        let exited = self.ffmpeg_exited.clone();
        ffmpeg.on_exited(move |_| exited.store(true, Ordering::SeqCst));
        app.log(&format!("[probe] {}", ffmpeg.safe_command_line()));
        ffmpeg.start().map_err(|e| format!("ffmpeg did not start: {e}"))?;
        if let Some(mut stdout) = ffmpeg.take_stdout() {
            let _ = thread::Builder::new().name("connection test sink".to_string()).spawn(move || {
                let _ = io::copy(&mut stdout, &mut io::sink());
            });
        }
        *self.ffmpeg.lock().unwrap() = Some(Arc::new(ffmpeg));
        Ok(())
    }
}

/// The test from joining to the result.
fn drive(app: &AppState, run: &Run) -> Result<Value, String> {
    run.viewer.start().map_err(|e| format!("could not start: {e}"))?;
    let joined = run.wait_for(JOIN_WAIT, || match run.viewer.state() {
        ViewerSessionState::Connected => Ok(true),
        ViewerSessionState::Refused => Err(run.viewer.status()),
        _ => Ok(false),
    })?;
    if !joined {
        return Err(format!("could not get through to the host ({})", run.viewer.status()));
    }

    // the stream: ffmpeg is started again if it gives up before the host's stream is there
    let mut came = false;
    for _ in 0..3 {
        run.start_ffmpeg(app)?;
        came = run.wait_for(FIRST_DATA_WAIT, || Ok(run.packets() > 0 || run.ffmpeg_exited.load(Ordering::SeqCst)))? && run.packets() > 0;
        if came {
            break;
        }
    }
    if !came {
        return Err("no stream came from the host".to_string());
    }
    run.set_phase(Phase::Settling);
    app.log("[probe] the stream comes: letting the connection settle");
    run.wait_for(SETTLE, || Ok(false))?;

    run.viewer.restart_srt_lateness();
    let first = run.viewer.srt_stats().unwrap_or_default();
    let since = Instant::now();
    run.set_phase(Phase::Measuring);
    let mut breaks = 0u32;
    let mut was_connected = true;
    run.wait_for(MEASURE, || {
        let connected = run.viewer.state() == ViewerSessionState::Connected;
        if was_connected && !connected {
            breaks += 1;
        }
        was_connected = connected;
        if run.ffmpeg_exited.load(Ordering::SeqCst) && connected {
            // the SRT connection ended (the host stopped its stream, or the path broke): once more
            breaks += 1;
            run.start_ffmpeg(app)?;
        }
        Ok(false)
    })?;
    let seconds = since.elapsed().as_secs_f64();
    let last = run.viewer.srt_stats().unwrap_or_default();
    let lateness = run.viewer.srt_lateness().unwrap_or_default();
    // a new path brings a new monitor: its counts start from nothing
    let counts = if last.total.sent >= first.total.sent { last.total.since(&first.total) } else { last.total };
    let measured = Measured {
        lateness,
        counts,
        seconds,
        rtt_ms: last.rtt_ms,
        rtt_var_ms: last.rtt_var_ms,
        test_latency_ms: TEST_LATENCY_MS,
        reconnects: last.connections.saturating_sub(first.connections).max(breaks),
    };
    let advice = srt_advice::advise(&measured);
    app.log(&format!("[probe] {}", describe(&run.shown_name(), &measured, &advice)));
    Ok(result_view(run, &measured, &advice, &last))
}

fn describe(host: &str, m: &Measured, a: &Advice) -> String {
    let l = &m.lateness;
    format!(
        "{host}: {} packets in {:.0} s, {:.2} % lost, {} not recovered, RTT {:.1} ms (± {:.1}); late on the first send \
         {:.0} ms (99.9 %), {} came on a resend (latest {:.0} ms), the latest of all {:.0} ms -> delay {} ms \
         (planned for {} resend(s), by {}), buffer {} s, headroom {} % ({})",
        m.counts.sent,
        m.seconds,
        a.loss_percent,
        m.counts.unrecovered,
        m.rtt_ms,
        m.rtt_var_ms,
        l.first_try_p999_ms,
        l.recovered,
        l.recovered_max_ms,
        l.max_ms,
        a.latency_ms,
        a.rounds,
        decided_by_name(a.decided_by),
        a.buffer_seconds,
        a.overhead_percent,
        verdict_name(a.verdict),
    )
}

fn verdict_name(v: Verdict) -> &'static str {
    match v {
        Verdict::Good => "good",
        Verdict::Fair => "fair",
        Verdict::Poor => "poor",
        Verdict::Unusable => "unusable",
    }
}

fn decided_by_name(d: DecidedBy) -> &'static str {
    match d {
        DecidedBy::Resends => "resends",
        DecidedBy::Latest => "latest",
        DecidedBy::Table => "table",
    }
}

fn note_view(note: &Note) -> Value {
    match note {
        Note::FewPackets(n) => json!({ "kind": "fewPackets", "n": n }),
        Note::LostForGood(n) => json!({ "kind": "lostForGood", "n": n }),
        Note::Reconnected(n) => json!({ "kind": "reconnected", "n": n }),
        Note::CutShort => json!({ "kind": "cutShort" }),
    }
}

fn result_view(run: &Run, m: &Measured, a: &Advice, stats: &SrtStats) -> Value {
    let l = &m.lateness;
    let at_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    json!({
        "hostId": run.host_id,
        "hostName": run.shown_name(),
        "atMs": at_ms,
        // what to set
        "latencyMs": a.latency_ms,
        "bufferSeconds": a.buffer_seconds,
        "overheadPercent": a.overhead_percent,
        "verdict": verdict_name(a.verdict),
        "decidedBy": decided_by_name(a.decided_by),
        "rounds": a.rounds,
        // the parts of the delay
        "firstTryMs": a.first_try_ms,
        "resendMs": a.resend_ms,
        "extraRoundMs": a.extra_round_ms,
        "latestMs": a.latest_ms,
        // what was measured
        "lossPercent": a.loss_percent,
        "packets": m.counts.sent,
        "unrecovered": m.counts.unrecovered,
        "recovered": l.recovered,
        "recoveredMaxMs": l.recovered_max_ms,
        "seconds": m.seconds,
        "mbps": if m.seconds > 0.0 { m.counts.sent_bytes as f64 * 8.0 / m.seconds / 1e6 } else { 0.0 },
        "rttMs": m.rtt_ms,
        "rttVarMs": m.rtt_var_ms,
        "path": run.viewer.path_description(),
        "capacityMbps": stats.capacity_mbps,
        "reconnects": m.reconnects,
        "notes": a.notes.iter().map(note_view).collect::<Vec<_>>(),
    })
}

pub fn status(app: &AppState) -> Value {
    let results: Value = app.probe.results.lock().unwrap().clone().into_iter().collect::<serde_json::Map<_, _>>().into();
    let Some(run) = current(app) else {
        return json!({ "run": null, "results": results });
    };
    let view = run.view.lock().unwrap();
    let phase = view.phase;
    let joining_state = (phase == Phase::Joining).then(|| run.viewer.state());
    let now = (phase == Phase::Measuring).then(|| {
        let stats = run.viewer.srt_stats().unwrap_or_default();
        let late = run.viewer.srt_lateness().unwrap_or_default();
        json!({
            "rttMs": stats.rtt_ms,
            "lossPercent": stats.recent.loss_percent(),
            "mbps": stats.recent_mbps(),
            "latestMs": late.max_ms,
        })
    });
    let elapsed = view.measuring_since.map(|t| t.elapsed().as_secs_f64().min(MEASURE.as_secs_f64()));
    json!({
        "run": {
            "hostId": run.host_id,
            "hostName": run.shown_name(),
            "phase": phase.name(),
            "active": !phase.ended(),
            // the host may be asking its user
            "approval": joining_state == Some(ViewerSessionState::Approval),
            "status": if phase == Phase::Joining { run.viewer.status() } else { String::new() },
            "secondsLeft": elapsed.map(|e| MEASURE.as_secs_f64() - e),
            "progress": match phase {
                Phase::Joining => 0.0,
                Phase::Settling => 0.05,
                Phase::Measuring => 0.1 + 0.9 * elapsed.unwrap_or(0.0) / MEASURE.as_secs_f64(),
                _ => 1.0,
            },
            "now": now,
            "error": view.error,
        },
        "results": results,
    })
}
