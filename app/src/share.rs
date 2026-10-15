//! Sharing this computer's screen with the group: the capture engine and encoder ([`SenderRunner`]), the P2P
//! host that serves every viewer from that one encode, and remote control for the viewers who ask.

use std::sync::{Arc, Mutex, Weak};

use hdrcast_capture::{CaptureArgs, CaptureFormat};
use hdrcast_core::common::firewall::row as firewall_row;
use hdrcast_media::sender::ffmpeg_command::{self, AudioCodec, FfmpegPlan};
use hdrcast_media::sender::settings::HdrMode;
use hdrcast_p2p::{P2pHost, P2pHostOptions, SwitchableStream, ViewerState};
use serde_json::{json, Value};

use crate::commands;
use crate::control_host::ControlHub;
use crate::sender_runner::{self, RunnerState, SendPlan, SenderRunner};
use crate::state::{self, AppState, Inner};

struct Session {
    runner: SenderRunner,
    ts: SwitchableStream,
    /// `None` once the encoder gave up for good and this computer went offline (the runner's last state stays
    /// on show until the next start).
    host: Mutex<Option<Arc<P2pHost>>>,
    hub: Arc<ControlHub>,
}

#[derive(Default)]
pub struct Share {
    session: Mutex<Option<Arc<Session>>>,
}

fn current(app: &AppState) -> Option<Arc<Session>> {
    app.share.session.lock().unwrap().clone()
}

/// Sharing, starting or stopping right now.
pub fn is_active(app: &AppState) -> bool {
    current(app).is_some_and(|s| !matches!(s.runner.state(), RunnerState::Idle | RunnerState::Failed))
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

/// Goes live. The error is the list of things to fix first, as sentences.
pub fn start(app: &Arc<AppState>) -> Result<(), Vec<String>> {
    if is_active(app) {
        return Err(vec!["Already sharing.".to_string()]);
    }
    let (plan, group, bitrate_bps, max_viewers, policy, display) = {
        let inner = app.inner.lock().unwrap();
        let plan = build_plan(app, &inner)?;
        let display = state::selected_output(&inner.sender, &inner.outputs).map(|o| o.device_name.clone()).unwrap_or_default();
        (plan, inner.group.current().clone(), inner.sender.bitrate_bps(), inner.sender.max_viewers.max(1) as u32, inner.ui.remote_control, display)
    };
    let keys = group.keys().map_err(|_| vec!["There is no group yet.".to_string()])?;
    app.log(&format!("[app] {}", sender_runner::summary(&plan.ffmpeg)));

    // Viewers come and go through the group; the encoder keeps running for all of them, and a restarted
    // encoder is spliced into the same fan-out, so a session always restarts automatically.
    let ts = SwitchableStream::new();
    let weak_app: Weak<AppState> = Arc::downgrade(app);
    let firewall_app = weak_app.clone();
    let policy_app = weak_app.clone();
    let host = Arc::new(P2pHost::new(
        P2pHostOptions {
            group,
            ffmpeg_path: plan.ffmpeg_exe.clone().into_os_string(),
            bitrate_bps,
            max_viewers,
            job: Some(app.job.clone()),
            map_router_port: state::router_mapping_wanted(),
            inbound_allowed: Some(Arc::new(move || {
                firewall_app.upgrade().and_then(|app| firewall_row::inbound_allowed(app.inner.lock().unwrap().firewall))
            })),
            offer_remote_control: Some(Arc::new(move || {
                policy_app.upgrade().is_some_and(|app| app.inner.lock().unwrap().ui.remote_control != 0)
            })),
            ..P2pHostOptions::default()
        },
        Box::new(ts.clone()),
        app.log_fn(),
    ));

    let send_host = Arc::downgrade(&host);
    let name_host = Arc::downgrade(&host);
    let hub = ControlHub::start(
        keys,
        policy,
        &display,
        Arc::new(move |sid, payload| send_host.upgrade().is_some_and(|h| h.send_side(sid, payload))),
        Arc::new(move |sid| name_host.upgrade().and_then(|h| h.viewers().into_iter().find(|v| v.sid == sid).map(|v| v.name))),
        app.logger(),
    );
    let side_hub = hub.clone();
    host.set_side_handler(Arc::new(move |sid, payload| side_hub.on_datagram(sid, payload)));

    if let Err(e) = host.start() {
        app.log(&format!("[app] cannot start the group session: {e}"));
        hub.stop();
        host.stop();
        ts.close();
        return Err(vec![format!("Cannot start the group session: {e}")]);
    }

    let attach = ts.clone();
    let runner = SenderRunner::new(app.job.clone(), app.logger(), Arc::new(move |stdout| attach.attach(Box::new(stdout))));
    runner.start(plan, true);
    *app.share.session.lock().unwrap() = Some(Arc::new(Session { runner, ts, host: Mutex::new(Some(host)), hub }));
    Ok(())
}

/// Goes offline: stops the encoder, tells the viewers and the signaling server. Takes a few seconds.
pub fn stop(app: &AppState) {
    let Some(session) = app.share.session.lock().unwrap().take() else { return };
    app.log("[app] stopping");
    session.runner.stop();
    session.shut_down_network();
    app.log("[app] stopped by user");
}

impl Session {
    fn shut_down_network(&self) {
        self.hub.stop();
        if let Some(host) = self.host.lock().unwrap().take() {
            host.stop();
        }
        self.ts.close();
    }
}

/// Runs with every status tick: an encoder that gave up for good takes this computer offline instead of
/// advertising a dead stream, and viewers who left are forgotten by the remote control.
pub fn housekeeping(app: &AppState) {
    let Some(session) = current(app) else { return };
    let host = session.host.lock().unwrap().clone();
    let Some(host) = host else { return };
    if session.runner.state() == RunnerState::Failed {
        let session = session.clone();
        std::thread::spawn(move || session.shut_down_network());
        return;
    }
    let sids: Vec<String> = host.viewers().into_iter().map(|v| v.sid).collect();
    session.hub.keep_only(&sids);
}

/// The remote-control rule changed in the settings.
pub fn policy_changed(app: &AppState, policy: u8) {
    if let Some(session) = current(app) {
        session.hub.set_policy(policy);
    }
}

/// The user's answer to a viewer who asked for control (or taking it back).
pub fn control_set(app: &AppState, sid: &str, allow: bool) -> Result<(), String> {
    current(app).ok_or("Not sharing.")?.hub.decide(sid, allow)
}

pub fn status(app: &AppState) -> Value {
    let Some(session) = current(app) else {
        return json!({ "state": "idle", "detail": "", "attempt": 0, "uptimeSec": 0, "capture": null, "encode": null, "video": null, "p2p": null });
    };
    let run = session.runner.snapshot();
    let host = session.host.lock().unwrap().clone();
    let p2p = host.map(|host| {
        let viewers: Vec<Value> = host
            .viewers()
            .into_iter()
            .map(|v| {
                json!({
                    "sid": v.sid,
                    "name": v.name,
                    "state": if v.state == ViewerState::Watching { "watching" } else { "connecting" },
                    "ipv6": v.ipv6,
                    "path": v.path,
                    "rttMs": v.rtt_ms,
                    "latencyMs": v.latency_ms,
                    "overheadPercent": v.overhead_percent,
                    "control": session.hub.state_of(&v.sid),
                })
            })
            .collect();
        json!({
            "status": host.status(),
            "hasIpv6": host.has_ipv6(),
            "ipv4Open": host.ipv4_open(),
            "uploadMbps": host.upload_mbps(),
            "viewers": viewers,
        })
    });
    json!({
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
        "p2p": p2p,
    })
}
