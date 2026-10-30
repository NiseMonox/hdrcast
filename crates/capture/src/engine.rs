//! The capture engine: captures one desktop, has every frame converted on the GPU and writes a constant-rate
//! raw stream into the video pipe, plus desktop audio into the audio pipe (design.md section 2.2).
//! Platform-neutral: everything OS-specific sits behind [`crate::platform`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::args::{CaptureArgs, CaptureFormat};
use crate::audio::AudioPipeline;
use crate::encode::{EncodeControl, EncodeStage, VideoEncoder};
use crate::clock::{now_ticks, SessionClock, TICKS_PER_MS, TICKS_PER_SECOND};
use crate::color;
use crate::frame::{FramePool, FrameSink, SharedFrame};
use crate::log;
use crate::pacer::{EagerPacer, FramePacer};
use crate::platform::{self, Backend, FrameLayout, FrameStatus, VideoError, VideoSource};
use crate::stats::CaptureStats;
use crate::timer::HighResTimer;

/// Process exit codes of the capture engine (design.md section 2.1).
pub mod exit_codes {
    pub const OK: i32 = 0;
    pub const BAD_ARGUMENTS: i32 = 2;
    /// GPU / display error.
    pub const DEVICE_ERROR: i32 = 3;
    /// The pipe consumer went away.
    pub const PIPE_BROKEN: i32 = 4;
    /// The desktop duplication was lost and could not be restored.
    pub const ACCESS_LOST: i32 = 5;
    /// The pipe was created but ffmpeg never connected to it.
    pub const NO_PIPE_CLIENT: i32 = 6;
    /// `--encode` was asked for, and that encoder cannot be had here or failed while encoding (design.md section
    /// 2.8): the app encodes with ffmpeg instead.
    pub const ENCODER_UNAVAILABLE: i32 = 7;
}

/// Room the video pipe is created with (design.md section 2.2): a little over two 4K P010 frames, so a short
/// hiccup of the encoder does not stall the writer in the middle of a frame.
pub const VIDEO_PIPE_BUFFER_BYTES: usize = 64 << 20;

/// Frames that may wait for the pipe writer before the loop starts dropping.
const SINK_QUEUE_FRAMES: usize = 4;
/// The queue, the "latest frame" slot and the frame being converted: with the writer giving a buffer back
/// before its queue slot, renting can never fail. With zero copy the encoder adds the surfaces it holds itself.
const POOL_FRAMES: usize = SINK_QUEUE_FRAMES + 2;
/// Control mode: one picture at most waits for the encoder. A newer capture takes its turn rather than queueing
/// behind it, so what reaches the encoder is never older than the picture it is busy with.
const LOW_LATENCY_QUEUE_FRAMES: usize = 1;
/// Control mode with nothing new on the screen: the last picture goes again this often (design.md section 11.2).
const IDLE_REPEAT_MS: i64 = 100;

const ACCESS_LOST_RETRY_MS: u64 = 500;
const ACCESS_LOST_GIVE_UP_MS: i64 = 30000;

/// Asks a running engine to stop (Ctrl+C, console close, the app shutting down).
#[derive(Debug, Clone, Default)]
pub struct StopHandle(Arc<AtomicBool>);

impl StopHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request_stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn stop_requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// One capture session. [`run`](Self::run) blocks until the session ends and returns the exit code; another
/// thread ends it through the [`StopHandle`]. The engine is `Send`: build it anywhere, run it on its own thread.
pub struct CaptureEngine {
    args: CaptureArgs,
    backend: Box<dyn Backend>,
    stop: StopHandle,
    clock: Arc<SessionClock>,

    captured: u64,
    duplicated: u64,
    dropped: u64,
    gpu_ms_accum: f64,
    gpu_ms_count: u32,
    last_written: u64,
    stats_sink: Box<dyn FnMut(&CaptureStats) + Send>,
    layout_sink: Box<dyn FnMut(&FrameLayout) + Send>,
}

