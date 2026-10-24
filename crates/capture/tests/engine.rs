//! The engine loop against a fake platform: pacing, frame repeats, start-up order, exit codes, audio
//! alignment. Runs everywhere - this is the contract a new backend (macOS) has to satisfy.

mod common;

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hdrcast_capture::encode::{EncodedPicture, VideoEncoder};
use hdrcast_capture::frame::SharedFrame;
use hdrcast_capture::platform::{
    AudioSink, AudioSource, Backend, FrameLayout, FrameStatus, PipeCanceller, PipeServer,
    ProcessWatch, VideoError, VideoSource,
};
use hdrcast_capture::{
    color, exit_codes, CaptureArgs, CaptureEngine, CaptureFormat, CaptureStats, EncodeArgs,
    EngineEncoder,
};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 36;

/// What the engine did, in order.
type Events = Arc<Mutex<Vec<String>>>;

fn note(events: &Events, event: impl Into<String>) {
    events.lock().unwrap().push(event.into());
}

/// A screen that changes every `change_every` and marks each new picture with its number.
struct FakeVideo {
    layout: FrameLayout,
    change_every: Duration,
    started: Instant,
    delivered: u64,
    /// Report "lost" once when this many pictures were delivered; `recover` then has to be called.
    lose_after: Option<u64>,
    lost: bool,
    fail_recover: bool,
    events: Events,
    /// Zero copy: the screen converts into the surfaces of an encoder it makes itself.
    zero_copy: Option<Arc<ZeroCopy>>,
}

impl FakeVideo {
    /// Waits up to `timeout` for the next picture; its number when one came.
    fn next_picture(&mut self, timeout: Duration) -> Result<Option<u64>, VideoError> {
        if self.lost {
            return Err(VideoError::Lost(None));
        }
        if self.lose_after == Some(self.delivered) {
            self.lose_after = None;
            self.lost = true;
            return Err(VideoError::Lost(Some("the fake screen went away".into())));
        }
        let next_change = self.change_every * (self.delivered as u32 + 1);
        let elapsed = self.started.elapsed();
        if elapsed < next_change {
            let wait = (next_change - elapsed).min(timeout);
            std::thread::sleep(wait);
            if self.started.elapsed() < next_change {
                return Ok(None);
            }
        }
        self.delivered += 1;
        Ok(Some(self.delivered))
    }
}

impl VideoSource for FakeVideo {
    fn layout(&self) -> FrameLayout {
        self.layout
    }

    fn next_frame(&mut self, dst: &mut [u8], timeout: Duration) -> Result<FrameStatus, VideoError> {
        assert_eq!(
            dst.len(),
            self.layout.frame_bytes(),
            "the engine hands over exactly one frame"
        );
        assert!(
            self.zero_copy.is_none(),
            "with zero copy nothing goes to memory"
        );
        match self.next_picture(timeout)? {
            Some(picture) => {
                dst.fill((picture % 200) as u8 + 20);
                Ok(FrameStatus::New { gpu_ms: 1.5 })
            }
            None => Ok(FrameStatus::Unchanged),
        }
    }

    fn recover(&mut self) -> Result<(), String> {
        note(&self.events, "recover");
        if self.fail_recover {
            return Err("still gone".into());
        }
        self.lost = false;
        Ok(())
    }

    fn surface_encoder(
        &mut self,
        _args: &EncodeArgs,
        _fps: u32,
        frames: usize,
    ) -> Option<Box<dyn VideoEncoder>> {
        let zero_copy = Arc::clone(self.zero_copy.as_ref()?);
        note(&self.events, format!("surface encoder for {frames} frames"));
        // Two more than the engine holds: the encoder keeps two frames back, like a run of B-frames.
        let surfaces = frames + 2;
        zero_copy
            .contents
            .lock()
            .unwrap()
            .resize(surfaces, u64::MAX);
        Some(Box::new(SurfaceEncoder {
            surfaces,
            zero_copy,
            held: std::collections::VecDeque::new(),
        }))
    }

    fn next_surface(
        &mut self,
        surface: usize,
        timeout: Duration,
    ) -> Result<FrameStatus, VideoError> {
        let zero_copy = Arc::clone(self.zero_copy.as_ref().expect("zero copy"));
        match self.next_picture(timeout)? {
            Some(picture) => {
                zero_copy.convert(surface, picture);
                Ok(FrameStatus::New { gpu_ms: 0.1 })
            }
            None => Ok(FrameStatus::Unchanged),
        }
    }

    fn black_surface(&mut self, surface: usize) -> Result<(), String> {
        note(&self.events, format!("black surface {surface}"));
        self.zero_copy
            .as_ref()
            .expect("zero copy")
            .convert(surface, 0);
        Ok(())
    }
}

