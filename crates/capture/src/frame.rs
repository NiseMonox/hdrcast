//! Raw video frames: a fixed pool of buffers and the sink that writes them to the video pipe or a dump file.

use std::fs::File;
use std::io::{self, Write};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::encode::EncodeStage;
use crate::engine::StopHandle;
use crate::platform::{PipeCanceller, PipeServer};

struct PoolShared {
    /// The free buffers, each with its index in the pool.
    free: Mutex<Vec<(usize, Vec<u8>)>>,
}

/// Fixed set of pre-allocated frame buffers (no per-frame allocation at 25 MB per 4K P010 frame).
pub struct FramePool {
    shared: Arc<PoolShared>,
    count: usize,
    frame_bytes: usize,
}

impl FramePool {
    /// With `frame_bytes` 0 the buffers hold no bytes: each one only stands for the encoder surface of its
    /// [`index`](FrameBuffer::index) (zero copy, design.md section 2.8), and the pool keeps track of which
    /// surfaces are in use.
    ///
    /// # Panics
    /// When `count` is less than 2.
    pub fn new(count: usize, frame_bytes: usize) -> Self {
        assert!(count >= 2, "a frame pool needs at least two buffers");
        // Filled with a non-zero byte on purpose: zeroed memory would stay uncommitted until the first frame
        // is written into each buffer, and the page faults of a 25 MB buffer would land in the capture loop.
        let free = (0..count)
            .map(|index| (index, vec![0x10u8; frame_bytes]))
            .collect();
        Self {
            shared: Arc::new(PoolShared {
                free: Mutex::new(free),
            }),
            count,
            frame_bytes,
        }
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    /// A buffer to fill, or `None` when every buffer is in use.
    pub fn try_rent(&self) -> Option<FrameBuffer> {
        let (index, data) = self
            .shared
            .free
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop()?;
        Some(FrameBuffer {
            data,
            index,
            pool: Arc::clone(&self.shared),
        })
    }
}

/// One raw video frame from a [`FramePool`]; goes back to the pool when dropped. Share a finished frame
/// between the queue and the "latest frame" slot as a [`SharedFrame`].
pub struct FrameBuffer {
    data: Vec<u8>,
    index: usize,
    pool: Arc<PoolShared>,
}

/// A finished frame: the sink's queue and the engine's "latest frame" slot can hold the same buffer.
pub type SharedFrame = Arc<FrameBuffer>;

impl FrameBuffer {
    /// Which of the pool's buffers this is (0 up to the pool's count), the same every time it is rented. In a
    /// pool without bytes it names the encoder surface the picture is in.
    pub fn index(&self) -> usize {
        self.index
    }
}

impl Deref for FrameBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.data
    }
}

impl DerefMut for FrameBuffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

impl Drop for FrameBuffer {
    fn drop(&mut self) {
        let data = std::mem::take(&mut self.data);
        self.pool
            .free
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((self.index, data));
    }
}

enum SinkTarget {
    Pipe(Box<dyn PipeServer>),
    File(File),
}

impl SinkTarget {
    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        match self {
            SinkTarget::Pipe(pipe) => pipe.write_all(data),
            SinkTarget::File(file) => file.write_all(data),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            SinkTarget::Pipe(_) => Ok(()),
            SinkTarget::File(file) => file.flush(),
        }
    }
}

struct SinkShared {
    queued: AtomicUsize,
    written: AtomicU64,
    broken: AtomicBool,
    /// It broke because the encoder failed, not the consumer.
    encoder_failed: AtomicBool,
    closing: AtomicBool,
    error: Mutex<Option<String>>,
}

impl SinkShared {
    fn fail(&self, error: &io::Error) {
        *self.error.lock().unwrap_or_else(PoisonError::into_inner) = Some(error.to_string());
        self.broken.store(true, Ordering::SeqCst);
    }

    fn fail_encoder(&self, message: String) {
        *self.error.lock().unwrap_or_else(PoisonError::into_inner) = Some(message);
        self.encoder_failed.store(true, Ordering::SeqCst);
        self.broken.store(true, Ordering::SeqCst);
    }
}