impl CaptureEngine {
    /// An engine on this platform's backend.
    pub fn new(args: CaptureArgs) -> Self {
        Self::with_backend(args, platform::native())
    }

    pub fn with_backend(args: CaptureArgs, backend: Box<dyn Backend>) -> Self {
        Self {
            args,
            backend,
            stop: StopHandle::new(),
            clock: Arc::new(SessionClock::new()),
            captured: 0,
            duplicated: 0,
            dropped: 0,
            gpu_ms_accum: 0.0,
            gpu_ms_count: 0,
            last_written: 0,
            stats_sink: Box::new(|stats| log::stdout_line(&stats.to_json_line())),
            layout_sink: Box::new(|layout| log::stdout_line(&layout.to_json_line())),
        }
    }

    /// Sends the once-per-second statistics to `sink` instead of printing them to stdout as a JSON line.
    pub fn set_stats_sink(&mut self, sink: impl FnMut(&CaptureStats) + Send + 'static) {
        self.stats_sink = Box::new(sink);
    }

    /// Sends the frame layout (once, when the display is open and before the pipes exist) to `sink` instead of
    /// printing it to stdout as a JSON line ([`FrameLayout::to_json_line`]).
    pub fn set_layout_sink(&mut self, sink: impl FnMut(&FrameLayout) + Send + 'static) {
        self.layout_sink = Box::new(sink);
    }

    pub fn stop_handle(&self) -> StopHandle {
        self.stop.clone()
    }

    pub fn request_stop(&self) {
        self.stop.request_stop();
    }