/// Zero copy as the fake screen and the fake encoder see it.
#[derive(Default)]
struct ZeroCopy {
    /// The picture in each surface (0 = black, `u64::MAX` = nothing yet).
    contents: Mutex<Vec<u64>>,
    /// Surfaces the encoder holds (received and not passed on yet).
    held: Mutex<Vec<usize>>,
    /// The pictures the encoder passed on, in order.
    encoded: Mutex<Vec<u64>>,
    /// What went wrong: a surface converted into while it was in use.
    errors: Mutex<Vec<String>>,
}

impl ZeroCopy {
    fn convert(&self, surface: usize, picture: u64) {
        if self.held.lock().unwrap().contains(&surface) {
            self.errors.lock().unwrap().push(format!(
                "picture {picture} converted into surface {surface}, which the encoder holds"
            ));
        }
        self.contents.lock().unwrap()[surface] = picture;
    }
}

/// An encoder on surfaces that keeps the last two frames back before passing their pictures on, and checks that
/// a held surface still has the picture it had when it was handed over.
struct SurfaceEncoder {
    surfaces: usize,
    zero_copy: Arc<ZeroCopy>,
    /// Frame, its picture when it came, its index, keyframe.
    held: std::collections::VecDeque<(SharedFrame, u64, u64, bool)>,
}

impl SurfaceEncoder {
    fn pass_on(&mut self, keep: usize, out: &mut Vec<EncodedPicture>) {
        while self.held.len() > keep {
            let (frame, picture, index, keyframe) = self.held.pop_front().unwrap();
            let surface = frame.index();
            let now = self.zero_copy.contents.lock().unwrap()[surface];
            if now != picture {
                self.zero_copy.errors.lock().unwrap().push(format!(
                    "surface {surface} came with picture {picture} and holds {now} when encoded"
                ));
            }
            let mut held = self.zero_copy.held.lock().unwrap();
            let at = held.iter().position(|&s| s == surface).unwrap();
            held.remove(at);
            self.zero_copy.encoded.lock().unwrap().push(picture);
            out.push(EncodedPicture {
                data: vec![0, 0, 1, 0x26, 1, picture as u8],
                pts: index as i64,
                dts: index as i64 - 2,
                keyframe,
            });
        }
    }
}

impl VideoEncoder for SurfaceEncoder {
    fn describe(&self) -> String {
        "a fake encoder on surfaces".into()
    }

    fn surfaces(&self) -> usize {
        self.surfaces
    }

    fn encode(
        &mut self,
        frame: &SharedFrame,
        index: u64,
        keyframe: bool,
        out: &mut Vec<EncodedPicture>,
    ) -> Result<(), String> {
        assert!(frame.is_empty(), "a surface frame holds no bytes");
        let picture = self.zero_copy.contents.lock().unwrap()[frame.index()];
        self.zero_copy.held.lock().unwrap().push(frame.index());
        self.held
            .push_back((Arc::clone(frame), picture, index, keyframe));
        self.pass_on(2, out);
        Ok(())
    }

    fn set_bitrate(&mut self, _kbps: u32, _vbv_kbits: u32) -> Result<(), String> {
        Ok(())
    }

    fn finish(&mut self, out: &mut Vec<EncodedPicture>) -> Result<(), String> {
        self.pass_on(0, out);
        Ok(())
    }
}

/// The consumer end of a fake pipe.
#[derive(Default)]
struct PipeState {
    /// When the "client" connects (None = never).
    connect_at: Option<Instant>,
    bytes: Mutex<Vec<u8>>,
    total: AtomicU64,
    /// Writes fail once this many bytes went through (the consumer went away).
    break_after: Option<u64>,
    cancelled: AtomicBool,
}

struct FakePipe {
    name: String,
    state: Arc<PipeState>,
    events: Events,
    announced: bool,
}

impl PipeServer for FakePipe {
    fn wait_for_client(&mut self, timeout: Duration) -> io::Result<bool> {
        if !self.announced {
            self.announced = true;
            note(&self.events, format!("wait {}", self.name));
        }
        match self.state.connect_at {
            Some(at) if Instant::now() >= at => Ok(true),
            Some(at) => {
                std::thread::sleep(at.saturating_duration_since(Instant::now()).min(timeout));
                Ok(Instant::now() >= at)
            }
            None => {
                std::thread::sleep(timeout);
                Ok(false)
            }
        }
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        if self.state.cancelled.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        if self
            .state
            .break_after
            .is_some_and(|limit| self.state.total.load(Ordering::SeqCst) >= limit)
        {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the fake consumer went away",
            ));
        }
        self.state.bytes.lock().unwrap().extend_from_slice(data);
        self.state
            .total
            .fetch_add(data.len() as u64, Ordering::SeqCst);
        Ok(())
    }

    fn canceller(&self) -> PipeCanceller {
        let state = Arc::clone(&self.state);
        PipeCanceller::new(move || state.cancelled.store(true, Ordering::SeqCst))
    }
}