/// Writes raw frames to the video pipe (server side, design.md section 2.2) or to a dump file on its own
/// thread. The capture loop enqueues shared frames; when the consumer (ffmpeg) falls behind the queue fills
/// up and the loop drops frames instead of stalling the GPU / pacer. With an [`EncodeStage`] the same thread
/// encodes each frame and writes the transport stream instead (design.md section 2.8).
pub struct FrameSink {
    shared: Arc<SinkShared>,
    capacity: usize,
    is_pipe: bool,
    canceller: Option<PipeCanceller>,
    /// The stream until the writer thread takes it over in [`start`](Self::start).
    target: Option<SinkTarget>,
    stage: Option<EncodeStage>,
    queue: Option<Sender<SharedFrame>>,
    pending_queue: Option<Receiver<SharedFrame>>,
    thread: Option<JoinHandle<()>>,
    done: Option<Receiver<()>>,
}

impl FrameSink {
    /// A sink on an already created pipe. `queue_capacity` frames may wait for the writer.
    pub fn for_pipe(pipe: Box<dyn PipeServer>, queue_capacity: usize) -> Self {
        let canceller = pipe.canceller();
        Self::new(SinkTarget::Pipe(pipe), Some(canceller), queue_capacity)
    }

    /// A sink that writes the frames to `path` (the `--dump` debug mode).
    pub fn create_file(path: &str, queue_capacity: usize) -> io::Result<Self> {
        Ok(Self::new(
            SinkTarget::File(File::create(path)?),
            None,
            queue_capacity,
        ))
    }

    fn new(target: SinkTarget, canceller: Option<PipeCanceller>, capacity: usize) -> Self {
        let (queue, pending_queue) = mpsc::channel();
        Self {
            shared: Arc::new(SinkShared {
                queued: AtomicUsize::new(0),
                written: AtomicU64::new(0),
                broken: AtomicBool::new(false),
                encoder_failed: AtomicBool::new(false),
                closing: AtomicBool::new(false),
                error: Mutex::new(None),
            }),
            capacity,
            is_pipe: matches!(target, SinkTarget::Pipe(_)),
            canceller,
            target: Some(target),
            stage: None,
            queue: Some(queue),
            pending_queue: Some(pending_queue),
            thread: None,
            done: None,
        }
    }

    pub fn is_pipe(&self) -> bool {
        self.is_pipe
    }

    /// True once a write failed: the consumer went away (or the encoder failed, see
    /// [`encoder_failed`](Self::encoder_failed)).
    pub fn broken(&self) -> bool {
        self.shared.broken.load(Ordering::SeqCst)
    }

    /// The sink broke because its encoder failed; [`error`](Self::error) says how.
    pub fn encoder_failed(&self) -> bool {
        self.shared.encoder_failed.load(Ordering::SeqCst)
    }

    /// Encodes every frame with `stage` and writes the transport stream instead of the raw frame. Call it before
    /// [`start`](Self::start).
    pub fn encode_with(&mut self, stage: EncodeStage) {
        self.stage = Some(stage);
    }

    pub fn error(&self) -> Option<String> {
        self.shared
            .error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn frames_written(&self) -> u64 {
        self.shared.written.load(Ordering::SeqCst)
    }

    pub fn queued_frames(&self) -> usize {
        self.shared.queued.load(Ordering::SeqCst)
    }

    /// Waits for the pipe client; returns true when connected (always true for files). Gives up after
    /// `timeout` or when `stop` is requested; the sink is broken then.
    pub fn wait_for_client(&mut self, timeout: Duration, stop: &StopHandle) -> bool {
        let Some(SinkTarget::Pipe(pipe)) = self.target.as_mut() else {
            return true;
        };
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() || stop.stop_requested() {
                break;
            }
            match pipe.wait_for_client(left.min(Duration::from_millis(50))) {
                Ok(true) => return true,
                Ok(false) => {}
                Err(e) => {
                    self.shared.fail(&e);
                    return false;
                }
            }
        }
        // Timed out or cancelled: nobody will ever read this pipe.
        self.shared.broken.store(true, Ordering::SeqCst);
        false
    }