    /// Runs the session to its end and returns the process exit code (see [`exit_codes`]).
    pub fn run(&mut self) -> i32 {
        let mut video = match self.backend.open_video(&self.args) {
            Ok(video) => video,
            Err(message) => {
                log::error(format!("capture: {message}"));
                return exit_codes::DEVICE_ERROR;
            }
        };
        let layout = video.layout();
        // A format asked for by name is a promise to whoever reads the pipe: a backend that cannot keep it
        // (macOS without HDR capture on this system or display) must not put another one into the pipe, where
        // it would be read as noise. `--format auto` leaves the choice to the backend instead; the layout line
        // below says what it chose, and the app starts ffmpeg only once it has read that line.
        if self.args.format != CaptureFormat::Auto && layout.format != self.args.format {
            let (asked, got) = (self.args.format.name(), layout.format.name());
            log::error(format!(
                "capture: this display cannot be captured as {asked} here (it would come out as {got}); \
                 use --format {got} or --format auto"
            ));
            return exit_codes::DEVICE_ERROR;
        }
        let low_latency = self.args.encode.as_ref().is_some_and(|e| e.low_latency);
        let (queue_frames, pool_frames) =
            if low_latency { (LOW_LATENCY_QUEUE_FRAMES, LOW_LATENCY_QUEUE_FRAMES + 2) } else { (SINK_QUEUE_FRAMES, POOL_FRAMES) };
        if low_latency {
            if let Some(what) = video.raise_gpu_priority() {
                log::info(format!("capture: control mode: {what}"));
            }
        }
        // The encoder is opened before the layout is announced: when it cannot be had, the app learns it from the
        // exit code before it starts an ffmpeg that would wait for a stream that never comes.
        let encoder: Option<Box<dyn VideoEncoder>> = match &self.args.encode {
            None => None,
            Some(encode) => {
                let fps = self.args.fps.max(1);
                // Zero copy when the screen can hand its pictures to the encoder on the GPU (design.md section
                // 2.8), frames in memory otherwise.
                let opened = match video.surface_encoder(encode, fps, pool_frames) {
                    Some(encoder) => Ok(encoder),
                    None => self.backend.open_encoder(encode, layout, fps),
                };
                match opened {
                    Ok(encoder) => {
                        log::info(format!("capture: encoding here: {}", encoder.describe()));
                        Some(encoder)
                    }
                    Err(message) => {
                        log::error(format!("capture: cannot encode here: {message}"));
                        return exit_codes::ENCODER_UNAVAILABLE;
                    }
                }
            }
        };
        // Before any pipe exists: whoever waits for the pipes knows the format by the time they are there.
        (self.layout_sink)(&layout);

        // With zero copy the pictures stay on the GPU: one buffer without bytes per surface.
        let surfaces = encoder.as_ref().map_or(0, |encoder| encoder.surfaces());
        let pool = if surfaces > 0 {
            FramePool::new(surfaces, 0)
        } else {
            FramePool::new(pool_frames, layout.frame_bytes())
        };
        let video_path = platform::pipe_path(&self.args.video_pipe);
        let sink = match &self.args.dump_path {
            Some(path) => FrameSink::create_file(path, queue_frames)
                .map_err(|e| format!("cannot create the dump file {path}: {e}")),
            None => self
                .backend
                .create_pipe(&self.args.video_pipe, VIDEO_PIPE_BUFFER_BYTES)
                .map(|pipe| FrameSink::for_pipe(pipe, queue_frames))
                .map_err(|e| format!("cannot create the video pipe {video_path}: {e}")),
        };
        let mut sink = match sink {
            Ok(sink) => sink,
            Err(message) => {
                log::error(format!("capture: unhandled error: {message}"));
                return exit_codes::DEVICE_ERROR;
            }
        };
        if let (Some(encoder), Some(encode)) = (encoder, &self.args.encode) {
            let control = EncodeControl::new();
            read_commands(Arc::clone(&control));
            let stage = EncodeStage::new(
                encoder,
                self.args.fps.max(1),
                encode.gop_frames,
                control,
                Box::new(|event| log::stdout_line(&event.to_json_line())),
            );
            sink.encode_with(if encode.low_latency { stage.framed() } else { stage });
        }

        // The audio PIPE has to exist before the video pipe blocks for its client: ffmpeg opens both
        // inputs, so waiting for ffmpeg first would deadlock the two sides. Recording itself starts later,
        // once the consumer is there, so the two streams share a start time.
        let mut audio = None;
        if self.args.audio && self.args.dump_path.is_none() {
            match AudioPipeline::create(
                self.backend.as_ref(),
                &self.args.audio_pipe,
                Arc::clone(&self.clock),
            ) {
                Ok(pipeline) => audio = Some(pipeline),
                Err(e) => {
                    // ffmpeg is waiting for this pipe, so failing to create it is fatal.
                    log::error(format!("capture: cannot create the audio pipe: {e}"));
                    return exit_codes::PIPE_BROKEN;
                }
            }
        }

        if sink.is_pipe() {
            let seconds = self.args.pipe_timeout_seconds;
            log::info(format!(
                "capture: waiting for the video pipe client on {video_path} ({seconds}s)"
            ));
            if !sink.wait_for_client(Duration::from_secs(u64::from(seconds)), &self.stop) {
                let reason = sink.error().map(|e| format!(": {e}")).unwrap_or_default();
                log::error(format!(
                    "capture: ffmpeg did not connect to {video_path} within {seconds}s{reason}"
                ));
                return exit_codes::NO_PIPE_CLIENT;
            }
            log::info("capture: video pipe connected");
        }
        sink.start();

        if let Some(audio) = audio.as_mut() {
            // Opening the endpoint can take a while (a Bluetooth headset: hundreds of ms). The session clock
            // starts only afterwards, together with the frame pacer (see run_loop), and audio captured before
            // that is discarded: otherwise the sound would trail the picture by however long this took.
            audio.begin_capture(
                self.backend.as_ref(),
                &self.args.audio_device,
                &self.args.audio_exclude,
            );
            if audio.degraded() {
                log::warn("capture: desktop audio is unavailable; the stream carries silence");
            }
        }

        let exit_code = self.run_loop(video.as_mut(), &pool, surfaces > 0, low_latency, &sink, audio.as_ref());

        // Tear down in this order: the audio threads first, then the sink (drains its queue, bounded).
        drop(audio);
        drop(sink);
        drop(video);
        exit_code
    }