/// Delivers a 48 kHz stereo ramp in bursts, the way a polled loopback endpoint does, until dropped.
struct FakeAudio {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeAudio {
    fn start(sink: AudioSink, burst: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let started = Instant::now();
            let mut delivered = 0u64;
            while !thread_stop.load(Ordering::SeqCst) {
                std::thread::sleep(burst);
                // The endpoint renders continuously: everything since the last burst arrives at once.
                let available = (started.elapsed().as_secs_f64() * 48000.0) as u64;
                let samples: Vec<f32> = (delivered..available)
                    .flat_map(|_| [0.25f32, -0.25])
                    .collect();
                delivered = available;
                sink(&samples);
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl AudioSource for FakeAudio {
    fn description(&self) -> String {
        "fake 48000 Hz 2 ch".into()
    }
}

impl Drop for FakeAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct AlwaysAlive;

impl ProcessWatch for AlwaysAlive {
    fn is_gone(&self) -> bool {
        false
    }
}

/// Reports the process as gone after a while.
struct GoneAfter(Instant);

impl ProcessWatch for GoneAfter {
    fn is_gone(&self) -> bool {
        Instant::now() >= self.0
    }
}

#[derive(Clone)]
struct FakeBackend {
    events: Events,
    video_error: Option<String>,
    change_every: Duration,
    lose_after: Option<u64>,
    fail_recover: bool,
    video_pipe: Arc<PipeState>,
    audio_pipe: Arc<PipeState>,
    /// `None` = the endpoint cannot be opened.
    audio_burst: Option<Duration>,
    /// How long opening the endpoint takes: before it delivers anything, and after it began to deliver.
    audio_open_delay: (Duration, Duration),
    parent_gone_after: Option<Duration>,
    /// The format the screen comes out as whatever was asked for (a Mac whose system refuses HDR capture);
    /// `None` = p010 when asked for, nv12 otherwise.
    delivers: Option<CaptureFormat>,
    /// Every layout the engine announced, with the number of backend events before it.
    announced: Arc<Mutex<Vec<(FrameLayout, usize)>>>,
    /// The screen offers an encoder on surfaces (zero copy).
    zero_copy: Option<Arc<ZeroCopy>>,
}

impl FakeBackend {
    fn new() -> Self {
        let connected = || {
            Arc::new(PipeState {
                connect_at: Some(Instant::now()),
                ..PipeState::default()
            })
        };
        Self {
            events: Events::default(),
            video_error: None,
            change_every: Duration::from_millis(50),
            lose_after: None,
            fail_recover: false,
            video_pipe: connected(),
            audio_pipe: connected(),
            audio_burst: Some(Duration::from_millis(62)),
            audio_open_delay: (Duration::ZERO, Duration::ZERO),
            parent_gone_after: None,
            delivers: None,
            announced: Arc::default(),
            zero_copy: None,
        }
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    fn announced(&self) -> Vec<(FrameLayout, usize)> {
        self.announced.lock().unwrap().clone()
    }
}

impl Backend for FakeBackend {
    fn open_video(&self, args: &CaptureArgs) -> Result<Box<dyn VideoSource>, String> {
        note(&self.events, "open video");
        if let Some(error) = &self.video_error {
            return Err(error.clone());
        }
        let format = match (self.delivers, args.format) {
            (Some(format), _) => format,
            (None, CaptureFormat::P010) => CaptureFormat::P010,
            (None, _) => CaptureFormat::Nv12,
        };
        Ok(Box::new(FakeVideo {
            layout: FrameLayout {
                format,
                width: WIDTH,
                height: HEIGHT,
            },
            change_every: self.change_every,
            started: Instant::now(),
            delivered: 0,
            lose_after: self.lose_after,
            lost: false,
            fail_recover: self.fail_recover,
            events: Arc::clone(&self.events),
            zero_copy: self.zero_copy.clone(),
        }))
    }

    fn create_pipe(&self, name: &str, out_buffer_bytes: usize) -> io::Result<Box<dyn PipeServer>> {
        note(&self.events, format!("create {name} {out_buffer_bytes}"));
        let state = if name == "video" {
            &self.video_pipe
        } else {
            &self.audio_pipe
        };
        Ok(Box::new(FakePipe {
            name: name.to_owned(),
            state: Arc::clone(state),
            events: Arc::clone(&self.events),
            announced: false,
        }))
    }

    fn open_audio(
        &self,
        device: &str,
        exclude_app: &str,
        sink: AudioSink,
    ) -> Result<Box<dyn AudioSource>, String> {
        note(&self.events, format!("open audio {device} [{exclude_app}]"));
        match self.audio_burst {
            Some(burst) => {
                std::thread::sleep(self.audio_open_delay.0);
                let source = FakeAudio::start(sink, burst);
                std::thread::sleep(self.audio_open_delay.1);
                Ok(Box::new(source))
            }
            None => Err("no audio endpoint".into()),
        }
    }

    fn watch_process(&self, pid: u32) -> Option<Box<dyn ProcessWatch>> {
        note(&self.events, format!("watch {pid}"));
        Some(match self.parent_gone_after {
            Some(after) => Box::new(GoneAfter(Instant::now() + after)),
            None => Box::new(AlwaysAlive),
        })
    }

    fn self_test(&self, _sdr_white: f32) -> i32 {
        0
    }
}

fn args(fps: u32) -> CaptureArgs {
    CaptureArgs {
        fps,
        video_pipe: "video".into(),
        audio_pipe: "audio".into(),
        ..CaptureArgs::for_output(0)
    }
}

/// Runs an engine on `backend` for `run_for` (then asks it to stop); returns the exit code and the statistics
/// it reported.
fn run_engine(
    backend: &FakeBackend,
    args: CaptureArgs,
    run_for: Option<Duration>,
) -> (i32, Vec<CaptureStats>) {
    let stats = Arc::new(Mutex::new(Vec::new()));
    let mut engine = CaptureEngine::with_backend(args, Box::new(backend.clone()));
    let sink_stats = Arc::clone(&stats);
    engine.set_stats_sink(move |s| sink_stats.lock().unwrap().push(*s));
    let (announced, events) = (Arc::clone(&backend.announced), Arc::clone(&backend.events));
    engine.set_layout_sink(move |layout| {
        let before = events.lock().unwrap().len();
        announced.lock().unwrap().push((*layout, before));
    });
    let stop = engine.stop_handle();
    // The stopper gives up waiting as soon as the engine returned on its own (the sender is dropped).
    let (finished, engine_finished) = std::sync::mpsc::channel::<()>();
    let stopper = run_for.map(|run_for| {
        std::thread::spawn(move || {
            let _ = engine_finished.recv_timeout(run_for);
            stop.request_stop();
        })
    });
    let code = engine.run();
    drop(finished);
    if let Some(stopper) = stopper {
        stopper.join().unwrap();
    }
    let stats = stats.lock().unwrap().clone();
    (code, stats)
}

/// Every emitted frame is either a new picture or a repeat. (A picture converted just before the session
/// ended may not have been emitted any more.)
fn assert_accounted(stats: &CaptureStats, frames_written: u64) {
    let counted = stats.captured + stats.duplicated;
    assert!(
        counted == frames_written || counted == frames_written + 1,
        "{frames_written} frames written, {stats:?}"
    );
}

#[test]
fn a_dump_holds_the_frame_rate_and_repeats_the_last_picture_between_changes() {
    let dir = common::TempDir::new("engine-dump");
    let path = dir.file("frames.nv12");
    let backend = FakeBackend::new(); // the fake screen changes 20 times a second
    let capture = CaptureArgs {
        dump_path: Some(path.to_string_lossy().into_owned()),
        dump_seconds: 1,
        ..args(50)
    };
    let (code, stats) = run_engine(&backend, capture, None);
    assert_eq!(code, exit_codes::OK);

    let frame_bytes = color::frame_bytes(CaptureFormat::Nv12, WIDTH, HEIGHT);
    let dump = std::fs::read(&path).unwrap();
    assert_eq!(dump.len() % frame_bytes, 0, "only whole frames are written");
    let frames = dump.len() / frame_bytes;
    // One second at 50 fps: frames 0..=50 have a deadline within the second.
    assert!((48..=52).contains(&frames), "{frames} frames");

    // The stream starts with black (nothing was captured yet: Y 16, chroma 128), repeated until the first
    // picture arrives; every later frame is a whole picture, in order.
    let mut black = vec![16u8; frame_bytes];
    black[color::uv_plane_offset(CaptureFormat::Nv12, WIDTH, HEIGHT)..].fill(128);
    let black_frames = dump
        .chunks_exact(frame_bytes)
        .take_while(|f| *f == &black[..])
        .count();
    assert!(
        (1..=5).contains(&black_frames),
        "{black_frames} black frames before the first picture (due after 50 ms)"
    );
    let marks: Vec<u8> = dump
        .chunks_exact(frame_bytes)
        .skip(black_frames)
        .map(|f| f[0])
        .collect();
    for (frame, mark) in dump
        .chunks_exact(frame_bytes)
        .skip(black_frames)
        .zip(&marks)
    {
        assert!(
            frame.iter().all(|b| b == mark),
            "a frame mixes two pictures"
        );
    }
    assert!(
        marks.windows(2).all(|w| w[0] <= w[1]),
        "pictures out of order: {marks:?}"
    );
    let distinct = marks
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    assert!(
        (17..=22).contains(&distinct),
        "{distinct} different pictures"
    );

    // The statistics add up: every emitted frame is either a new picture or a repeat.
    let last = stats.last().expect("a final statistics line");
    assert_accounted(last, frames as u64);
    assert!((17..=22).contains(&last.captured), "{last:?}");
    assert_eq!(last.dropped, 0);
    assert_eq!(last.gpu_ms, 1.5);
    assert!((46.0..=54.0).contains(&last.fps), "{last:?}");
    // No audio and no pipes in dump mode.
    assert_eq!(backend.events(), ["open video"]);
}

#[test]
fn both_pipes_exist_before_the_engine_waits_for_the_video_client() {
    let mut backend = FakeBackend::new();
    // ffmpeg connects after a moment, reads for a while and goes away.
    backend.video_pipe = Arc::new(PipeState {
        connect_at: Some(Instant::now() + Duration::from_millis(300)),
        break_after: Some(40 * color::frame_bytes(CaptureFormat::P010, WIDTH, HEIGHT) as u64),
        ..PipeState::default()
    });
    let capture = CaptureArgs {
        format: CaptureFormat::P010,
        parent_pid: 77,
        audio_exclude: "Discord.exe".into(),
        ..args(100)
    };
    let (code, stats) = run_engine(&backend, capture, Some(Duration::from_secs(10)));
    assert_eq!(code, exit_codes::PIPE_BROKEN, "the consumer went away");

    let events = backend.events();
    let position = |event: &str| {
        events
            .iter()
            .position(|e| e == event)
            .unwrap_or_else(|| panic!("no '{event}' in {events:?}"))
    };
    // ffmpeg opens both inputs: with only the video pipe there the two sides would wait for each other.
    assert_eq!(events[0], "open video");
    assert_eq!(events[1], format!("create video {}", 64 << 20));
    assert_eq!(events[2], format!("create audio {}", 8 << 20));
    assert!(position("wait video") > position("create audio 8388608"));
    // Recording starts only once the consumer is there, so both streams share a start time.
    assert!(position("open audio default [Discord.exe]") > position("wait video"));
    assert!(position("watch 77") > position("wait video"));

    let frame_bytes = color::frame_bytes(CaptureFormat::P010, WIDTH, HEIGHT) as u64;
    assert_eq!(
        backend.video_pipe.total.load(Ordering::SeqCst),
        40 * frame_bytes
    );
    // Whole frames, each one picture (the screen changed while the engine waited, so there is no black lead-in).
    let video = backend.video_pipe.bytes.lock().unwrap();
    for frame in video.chunks_exact(frame_bytes as usize) {
        assert!(
            frame.iter().all(|b| *b == frame[0]) && frame[0] > 20,
            "a frame mixes two pictures"
        );
    }
    assert!(!stats.is_empty());
}

#[test]
fn the_engine_gives_up_when_ffmpeg_never_connects() {
    let mut backend = FakeBackend::new();
    backend.video_pipe = Arc::new(PipeState::default()); // nobody ever connects
    let started = Instant::now();
    let (code, stats) = run_engine(
        &backend,
        CaptureArgs {
            pipe_timeout_seconds: 1,
            ..args(60)
        },
        None,
    );
    assert_eq!(code, exit_codes::NO_PIPE_CLIENT);
    assert!(
        started.elapsed() >= Duration::from_millis(900)
            && started.elapsed() < Duration::from_secs(5)
    );
    assert!(stats.is_empty(), "the loop never ran");
    // The audio pipe was there all along, but nothing was recorded.
    let events = backend.events();
    assert!(events.contains(&"create audio 8388608".to_owned()));
    assert!(!events.iter().any(|e| e.starts_with("open audio")));
}

#[test]
fn a_stop_request_ends_the_wait_for_the_client() {
    let mut backend = FakeBackend::new();
    backend.video_pipe = Arc::new(PipeState::default());
    let started = Instant::now();
    let (code, _) = run_engine(&backend, args(60), Some(Duration::from_millis(300)));
    assert_eq!(code, exit_codes::NO_PIPE_CLIENT);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the 30 s timeout was not cut short"
    );
}

#[test]
fn a_display_that_cannot_be_opened_is_a_device_error() {
    let mut backend = FakeBackend::new();
    backend.video_error = Some("DXGI output 9 does not exist (run list-outputs)".into());
    let (code, stats) = run_engine(&backend, args(60), None);
    assert_eq!(code, exit_codes::DEVICE_ERROR);
    assert!(stats.is_empty());
    assert_eq!(
        backend.events(),
        ["open video"],
        "no pipe is created for a display that is not there"
    );
}

#[test]
fn with_format_auto_the_backend_chooses_and_says_so_before_the_pipes_exist() {
    let mut backend = FakeBackend::new();
    // An HDR display on a Mac whose system refuses the HDR pixel formats: the backend falls back to SDR.
    backend.delivers = Some(CaptureFormat::Nv12);
    backend.audio_burst = None;
    let (code, stats) = run_engine(
        &backend,
        CaptureArgs {
            format: CaptureFormat::Auto,
            audio: false,
            ..args(50)
        },
        Some(Duration::from_millis(500)),
    );
    assert_eq!(code, exit_codes::OK);

    let events = backend.events();
    let announced = backend.announced();
    let layout = FrameLayout {
        format: CaptureFormat::Nv12,
        width: WIDTH,
        height: HEIGHT,
    };
    // Once, after the display was opened and before the first pipe: the app starts ffmpeg as soon as the
    // pipes are there, so by then it has to know what to tell it.
    assert_eq!(announced, [(layout, 1)], "{events:?}");
    assert_eq!(events[0], "open video");
    assert_eq!(events[1], format!("create video {}", 64 << 20));

    // and the pipe carries what was announced
    let frame_bytes = layout.frame_bytes() as u64;
    let total = backend.video_pipe.total.load(Ordering::SeqCst);
    assert!(total > 0 && total % frame_bytes == 0, "{total} bytes");
    assert_accounted(stats.last().unwrap(), total / frame_bytes);
}

#[test]
fn a_format_asked_for_by_name_that_cannot_be_had_is_refused() {
    let mut backend = FakeBackend::new();
    backend.delivers = Some(CaptureFormat::Nv12);
    let (code, stats) = run_engine(
        &backend,
        CaptureArgs {
            format: CaptureFormat::P010,
            ..args(60)
        },
        None,
    );
    assert_eq!(code, exit_codes::DEVICE_ERROR);
    assert!(stats.is_empty());
    // No pipe and no layout: whoever asked for p010 would read nv12 as noise.
    assert_eq!(backend.events(), ["open video"]);
    assert!(backend.announced().is_empty());
}

#[test]
fn a_dump_announces_its_layout_too() {
    let dir = common::TempDir::new("engine-dump-layout");
    let backend = FakeBackend::new();
    let (code, _) = run_engine(
        &backend,
        CaptureArgs {
            format: CaptureFormat::P010,
            dump_path: Some(dir.file("frames.p010").to_string_lossy().into_owned()),
            dump_seconds: 1,
            ..args(30)
        },
        None,
    );
    assert_eq!(code, exit_codes::OK);
    let layout = FrameLayout {
        format: CaptureFormat::P010,
        width: WIDTH,
        height: HEIGHT,
    };
    assert_eq!(backend.announced(), [(layout, 1)]);
}

#[test]
fn a_lost_capture_is_recreated_and_the_stream_goes_on() {
    let mut backend = FakeBackend::new();
    backend.lose_after = Some(3);
    backend.audio_burst = None;
    let (code, stats) = run_engine(
        &backend,
        CaptureArgs {
            audio: false,
            ..args(50)
        },
        Some(Duration::from_millis(1200)),
    );
    assert_eq!(code, exit_codes::OK);
    assert_eq!(
        backend.events().iter().filter(|e| *e == "recover").count(),
        1
    );
    let last = stats.last().unwrap();
    assert!(
        last.captured > 10,
        "the capture went on after the loss: {last:?}"
    );
    let frame_bytes = color::frame_bytes(CaptureFormat::Nv12, WIDTH, HEIGHT) as u64;
    let written = backend.video_pipe.total.load(Ordering::SeqCst) / frame_bytes;
    assert!(
        (55..=65).contains(&written),
        "{written} frames in 1.2 s at 50 fps"
    );
    assert_accounted(last, written);
}

#[test]
fn the_engine_stops_when_its_parent_is_gone() {
    let mut backend = FakeBackend::new();
    backend.parent_gone_after = Some(Duration::from_millis(400));
    let started = Instant::now();
    let (code, _) = run_engine(
        &backend,
        CaptureArgs {
            audio: false,
            parent_pid: 4321,
            ..args(60)
        },
        Some(Duration::from_secs(20)),
    );
    assert_eq!(code, exit_codes::OK);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(backend.events().contains(&"watch 4321".to_owned()));
}

/// f32le stereo from the audio pipe as (frames, frames that are not silence).
fn audio_frames(pipe: &PipeState) -> (usize, usize) {
    let bytes = pipe.bytes.lock().unwrap();
    assert_eq!(bytes.len() % 8, 0, "whole stereo frames");
    let loud = bytes
        .chunks_exact(8)
        .filter(|frame| frame.iter().any(|b| *b != 0))
        .count();
    (bytes.len() / 8, loud)
}

#[test]
fn bursty_audio_reaches_the_pipe_untouched_and_on_the_video_clock() {
    let backend = FakeBackend::new(); // audio arrives every ~62 ms, like a polled loopback endpoint
    let (code, stats) = run_engine(&backend, args(50), Some(Duration::from_millis(2000)));
    assert_eq!(code, exit_codes::OK);
    let last = stats.last().unwrap();
    assert_eq!(
        last.audio_underrun, 0,
        "no silence was patched in between the bursts"
    );

    let (frames, loud) = audio_frames(&backend.audio_pipe);
    // Two seconds of audio, give or take the last burst and the alignment tolerance.
    let seconds = last.t;
    let expected = seconds * 48000.0;
    assert!(
        (frames as f64 - expected).abs() < 0.2 * 48000.0,
        "{frames} frames for {seconds:.2} s"
    );
    assert_eq!(loud, frames, "everything in the pipe is captured audio");
    // Left / right stay in place (an odd number of dropped samples would swap them).
    let bytes = backend.audio_pipe.bytes.lock().unwrap();
    assert_eq!(
        &bytes[..8],
        &[0.25f32.to_le_bytes(), (-0.25f32).to_le_bytes()].concat()[..]
    );
}

/// A slow endpoint (a Bluetooth headset takes hundreds of ms to open) must not shift the sound against the
/// picture: both streams start with the loop. Counting the audio from before the endpoint was opened put
/// that much silence in front of it, i.e. the sound trailed the picture for the whole session.
#[test]
fn a_slow_audio_start_does_not_delay_the_sound_against_the_picture() {
    let slow = Duration::from_millis(400);
    // The endpoint is slow to deliver its first sample / it delivers at once but the rest of the set-up drags.
    for delay in [(slow, Duration::ZERO), (Duration::ZERO, slow)] {
        let mut backend = FakeBackend::new();
        backend.audio_burst = Some(Duration::from_millis(10));
        backend.audio_open_delay = delay;
        let (code, stats) = run_engine(&backend, args(50), Some(Duration::from_millis(1700)));
        assert_eq!(code, exit_codes::OK);
        let last = stats.last().unwrap();
        assert_eq!(
            last.audio_underrun, 0,
            "{delay:?}: no silence was put in front of the audio: {last:?}"
        );

        let (frames, loud) = audio_frames(&backend.audio_pipe);
        assert_eq!(
            loud, frames,
            "{delay:?}: the audio track starts with captured sound, not with padding"
        );
        // Picture and sound cover the same stretch of time (the session clock started with the loop).
        let frame_bytes = color::frame_bytes(CaptureFormat::Nv12, WIDTH, HEIGHT) as u64;
        let video_frames = backend.video_pipe.total.load(Ordering::SeqCst) / frame_bytes;
        let (video_seconds, audio_seconds) = (video_frames as f64 / 50.0, frames as f64 / 48000.0);
        assert!(
            (video_seconds - audio_seconds).abs() < 0.12,
            "{delay:?}: {video_seconds:.2} s of video, {audio_seconds:.2} s of audio"
        );
        assert!(
            (0.9..=1.6).contains(&last.t),
            "{delay:?}: the clock does not count the set-up: {last:?}"
        );
    }
}

#[test]
fn the_audio_pipe_carries_silence_when_the_endpoint_cannot_be_opened() {
    let mut backend = FakeBackend::new();
    backend.audio_burst = None;
    let (code, stats) = run_engine(&backend, args(50), Some(Duration::from_millis(1500)));
    assert_eq!(code, exit_codes::OK);
    let last = stats.last().unwrap();
    assert!(
        last.audio_underrun >= 1,
        "the silence shows up in the statistics: {last:?}"
    );

    let (frames, loud) = audio_frames(&backend.audio_pipe);
    assert_eq!(loud, 0);
    // ffmpeg was told there is an audio input: the track has to keep up with the clock.
    assert!(
        (frames as f64 - last.t * 48000.0).abs() < 0.25 * 48000.0,
        "{frames} frames for {:.2} s",
        last.t
    );
}

#[test]
fn a_stalled_consumer_is_counted_once_and_not_once_per_spin() {
    let mut backend = FakeBackend::new();
    // The consumer takes 30 frames and then blocks forever (a full pipe): the queue fills up.
    struct Stalling(Arc<PipeState>, u64);
    impl PipeServer for Stalling {
        fn wait_for_client(&mut self, _timeout: Duration) -> io::Result<bool> {
            Ok(true)
        }
        fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
            if self.0.total.load(Ordering::SeqCst) >= self.1 {
                while !self.0.cancelled.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            self.0.total.fetch_add(data.len() as u64, Ordering::SeqCst);
            Ok(())
        }
        fn canceller(&self) -> PipeCanceller {
            let state = Arc::clone(&self.0);
            PipeCanceller::new(move || state.cancelled.store(true, Ordering::SeqCst))
        }
    }
    #[derive(Clone)]
    struct StallingBackend(FakeBackend, u64);
    impl Backend for StallingBackend {
        fn open_video(&self, args: &CaptureArgs) -> Result<Box<dyn VideoSource>, String> {
            self.0.open_video(args)
        }
        fn create_pipe(&self, _name: &str, _bytes: usize) -> io::Result<Box<dyn PipeServer>> {
            Ok(Box::new(Stalling(Arc::clone(&self.0.video_pipe), self.1)))
        }
        fn open_audio(
            &self,
            device: &str,
            exclude_app: &str,
            sink: AudioSink,
        ) -> Result<Box<dyn AudioSource>, String> {
            self.0.open_audio(device, exclude_app, sink)
        }
        fn watch_process(&self, pid: u32) -> Option<Box<dyn ProcessWatch>> {
            self.0.watch_process(pid)
        }
        fn self_test(&self, _sdr_white: f32) -> i32 {
            0
        }
    }
    backend.audio_burst = None;
    let frame_bytes = color::frame_bytes(CaptureFormat::Nv12, WIDTH, HEIGHT) as u64;
    let stalling = StallingBackend(backend.clone(), 30 * frame_bytes);

    let stats = Arc::new(Mutex::new(Vec::new()));
    let mut engine = CaptureEngine::with_backend(
        CaptureArgs {
            audio: false,
            ..args(100)
        },
        Box::new(stalling),
    );
    let sink_stats = Arc::clone(&stats);
    engine.set_stats_sink(move |s: &CaptureStats| sink_stats.lock().unwrap().push(*s));
    let stop = engine.stop_handle();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(800));
        stop.request_stop();
    });
    let started = Instant::now();
    let code = engine.run();
    stopper.join().unwrap();
    assert_eq!(code, exit_codes::OK);
    // Tearing down a sink whose writer is stuck is bounded (3 s drain + abort).
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "shutdown took {:?}",
        started.elapsed()
    );

