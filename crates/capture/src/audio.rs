//! Desktop audio on its way to the audio pipe (design.md section 2.4): the platform's [`AudioSource`] delivers
//! f32 48 kHz stereo, this keeps it locked to the video clock and writes it to the pipe as f32le.
//!
//! The stream is kept locked to the video clock by [`AudioClockAligner`]: silence is inserted when we fall
//! behind, surplus frames are dropped when we run ahead, so ffmpeg can use ideal timestamps on both inputs.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::aligner::AudioClockAligner;
use crate::clock::{SessionClock, TICKS_PER_SECOND};
use crate::log;
use crate::platform::{AudioSink, AudioSource, Backend, PipeCanceller, PipeServer};
use crate::resample::{TARGET_CHANNELS, TARGET_SAMPLE_RATE};

/// Room the audio pipe is created with.
pub const AUDIO_PIPE_BUFFER_BYTES: usize = 8 << 20;

/// Converted samples waiting for the pipe writer: 4 s.
const RING_SAMPLES: usize = TARGET_SAMPLE_RATE as usize * TARGET_CHANNELS * 4;

/// Fixed-size FIFO of samples.
struct SampleRing {
    buffer: Box<[f32]>,
    head: usize,
    len: usize,
}

impl SampleRing {
    fn new(capacity: usize) -> Self {
        Self {
            buffer: vec![0.0; capacity].into_boxed_slice(),
            head: 0,
            len: 0,
        }
    }

    fn capacity(&self) -> usize {
        self.buffer.len()
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push_back(&mut self, sample: f32) {
        let tail = (self.head + self.len) % self.buffer.len();
        self.buffer[tail] = sample;
        self.len += 1;
    }

    fn write(&mut self, data: &[f32]) {
        // More than the whole ring at once: only the newest part can stay.
        let data = &data[data.len().saturating_sub(self.capacity())..];
        if self.len + data.len() > self.capacity() {
            // Overflow: the consumer is far behind (ffmpeg not reading). Drop the oldest samples.
            let drop = self.len + data.len() - self.capacity();
            self.head = (self.head + drop) % self.buffer.len();
            self.len -= drop;
        }
        for &sample in data {
            self.push_back(sample);
        }
    }

    fn write_silence(&mut self, count: usize) {
        let count = count.min(self.capacity() - self.len);
        for _ in 0..count {
            self.push_back(0.0);
        }
    }

    fn read(&mut self, out: &mut [f32]) -> usize {
        let take = self.len.min(out.len());
        for slot in &mut out[..take] {
            *slot = self.buffer[self.head];
            self.head = (self.head + 1) % self.buffer.len();
        }
        self.len -= take;
        take
    }
}

struct AudioState {
    aligner: AudioClockAligner,
    ring: SampleRing,
}

struct AudioShared {
    state: Mutex<AudioState>,
    wake: Condvar,
    running: AtomicBool,
    client_connected: AtomicBool,
    clock: Arc<SessionClock>,
}

impl AudioShared {
    /// A captured chunk (interleaved stereo at 48 kHz) from the source's thread.
    fn on_audio(&self, samples: &[f32]) {
        let frames = samples.len() / TARGET_CHANNELS;
        if frames == 0 || !self.running.load(Ordering::SeqCst) {
            return;
        }
        // The stream starts when the session clock does (with the first video frame). What the endpoint
        // delivered while the rest of the audio set-up was still under way belongs to no position in it.
        if !self.clock.is_running() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let expected = state
            .aligner
            .expected_frames(self.clock.elapsed_ticks(), TICKS_PER_SECOND);
        let align = state.aligner.align(expected, frames);
        let dropped = align.drop_frames.min(frames);
        state
            .ring
            .write(&samples[dropped * TARGET_CHANNELS..frames * TARGET_CHANNELS]);
        if align.pad_frames > 0 {
            state.ring.write_silence(align.pad_frames * TARGET_CHANNELS);
        }
        self.wake.notify_one();
    }

