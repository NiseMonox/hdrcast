//! The once-per-second statistics line the capture engine prints to stdout (design.md section 2.2), e.g.
//! `{"t":12.0,"captured":700,"duplicated":20,"dropped":0,"fps":59.9,"gpu_ms":0.8,"audio_underrun":0,"pipe_queue_ms":16.7}`

use serde::Serialize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct CaptureStats {
    /// Seconds since the session clock started.
    pub t: f64,
    pub captured: u64,
    pub duplicated: u64,
    pub dropped: u64,
    pub fps: f64,
    pub gpu_ms: f64,
    pub audio_underrun: u64,
    pub pipe_queue_ms: f64,
}

fn round_to(value: f64, decimals: i32) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    let scale = 10f64.powi(decimals);
    (value * scale).round() / scale
}

impl CaptureStats {
    /// One line of JSON (no newline) with the precision the line always had: one decimal for the times and
    /// the frame rate, two for the GPU time.
    pub fn to_json_line(&self) -> String {
        let rounded = CaptureStats {
            t: round_to(self.t, 1),
            fps: round_to(self.fps, 1),
            gpu_ms: round_to(self.gpu_ms, 2),
            pipe_queue_ms: round_to(self.pipe_queue_ms, 1),
            ..*self
        };
        serde_json::to_string(&rounded).expect("a flat struct of numbers always serialises")
    }

    /// Parses a stats line; returns `None` for anything that is not a flat JSON object with at least one
    /// number in it. Unknown keys are ignored, missing ones stay 0.
    pub fn try_parse(line: &str) -> Option<CaptureStats> {
        let line = line.trim();
        if !(line.starts_with('{') && line.ends_with('}')) {
            return None;
        }
        let object: serde_json::Map<String, serde_json::Value> = serde_json::from_str(line).ok()?;
        let mut s = CaptureStats::default();
        let mut any = false;
        for (key, value) in &object {
            let Some(d) = value.as_f64() else { continue };
            any = true;
            match key.as_str() {
                "t" => s.t = d,
                "captured" => s.captured = d as u64,
                "duplicated" => s.duplicated = d as u64,
                "dropped" => s.dropped = d as u64,
                "fps" => s.fps = d,
                "gpu_ms" => s.gpu_ms = d,
                "audio_underrun" => s.audio_underrun = d as u64,
                "pipe_queue_ms" => s.pipe_queue_ms = d,
                _ => {}
            }
        }
        any.then_some(s)
    }
}
