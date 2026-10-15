//! Keeps the audio stream locked to the video clock (design.md section 2.4).

/// Result of one alignment step: how many frames to drop from the front of the chunk and how many silent
/// frames to append.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioAlignment {
    pub drop_frames: usize,
    pub pad_frames: usize,
}

impl AudioAlignment {
    pub fn is_identity(&self) -> bool {
        self.drop_frames == 0 && self.pad_frames == 0
    }
}

/// The caller measures elapsed time on the same clock as the video pacer and reports every captured chunk;
/// the aligner compares "frames that should have been written by now" (elapsed x sample rate) with the frames
/// actually written: if we are behind by more than the tolerance it asks for silence padding, if we are ahead
/// it asks to drop the oldest frames. Pure logic; unit-tested.
#[derive(Debug, Clone)]
pub struct AudioClockAligner {
    sample_rate: u32,
    tolerance_frames: i64,
    written_frames: i64,
    total_padded_frames: u64,
    total_dropped_frames: u64,
    underruns: u64,
    overruns: u64,
}

impl AudioClockAligner {
    /// Must stay well above how bursty loopback capture is (it is polled, so audio arrives tens of ms at a
    /// time, a little late). With 20 ms the pipe writer padded silence between two bursts and the next burst
    /// lost its head to the "ahead" check, dozens of times a second: viewers heard a low buzz instead of the
    /// desktop audio. The tolerance only bounds slow clock drift and real stalls, so a larger value costs
    /// nothing audible.
    pub const DEFAULT_TOLERANCE_MS: u32 = 150;

    /// # Panics
    /// When `sample_rate` is 0.
    pub fn new(sample_rate: u32, tolerance_ms: u32) -> Self {
        assert!(sample_rate > 0, "sample_rate must be positive");
        Self {
            sample_rate,
            tolerance_frames: i64::from(sample_rate) * i64::from(tolerance_ms) / 1000,
            written_frames: 0,
            total_padded_frames: 0,
            total_dropped_frames: 0,
            underruns: 0,
            overruns: 0,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn tolerance_frames(&self) -> i64 {
        self.tolerance_frames
    }

    /// Frames (per channel) written so far, after padding / dropping.
    pub fn written_frames(&self) -> i64 {
        self.written_frames
    }

    pub fn total_padded_frames(&self) -> u64 {
        self.total_padded_frames
    }

    pub fn total_dropped_frames(&self) -> u64 {
        self.total_dropped_frames
    }

    /// Number of times silence had to be inserted (reported as "audio_underrun").
    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    pub fn overruns(&self) -> u64 {
        self.overruns
    }

    /// Frames that should have been written after `elapsed_ticks` on a clock of `ticks_per_second`.
    pub fn expected_frames(&self, elapsed_ticks: i64, ticks_per_second: i64) -> i64 {
        if elapsed_ticks <= 0 {
            return 0;
        }
        (i128::from(elapsed_ticks) * i128::from(self.sample_rate) / i128::from(ticks_per_second))
            as i64
    }

    /// Decides what to do with a chunk of `incoming_frames` frames that arrived when `expected_frames` frames
    /// should have been written in total (see [`expected_frames`](Self::expected_frames)). Updates the written
    /// counter as if the caller applied the result (drop first, then chunk, then pad).
    pub fn align(&mut self, expected_frames: i64, incoming_frames: usize) -> AudioAlignment {
        let incoming = incoming_frames as i64;
        let mut r = AudioAlignment::default();
        let after_chunk = self.written_frames + incoming;
        let delta = after_chunk - expected_frames; // > 0: ahead of the clock, < 0: behind
        if delta > self.tolerance_frames {
            // Ahead: drop the oldest frames of this chunk (never more than the chunk itself).
            let drop = delta.min(incoming);
            r.drop_frames = drop as usize;
            self.total_dropped_frames += drop as u64;
            if drop > 0 {
                self.overruns += 1;
            }
        } else if -delta > self.tolerance_frames {
            // Behind: append silence to catch up exactly.
            let pad = (-delta).min(i64::from(i32::MAX));
            r.pad_frames = pad as usize;
            self.total_padded_frames += pad as u64;
            self.underruns += 1;
        }
        self.written_frames = after_chunk - r.drop_frames as i64 + r.pad_frames as i64;
        r
    }

    /// Called periodically when no audio arrived (e.g. the loopback endpoint stopped delivering):
    /// returns the number of silent frames to write now, or 0.
    pub fn pad_if_behind(&mut self, expected_frames: i64) -> usize {
        self.align(expected_frames, 0).pad_frames
    }

    pub fn reset(&mut self) {
        self.written_frames = 0;
    }
}

impl Default for AudioClockAligner {
    /// 48 kHz with the default tolerance.
    fn default() -> Self {
        Self::new(48000, Self::DEFAULT_TOLERANCE_MS)
    }
}