    fn writer_loop(&self, pipe: &mut dyn PipeServer) -> io::Result<()> {
        loop {
            if !self.running.load(Ordering::SeqCst) {
                return Ok(());
            }
            if pipe.wait_for_client(Duration::from_millis(100))? {
                break;
            }
        }
        self.client_connected.store(true, Ordering::SeqCst);
        log::info("audio: pipe client connected");

        let mut samples = [0f32; 4096];
        let mut bytes = [0u8; 4096 * 4];
        while self.running.load(Ordering::SeqCst) {
            let take = {
                let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                if state.ring.is_empty() {
                    state = self
                        .wake
                        .wait_timeout(state, Duration::from_millis(20))
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
                if state.ring.is_empty() {
                    // Nothing captured: either the endpoint is silent/idle or it never opened. Top the
                    // stream up from the same clock the video uses so ffmpeg's audio track keeps moving
                    // and the muxer does not stall waiting for samples.
                    let expected = state
                        .aligner
                        .expected_frames(self.clock.elapsed_ticks(), TICKS_PER_SECOND);
                    let pad = state.aligner.pad_if_behind(expected);
                    if pad > 0 {
                        state.ring.write_silence(pad * TARGET_CHANNELS);
                    }
                }
                state.ring.read(&mut samples)
            };
            if take == 0 {
                continue;
            }
            for (sample, out) in samples[..take].iter().zip(bytes.chunks_exact_mut(4)) {
                out.copy_from_slice(&sample.to_le_bytes());
            }
            pipe.write_all(&bytes[..take * 4])?;
        }
        Ok(())
    }
}

/// The audio half of a capture session: the audio pipe, its writer thread and (once
/// [`begin_capture`](Self::begin_capture) ran) the platform's audio source.
pub struct AudioPipeline {
    shared: Arc<AudioShared>,
    canceller: PipeCanceller,
    writer: Option<JoinHandle<()>>,
    writer_done: Receiver<()>,
    source: Option<Box<dyn AudioSource>>,
    degraded: bool,
    format_description: String,
}

impl AudioPipeline {
    /// Creates the pipe and its writer thread. Done early, before the video pipe waits for ffmpeg: ffmpeg
    /// opens both inputs, so both pipes have to exist or the two sides wait for each other forever.
    ///
    /// The pipe comes first and the audio endpoint second ([`begin_capture`](Self::begin_capture)), on
    /// purpose: ffmpeg was started with an audio input, so the pipe MUST exist even when the endpoint cannot
    /// be opened (unplugged headphones, a stale device id in the settings, no render endpoint at all). If the
    /// capture fails the stream degrades to silence and the session keeps running, instead of ffmpeg dying on
    /// a missing pipe and the app blaming the encoder.
    pub fn create(
        backend: &dyn Backend,
        pipe_name: &str,
        clock: Arc<SessionClock>,
    ) -> io::Result<Self> {
        let mut pipe = backend.create_pipe(pipe_name, AUDIO_PIPE_BUFFER_BYTES)?;
        let canceller = pipe.canceller();
        let shared = Arc::new(AudioShared {
            state: Mutex::new(AudioState {
                aligner: AudioClockAligner::new(
                    TARGET_SAMPLE_RATE,
                    AudioClockAligner::DEFAULT_TOLERANCE_MS,
                ),
                ring: SampleRing::new(RING_SAMPLES),
            }),
            wake: Condvar::new(),
            running: AtomicBool::new(true),
            client_connected: AtomicBool::new(false),
            clock,
        });
        let (done_tx, writer_done) = mpsc::channel();
        let thread_shared = Arc::clone(&shared);
        let writer = std::thread::Builder::new()
            .name("audio-pipe-writer".into())
            .spawn(move || {
                if let Err(e) = thread_shared.writer_loop(pipe.as_mut()) {
                    if thread_shared.running.load(Ordering::SeqCst) {
                        log::warn(format!("audio: pipe writer stopped: {e}"));
                    }
                }
                drop(pipe);
                let _ = done_tx.send(());
            })?;
        Ok(Self {
            shared,
            canceller,
            writer: Some(writer),
            writer_done,
            source: None,
            degraded: false,
            format_description: String::new(),
        })
    }

    /// Opens the audio endpoint and starts recording. Called once the video consumer is connected so the
    /// audio timeline starts with the video one; starting it earlier would leave the first seconds of audio
    /// sitting in the ring buffer and the two streams offset by the connect delay.
    pub fn begin_capture(&mut self, backend: &dyn Backend, device: &str, exclude_app: &str) {
        if self.source.is_some() || self.degraded {
            return;
        }
        let shared = Arc::clone(&self.shared);
        let sink: AudioSink = Arc::new(move |samples: &[f32]| shared.on_audio(samples));
        match backend.open_audio(device, exclude_app, sink) {
            Ok(source) => {
                self.format_description = source.description();
                self.source = Some(source);
            }
            Err(message) => {
                self.degraded = true;
                self.format_description = format!("silence ({message})");
                log::warn(format!(
                    "audio: no desktop audio ({message}); sending silence so the stream keeps its audio track"
                ));
            }
        }
    }

    /// Number of times silence had to be inserted (the `audio_underrun` statistic).
    pub fn underruns(&self) -> u64 {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .aligner
            .underruns()
    }

    pub fn written_frames(&self) -> i64 {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .aligner
            .written_frames()
    }

    pub fn format_description(&self) -> &str {
        &self.format_description
    }

    /// True once ffmpeg opened the audio pipe.
    pub fn client_connected(&self) -> bool {
        self.shared.client_connected.load(Ordering::SeqCst)
    }

    /// True when the audio endpoint could not be opened and the stream is silence only.
    pub fn degraded(&self) -> bool {
        self.degraded
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        self.shared.running.store(false, Ordering::SeqCst);
        self.shared.wake.notify_all();
        self.source = None; // stops the capture threads
        let Some(writer) = self.writer.take() else {
            return;
        };
        let mut finished = !matches!(
            self.writer_done.recv_timeout(Duration::from_secs(1)),
            Err(RecvTimeoutError::Timeout)
        );
        if !finished {
            // Stuck in a write nobody reads: abort it.
            self.canceller.cancel();
            finished = !matches!(
                self.writer_done.recv_timeout(Duration::from_secs(1)),
                Err(RecvTimeoutError::Timeout)
            );
        }
        if finished {
            let _ = writer.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_is_a_fifo_that_drops_the_oldest_samples_on_overflow() {
        let mut ring = SampleRing::new(8);
        ring.write(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        let mut out = [0.0; 4];
        assert_eq!(ring.read(&mut out), 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        // 2 left + 8 new = 2 too many: the two oldest go.
        ring.write(&[7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0]);
        let mut all = [0.0; 16];
        assert_eq!(ring.read(&mut all), 8);
        assert_eq!(&all[..8], &[7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0]);
        assert!(ring.is_empty());
    }

    #[test]
    fn silence_never_pushes_real_audio_out_of_the_ring() {
        let mut ring = SampleRing::new(8);
        ring.write(&[1.0; 6]);
        ring.write_silence(100);
        let mut out = [9.0; 16];
        assert_eq!(ring.read(&mut out), 8);
        assert_eq!(&out[..8], &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn a_chunk_larger_than_the_ring_keeps_its_newest_part() {
        let mut ring = SampleRing::new(4);
        ring.write(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        let mut out = [0.0; 4];
        assert_eq!(ring.read(&mut out), 4);
        assert_eq!(out, [3.0, 4.0, 5.0, 6.0]);
    }
}