    let last = *stats.lock().unwrap().last().unwrap();
    // About half a second of stall at 100 fps: one backlog, not thousands of "drops".
    assert_eq!(last.dropped, 1, "{last:?}");
    assert_eq!(
        backend.video_pipe.total.load(Ordering::SeqCst),
        30 * frame_bytes
    );
}

/// Zero copy (design.md section 2.8): the screen converts straight into the surfaces of an encoder it made, and
/// the engine keeps track of them with its frame pool (one buffer without bytes per surface). It starts with a
/// black surface, repeats a picture by handing the same surface over again, and never converts into a surface
/// that is queued, is the latest frame or is still held by the encoder.
#[test]
fn with_zero_copy_no_surface_is_converted_into_while_it_is_in_use() {
    let zero_copy = Arc::new(ZeroCopy::default());
    let mut backend = FakeBackend::new();
    // New pictures more often than frames go out (100 fps), and sometimes not: both, repeats and fresh ones.
    backend.change_every = Duration::from_millis(7);
    backend.zero_copy = Some(Arc::clone(&zero_copy));
    let capture = CaptureArgs {
        audio: false,
        encode: Some(EncodeArgs::defaults(EngineEncoder::Nvenc, 100)),
        ..args(100)
    };
    let (code, stats) = run_engine(&backend, capture, Some(Duration::from_millis(1500)));
    assert_eq!(code, exit_codes::OK);

    let events = backend.events();
    assert_eq!(events[..2], ["open video", "surface encoder for 6 frames"]);
    assert!(
        events.contains(&"black surface 7".to_owned()),
        "the pool hands out its last buffer first: {events:?}"
    );
    let errors = zero_copy.errors.lock().unwrap();
    assert!(errors.is_empty(), "{errors:#?}");
    let encoded = zero_copy.encoded.lock().unwrap();
    assert_eq!(
        encoded.first(),
        Some(&0),
        "the stream starts with the black surface"
    );
    assert!(
        encoded.windows(2).all(|w| w[0] <= w[1]),
        "pictures out of order: {encoded:?}"
    );
    // Every frame that went out was passed on by the encoder (the last two by `finish`), as MPEG-TS.
    let last = stats.last().expect("a final statistics line");
    assert!(
        encoded.len() as u64 + 1 >= last.captured,
        "{} pictures, {last:?}",
        encoded.len()
    );
    assert!(
        (120..=160).contains(&encoded.len()),
        "{} pictures",
        encoded.len()
    );
    assert_eq!(last.dropped, 0, "{last:?}");
    assert_eq!(backend.video_pipe.total.load(Ordering::SeqCst) % 188, 0);
}