    /// Starts the writer thread.
    pub fn start(&mut self) {
        let (Some(mut target), Some(queue)) = (self.target.take(), self.pending_queue.take())
        else {
            return;
        };
        let shared = Arc::clone(&self.shared);
        let mut stage = self.stage.take();
        let (done_tx, done_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("video-pipe-writer".into())
            .spawn(move || {
                for frame in queue {
                    if !shared.broken.load(Ordering::SeqCst) {
                        let written = match stage.as_mut() {
                            None => target.write_all(&frame),
                            Some(stage) => match stage.push(&frame) {
                                Ok(()) => target.write_all(stage.output()).inspect(|_| stage.written()),
                                Err(message) => {
                                    shared.fail_encoder(message);
                                    Ok(())
                                }
                            },
                        };
                        match written {
                            Ok(()) => {
                                shared.written.fetch_add(1, Ordering::SeqCst);
                            }
                            // "The pipe is being closed" / "broken pipe" = consumer went away (exit code 4).
                            Err(e) => shared.fail(&e),
                        }
                    }
                    // Give the buffer back before the slot: whoever sees a free slot can also rent a buffer.
                    drop(frame);
                    shared.queued.fetch_sub(1, Ordering::SeqCst);
                }
                if let Some(stage) = stage.as_mut().filter(|_| !shared.broken.load(Ordering::SeqCst)) {
                    // the pictures the encoder still holds (B-frames waiting for their reference)
                    match stage.finish() {
                        Ok(()) => {
                            if let Err(e) = target.write_all(stage.output()) {
                                shared.fail(&e);
                            }
                        }
                        Err(message) => shared.fail_encoder(message),
                    }
                }
                drop(stage);
                if !shared.broken.load(Ordering::SeqCst) {
                    let _ = target.flush();
                }
                drop(target);
                let _ = done_tx.send(());
            });
        match thread {
            Ok(handle) => {
                self.thread = Some(handle);
                self.done = Some(done_rx);
            }
            Err(e) => self.shared.fail(&e),
        }
    }

    /// Queues a frame (keeps its own reference). Returns false when the queue is full or the sink is broken.
    pub fn try_enqueue(&self, frame: &SharedFrame) -> bool {
        if self.broken() || self.shared.closing.load(Ordering::SeqCst) {
            return false;
        }
        let Some(queue) = &self.queue else {
            return false;
        };
        // Only this (the capture) thread adds, so check-then-add cannot overshoot the capacity.
        if self.shared.queued.load(Ordering::SeqCst) >= self.capacity {
            return false;
        }
        self.shared.queued.fetch_add(1, Ordering::SeqCst);
        if queue.send(Arc::clone(frame)).is_err() {
            self.shared.queued.fetch_sub(1, Ordering::SeqCst);
            return false;
        }
        true
    }

    /// Stops accepting frames, drains the queue (bounded wait) and closes the stream.
    pub fn close(&mut self) {
        self.shared.closing.store(true, Ordering::SeqCst);
        self.queue = None; // the writer's loop ends once the queue is drained
        self.pending_queue = None;
        self.target = None;
        let (Some(thread), Some(done)) = (self.thread.take(), self.done.take()) else {
            return;
        };
        let mut finished = !matches!(
            done.recv_timeout(Duration::from_secs(3)),
            Err(RecvTimeoutError::Timeout)
        );
        if !finished {
            // Still inside a write nobody reads: abort it, the rest of the queue is skipped.
            self.shared.broken.store(true, Ordering::SeqCst);
            if let Some(canceller) = &self.canceller {
                canceller.cancel();
            }
            finished = !matches!(
                done.recv_timeout(Duration::from_secs(1)),
                Err(RecvTimeoutError::Timeout)
            );
        }
        if finished {
            let _ = thread.join();
        }
    }
}

impl Drop for FrameSink {
    fn drop(&mut self) {
        self.close();
    }
}