    /// `on_surfaces`: the pool's buffers stand for the encoder's surfaces, which the video source converts into
    /// (zero copy). `low_latency`: control mode, each picture goes the moment it is captured ([`EagerPacer`]) instead
    /// of on the constant beat.
    fn run_loop(
        &mut self,
        video: &mut dyn VideoSource,
        pool: &FramePool,
        on_surfaces: bool,
        low_latency: bool,
        sink: &FrameSink,
        audio: Option<&AudioPipeline>,
    ) -> i32 {
        let layout = video.layout();
        let fps = self.args.fps.max(1);
        let mut pacer = FramePacer::new(fps, TICKS_PER_SECOND);
        let mut eager = EagerPacer::new(fps, TICKS_PER_SECOND, IDLE_REPEAT_MS * TICKS_PER_MS);
        let parent = self.watch_parent();
        let mut timer = HighResTimer::new();

        // From here the audio clock and the frame pacer measure the same wall clock, from the same instant.
        //
        // The pacer works in absolute clock ticks because that is what HighResTimer::wait_until compares
        // against. Mixing in the session clock's elapsed ticks (which count from zero) made every deadline
        // look long past, so the loop never waited and spun at the desktop's refresh rate instead of the
        // requested frame rate.
        pacer.start(self.clock.start());

        // Start from a black frame so the encoder has valid input even before the first desktop update.
        let Some(mut first) = pool.try_rent() else {
            return exit_codes::DEVICE_ERROR;
        };
        if on_surfaces {
            if let Err(message) = video.black_surface(first.index()) {
                log::error(format!("capture: conversion failed: {message}"));
                return exit_codes::DEVICE_ERROR;
            }
        } else {
            color::fill_black(&mut first, layout.format, layout.width, layout.height);
        }
        let mut latest: SharedFrame = Arc::new(first);

        let mut last_stats_tick = now_ticks();
        let mut access_lost_since: Option<i64> = None;
        let mut exit_code = exit_codes::OK;
        // a new desktop image was converted since the last emitted frame
        let mut fresh_frame = false;
        // the sink refused the frame that is due and has not taken one since
        let mut backlogged = false;

        while !self.stop.stop_requested() {
            if parent.as_ref().is_some_and(|p| p.is_gone()) {
                log::info(format!(
                    "capture: parent process {} is gone, stopping",
                    self.args.parent_pid
                ));
                break;
            }
            if sink.broken() {
                let reason = sink.error().map(|e| format!(": {e}")).unwrap_or_default();
                if sink.encoder_failed() {
                    log::error(format!("capture: the encoder failed{reason}"));
                    exit_code = exit_codes::ENCODER_UNAVAILABLE;
                } else {
                    log::error(format!("capture: the pipe consumer went away{reason}"));
                    exit_code = exit_codes::PIPE_BROKEN;
                }
                break;
            }
            if self.args.dump_seconds > 0
                && self.clock.elapsed_seconds() >= f64::from(self.args.dump_seconds)
            {
                break;
            }

            // Control mode: nothing is captured while no picture may go (less than a frame interval since the last
            // one). Desktop duplication keeps the newest image meanwhile, so the one taken when the interval is over
            // is as fresh as any: converting every image of a 360 Hz desktop for 120 pictures a second only kept the
            // GPU, and the device the encoder shares with the converter, busy.
            if low_latency && !fresh_frame && !backlogged {
                let wait = eager.ticks_until_due(now_ticks(), true);
                if wait > 0 {
                    timer.wait_until(now_ticks() + wait);
                }
            }
            // Pull whatever the desktop has, without blocking past the next frame deadline. In control mode that is
            // when a waiting picture may go, or the idle repeat; a new desktop image ends the wait at once.
            let wait_ms = if low_latency {
                let ticks_left = if backlogged { TICKS_PER_MS } else { eager.ticks_until_due(now_ticks(), fresh_frame) };
                ((ticks_left + TICKS_PER_MS - 1) / TICKS_PER_MS).clamp(0, 8) as u64
            } else {
                (pacer.ticks_until_next(now_ticks()) / TICKS_PER_MS).clamp(0, 8) as u64
            };
            match pool.try_rent() {
                // every buffer is still queued: the consumer is behind
                None => self.dropped += 1,
                Some(mut target) => {
                    let timeout = Duration::from_millis(wait_ms);
                    let next = if on_surfaces {
                        video.next_surface(target.index(), timeout)
                    } else {
                        video.next_frame(&mut target, timeout)
                    };
                    match next {
                        Ok(FrameStatus::New { gpu_ms }) => {
                            access_lost_since = None;
                            self.gpu_ms_accum += gpu_ms;
                            self.gpu_ms_count += 1;
                            target.set_captured_ticks(now_ticks());
                            latest = Arc::new(target);
                            self.captured += 1;
                            fresh_frame = true;
                        }
                        Ok(FrameStatus::Unchanged) => access_lost_since = None,
                        Err(VideoError::Fatal(message)) => {
                            log::error(format!("capture: conversion failed: {message}"));
                            exit_code = exit_codes::DEVICE_ERROR;
                            break;
                        }
                        Err(VideoError::Lost(message)) => {
                            drop(target);
                            if let Some(message) = message {
                                log::error(format!("capture: acquire failed: {message}"));
                            }
                            match access_lost_since {
                                None => {
                                    access_lost_since = Some(now_ticks());
                                    log::warn(
                                    "capture: desktop duplication lost (mode change or full-screen app); retrying",
                                );
                                }
                                Some(since)
                                    if (now_ticks() - since) / TICKS_PER_MS
                                        > ACCESS_LOST_GIVE_UP_MS =>
                                {
                                    log::error(format!(
                                        "capture: could not recreate the duplication within {}s",
                                        ACCESS_LOST_GIVE_UP_MS / 1000
                                    ));
                                    exit_code = exit_codes::ACCESS_LOST;
                                    break;
                                }
                                Some(_) => {}
                            }
                            // Try immediately first: a full-screen toggle usually clears within a frame or two,
                            // and sleeping half a second every time would stall the stream for no reason.
                            match video.recover() {
                                Ok(()) => {
                                    access_lost_since = None;
                                    log::info("capture: duplication restored");
                                }
                                Err(message) => {
                                    log::warn(format!("capture: retry failed: {message}"));
                                    std::thread::sleep(Duration::from_millis(ACCESS_LOST_RETRY_MS));
                                }
                            }
                        }
                    }
                }
            }

            // Emit every frame whose deadline has passed (repeating the last image when idle).
            let now = now_ticks();
            let mut due = if low_latency { 0 } else { pacer.frames_due(now) };
            if low_latency && eager.due(now, fresh_frame) {
                if sink.try_enqueue(&latest) {
                    eager.sent(now);
                    if !fresh_frame {
                        self.duplicated += 1;
                    }
                    fresh_frame = false;
                    backlogged = false;
                } else if !backlogged {
                    // the encoder is still busy with the previous picture: this one waits (a newer one replaces it)
                    self.dropped += 1;
                    backlogged = true;
                }
            }
            if due > 0 {
                if due > i64::from(fps) {
                    // More than a second behind (suspend/resume, long stall): jump to now instead of catching up.
                    let skipped = pacer.skip_missed(now);
                    self.dropped += skipped.max(0) as u64;
                    due = pacer.frames_due(now);
                }
                let mut emitted = 0;
                for i in 0..due {
                    // A refused frame is not consumed, so it is re-offered on the next pass; counting it
                    // as dropped every time would inflate the number. Count the backlog once instead.
                    if !sink.try_enqueue(&latest) {
                        if i == 0 && !backlogged {
                            self.dropped += 1;
                            backlogged = true;
                        }
                        break;
                    }
                    backlogged = false;
                    emitted += 1;
                    // The first frame after a fresh capture carries new pixels; the rest repeat it.
                    if fresh_frame && i == 0 {
                        fresh_frame = false;
                    } else {
                        self.duplicated += 1;
                    }
                }
                pacer.consume(emitted);
                fresh_frame = false;
            }

            let since_stats = now_ticks() - last_stats_tick;
            if since_stats >= TICKS_PER_SECOND {
                last_stats_tick = now_ticks();
                self.emit_stats(sink, audio, Some(since_stats));
            }

            if low_latency {
                // the next pass waits in the capture itself (up to the moment something is due)
            } else if pacer.ticks_until_next(now_ticks()) > 0 {
                timer.wait_until(pacer.next_deadline_ticks());
            } else if backlogged {
                // The frame that is due cannot go anywhere until the writer frees a slot: do not spin on it.
                timer.wait_until(now_ticks() + TICKS_PER_MS);
            }
        }

        self.emit_stats(sink, audio, None);
        exit_code
    }

