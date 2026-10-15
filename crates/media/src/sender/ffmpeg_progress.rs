//! ffmpeg's `-progress` output: "key=value" lines, each block closed by "progress=continue" (or
//! "progress=end"); values can be "N/A".

use std::time::Duration;

use serde::Serialize;

use crate::text::{
    ends_with_ignore_ascii_case, find_ignore_ascii_case, parse_float, parse_integer,
};

/// One block of ffmpeg's `-progress` output.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FfmpegProgress {
    pub frame: i64,
    pub fps: f64,
    /// Instantaneous bitrate in kbit/s (-1 when ffmpeg reports N/A).
    pub bitrate_kbps: f64,
    /// -1 until ffmpeg reported one.
    pub total_size_bytes: i64,
    /// -1 until ffmpeg reported one.
    pub out_time_us: i64,
    pub dup_frames: i64,
    pub drop_frames: i64,
    /// Encoding speed relative to real time (-1 when ffmpeg reports N/A).
    pub speed: f64,
    pub ended: bool,
}

impl Default for FfmpegProgress {
    fn default() -> Self {
        FfmpegProgress {
            frame: 0,
            fps: 0.0,
            bitrate_kbps: -1.0,
            total_size_bytes: -1,
            out_time_us: -1,
            dup_frames: 0,
            drop_frames: 0,
            speed: -1.0,
            ended: false,
        }
    }
}

impl FfmpegProgress {
    /// Stream time written so far (zero while unknown).
    pub fn out_time(&self) -> Duration {
        u64::try_from(self.out_time_us)
            .map(Duration::from_micros)
            .unwrap_or(Duration::ZERO)
    }
}

/// Accumulates `-progress` lines and hands out a snapshot at the end of every block.
#[derive(Debug, Clone, Default)]
pub struct FfmpegProgressParser {
    current: FfmpegProgress,
}

impl FfmpegProgressParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// True for a -progress line ("key=value", key of [a-z0-9_]). Used because -progress shares stderr with
    /// log messages, which start with '[' or contain spaces before any '='.
    pub fn is_progress_line(line: &str) -> bool {
        match line.find('=') {
            Some(eq) if eq > 0 => line[..eq]
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_'),
            _ => false,
        }
    }

    /// Feeds one line. Returns the finished block when the line closed one.
    pub fn feed(&mut self, line: &str) -> Option<FfmpegProgress> {
        let line = line.trim();
        let (key, value) = line.split_once('=')?;
        if key.is_empty() {
            return None;
        }
        let (key, value) = (key.trim(), value.trim());
        let current = &mut self.current;

        match key {
            "frame" => current.frame = parse_integer(value).unwrap_or(current.frame),
            "fps" => current.fps = parse_float(value).unwrap_or(current.fps),
            "bitrate" => current.bitrate_kbps = parse_bitrate(value),
            "total_size" => current.total_size_bytes = parse_integer(value).unwrap_or(-1),
            // Note: ffmpeg's out_time_ms is microseconds too (a long-standing naming quirk).
            "out_time_us" | "out_time_ms" => {
                current.out_time_us = parse_integer(value).unwrap_or(-1)
            }
            "dup_frames" => current.dup_frames = parse_integer(value).unwrap_or(current.dup_frames),
            "drop_frames" => {
                current.drop_frames = parse_integer(value).unwrap_or(current.drop_frames)
            }
            "speed" => current.speed = parse_speed(value),
            "progress" => {
                // carry forward monotonic counters so a block without them is not reported as zero
                let next = FfmpegProgress {
                    frame: current.frame,
                    dup_frames: current.dup_frames,
                    drop_frames: current.drop_frames,
                    total_size_bytes: current.total_size_bytes,
                    out_time_us: current.out_time_us,
                    ..FfmpegProgress::default()
                };
                let mut done = std::mem::replace(current, next);
                done.ended = value == "end";
                return Some(done);
            }
            _ => {}
        }
        None
    }
}

/// "4520.1kbits/s" or "N/A".
fn parse_bitrate(value: &str) -> f64 {
    let mut v = value.trim();
    match find_ignore_ascii_case(v, "kbits/s") {
        Some(at) if at > 0 => v = &v[..at],
        _ if ends_with_ignore_ascii_case(v, "bits/s") => v = &v[..v.len() - 6],
        _ => {}
    }
    parse_float(v).unwrap_or(-1.0)
}

/// "1.01x" or "N/A".
fn parse_speed(value: &str) -> f64 {
    let mut v = value.trim();
    if ends_with_ignore_ascii_case(v, "x") {
        v = &v[..v.len() - 1];
    }
    parse_float(v).unwrap_or(-1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_carry_over_to_a_block_that_leaves_them_out() {
        let mut parser = FfmpegProgressParser::new();
        for line in [
            "frame=100",
            "dup_frames=2",
            "drop_frames=1",
            "total_size=5000",
            "out_time_us=1000000",
            "fps=60",
            "speed=1x",
        ] {
            assert_eq!(parser.feed(line), None);
        }
        let first = parser.feed("progress=continue").unwrap();
        assert_eq!((first.frame, first.fps, first.speed), (100, 60.0, 1.0));

        // the next block only brings a frame count: the rest is carried, the rates are "unknown" again
        parser.feed("frame=160");
        let second = parser.feed("progress=continue").unwrap();
        assert_eq!(second.frame, 160);
        assert_eq!(
            (
                second.dup_frames,
                second.drop_frames,
                second.total_size_bytes,
                second.out_time_us
            ),
            (2, 1, 5000, 1_000_000)
        );
        assert_eq!(
            (second.fps, second.bitrate_kbps, second.speed),
            (0.0, -1.0, -1.0)
        );
        assert!(!second.ended);
    }

    #[test]
    fn values_ffmpeg_pads_or_cannot_know_are_read_or_kept() {
        let mut parser = FfmpegProgressParser::new();
        parser.feed("frame=7");
        parser.feed("frame=N/A"); // keeps the last good count
        parser.feed("  bitrate=  39.8kbits/s  ");
        parser.feed("out_time_ms=2500000"); // microseconds despite the name
        parser.feed("speed= 1e+03x");
        parser.feed("total_size=N/A");
        let block = parser.feed("progress=end").unwrap();
        assert_eq!(block.frame, 7);
        assert_eq!(block.bitrate_kbps, 39.8);
        assert_eq!(block.out_time(), Duration::from_millis(2500));
        assert_eq!(block.speed, 1000.0);
        assert_eq!(block.total_size_bytes, -1);
        assert!(block.ended);
        assert_eq!(FfmpegProgress::default().out_time(), Duration::ZERO);
    }

    #[test]
    fn c_style_infinities_are_not_numbers() {
        let mut parser = FfmpegProgressParser::new();
        parser.feed("speed=infx");
        parser.feed("bitrate=nankbits/s");
        parser.feed("fps=inf");
        let block = parser.feed("progress=continue").unwrap();
        assert_eq!(
            (block.speed, block.bitrate_kbps, block.fps),
            (-1.0, -1.0, 0.0)
        );
    }
}
