//! Constant-frame-rate scheduler (design.md section 2.2).

/// Deadlines are computed from the start tick with integer arithmetic
/// (`deadline_n = start + n * ticks_per_second / fps`) so there is no accumulated drift.
/// Pure logic; the caller supplies the clock (see [`crate::clock::now_ticks`]).
#[derive(Debug, Clone)]
pub struct FramePacer {
    ticks_per_second: i64,
    fps: u32,
    start: i64,
    /// index of the next frame whose deadline has not been consumed yet
    next_frame: i64,
}

impl FramePacer {
    /// # Panics
    /// When `fps` is 0 or `ticks_per_second` is not positive.
    pub fn new(fps: u32, ticks_per_second: i64) -> Self {
        assert!(fps > 0, "fps must be positive");
        assert!(ticks_per_second > 0, "ticks_per_second must be positive");
        Self {
            ticks_per_second,
            fps,
            start: 0,
            next_frame: 0,
        }
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }

    pub fn ticks_per_second(&self) -> i64 {
        self.ticks_per_second
    }

    /// Nominal frame interval in ticks (rounded).
    pub fn frame_interval_ticks(&self) -> i64 {
        (self.ticks_per_second + i64::from(self.fps) / 2) / i64::from(self.fps)
    }

    pub fn frame_interval_ms(&self) -> f64 {
        1000.0 / f64::from(self.fps)
    }

    /// Index of the next frame to be emitted (0-based). Equals the number of frames consumed so far.
    pub fn next_frame_index(&self) -> i64 {
        self.next_frame
    }

    /// Starts the timeline; frame 0 is due immediately at `now_ticks`.
    pub fn start(&mut self, now_ticks: i64) {
        self.start = now_ticks;
        self.next_frame = 0;
    }

    /// Absolute tick at which frame `frame_index` is due.
    ///
    /// # Panics
    /// When `frame_index` is negative.
    pub fn deadline_ticks(&self, frame_index: i64) -> i64 {
        assert!(frame_index >= 0, "frame_index must not be negative");
        self.start + mul_div(frame_index, self.ticks_per_second, i64::from(self.fps), 0)
    }

    /// Deadline of the next frame that has not been consumed.
    pub fn next_deadline_ticks(&self) -> i64 {
        self.deadline_ticks(self.next_frame)
    }

    /// Number of frames whose deadline is at or before `now_ticks` and that have not been consumed.
    /// 0 means "not yet time"; 1 is the normal case; more than 1 means the loop fell behind.
    /// Does not consume anything.
    pub fn frames_due(&self, now_ticks: i64) -> i64 {
        let elapsed = now_ticks - self.start;
        if elapsed < 0 {
            return 0;
        }
        // Largest n with deadline_ticks(n) <= now. deadline_ticks truncates (n*tps/fps), so the inverse must
        // use the same rounding: floor(n*tps/fps) <= elapsed  <=>  n <= ((elapsed+1)*fps - 1)/tps.
        // Deriving it from elapsed*fps/tps instead would miss the frame that is due on its exact deadline tick.
        let last_due = mul_div(elapsed + 1, i64::from(self.fps), self.ticks_per_second, -1);
        (last_due + 1 - self.next_frame).max(0)
    }

    /// Marks `count` frames as emitted.
    ///
    /// # Panics
    /// When `count` is negative.
    pub fn consume(&mut self, count: i64) {
        assert!(count >= 0, "count must not be negative");
        self.next_frame += count;
    }

    /// Skips frames so that the next deadline is in the future (used after a long stall when the caller
    /// decided to drop rather than catch up). Returns the number of frames skipped.
    pub fn skip_missed(&mut self, now_ticks: i64) -> i64 {
        let due = self.frames_due(now_ticks);
        if due <= 1 {
            return 0;
        }
        let skipped = due - 1;
        self.next_frame += skipped;
        skipped
    }

    /// Ticks to wait from `now_ticks` until the next deadline (0 if already due).
    pub fn ticks_until_next(&self, now_ticks: i64) -> i64 {
        (self.next_deadline_ticks() - now_ticks).max(0)
    }
}

/// `(a * b + bias) / c` with the multiplication kept exact (nanosecond ticks times a frame index or a frame
/// rate leave the range of an i64 within minutes).
fn mul_div(a: i64, b: i64, c: i64, bias: i64) -> i64 {
    ((i128::from(a) * i128::from(b) + i128::from(bias)) / i128::from(c)) as i64
}