    /// Watches the parent through a single handle. Looking the process up by id on every pass would take a
    /// snapshot of every process on the machine, which at 60-120 fps inside the pacing loop costs real CPU;
    /// a handle is also immune to PID reuse (the PID cannot be recycled while we hold it open).
    fn watch_parent(&self) -> Option<Box<dyn platform::ProcessWatch>> {
        if self.args.parent_pid == 0 {
            return None;
        }
        let watch = self.backend.watch_process(self.args.parent_pid);
        if watch.is_none() {
            log::warn(format!(
                "capture: cannot watch parent process {}",
                self.args.parent_pid
            ));
        }
        watch
    }

    /// Prints the statistics line: over the last `interval_ticks`, or over the whole session at the end.
    fn emit_stats(
        &mut self,
        sink: &FrameSink,
        audio: Option<&AudioPipeline>,
        interval_ticks: Option<i64>,
    ) {
        let written = sink.frames_written();
        let (seconds, frames) = match interval_ticks {
            Some(ticks) => (
                ticks as f64 / TICKS_PER_SECOND as f64,
                written - self.last_written,
            ),
            None => (self.clock.elapsed_seconds(), written),
        };
        self.last_written = written;
        let stats = CaptureStats {
            t: self.clock.elapsed_seconds(),
            captured: self.captured,
            duplicated: self.duplicated,
            dropped: self.dropped,
            // Frames per second over the last interval, not the average since start: a stall has to be
            // visible in the app while it is happening.
            fps: if seconds > 0.0 {
                frames as f64 / seconds
            } else {
                0.0
            },
            gpu_ms: if self.gpu_ms_count > 0 {
                self.gpu_ms_accum / f64::from(self.gpu_ms_count)
            } else {
                0.0
            },
            audio_underrun: audio.map_or(0, AudioPipeline::underruns),
            pipe_queue_ms: sink.queued_frames() as f64 * (1000.0 / f64::from(self.args.fps.max(1))),
        };
        (self.stats_sink)(&stats);
        self.gpu_ms_accum = 0.0;
        self.gpu_ms_count = 0;
    }
}

/// Reads the app's commands for the encoder from stdin, one per line, until stdin ends (design.md section 2.8).
/// The thread is left to run: a read cannot be interrupted, and the process ends with the session.
fn read_commands(control: Arc<EncodeControl>) {
    let spawned = std::thread::Builder::new().name("encoder-commands".into()).spawn(move || {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if let Err(e) = control.command(&line) {
                log::warn(format!("capture: {e}"));
            }
        }
    });
    if let Err(e) = spawned {
        log::warn(format!("capture: cannot read commands from stdin: {e}"));
    }
}

// An engine can be moved to the thread that runs it.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<CaptureEngine>();
};
