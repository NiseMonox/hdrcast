//! Builds the argument vector of the encoding ffmpeg (design.md §2.5). Pure logic.

use std::path::Path;

use hdrcast_capture::{CaptureArgs, CaptureFormat, FrameLayout};
use hdrcast_core::common::arg_quote;

use super::encoder_profiles::{self, EncoderSettings};

int_enum! {
    /// Audio codec for the MPEG-TS stream. AAC is the safe default; Opus is smaller but less universally played.
    pub enum AudioCodec (fallback = AudioCodec::Aac) {
        None = 0,
        Aac = 1,
        Opus = 2,
    }
}

/// Everything the ffmpeg child process needs (design.md §2.5).
#[derive(Debug, Clone, PartialEq)]
pub struct FfmpegPlan {
    /// Name of the capture engine's video pipe (see [`pipe_path`]).
    pub video_pipe_name: String,
    /// Name of the audio pipe; `None` (or [`AudioCodec::None`]) = no audio input.
    pub audio_pipe_name: Option<String>,
    pub pixel_format: CaptureFormat,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub encoder: EncoderSettings,
    pub audio: AudioCodec,
    pub audio_bitrate_kbps: u32,
    /// Where the MPEG-TS goes: stdout, into the P2P fan-out (design.md §8.8).
    pub output_url: String,
    /// Emit -progress so the UI can read encoder statistics.
    pub progress: bool,
    /// Where -progress writes: stderr, because stdout carries the stream (lines are told apart by shape).
    pub progress_url: String,
    pub log_level: String,
}

impl FfmpegPlan {
    /// This plan for the frames the capture engine announced (its layout line, see [`FrameLayout`]): their
    /// pixel format and size, and HDR encoding exactly when the frames are p010. The engine has the last word
    /// because with `--format auto` it chooses, and macOS falls back to SDR when the system refuses HDR capture.
    pub fn for_frames(&self, layout: FrameLayout) -> FfmpegPlan {
        let mut plan = self.clone();
        plan.pixel_format = layout.format;
        plan.width = layout.width;
        plan.height = layout.height;
        plan.encoder.hdr = layout.format == CaptureFormat::P010;
        plan
    }
}

impl Default for FfmpegPlan {
    fn default() -> Self {
        FfmpegPlan {
            video_pipe_name: CaptureArgs::DEFAULT_VIDEO_PIPE.to_string(),
            audio_pipe_name: None,
            pixel_format: CaptureFormat::P010,
            width: 0,
            height: 0,
            fps: 60,
            encoder: EncoderSettings::default(),
            audio: AudioCodec::Aac,
            audio_bitrate_kbps: 192,
            output_url: "pipe:1".to_string(),
            progress: true,
            progress_url: "pipe:2".to_string(),
            log_level: "warning".to_string(),
        }
    }
}

/// Why [`build`] refused a plan; the text is what the UI shows.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FfmpegPlanError {
    #[error("frame size is required")]
    FrameSizeRequired,
    #[error("frame size must be even")]
    FrameSizeOdd,
    #[error("fps must be positive")]
    Fps,
    #[error("output url is required")]
    OutputUrl,
    #[error("pixel format must be resolved")]
    PixelFormatUnresolved,
    /// Carries the encoder's display name.
    #[error("{0} has no 10-bit mode")]
    No10Bit(&'static str),
    #[error("HDR encoding needs p010 input (and SDR needs nv12)")]
    HdrFormatMismatch,
}

/// The path both the capture engine (server) and ffmpeg (client) use for the pipe `name`: `\\.\pipe\<name>`
/// on Windows, whatever the capture engine serves elsewhere.
pub fn pipe_path(name: &str) -> String {
    hdrcast_capture::pipe_path(name)
}

pub fn build(p: &FfmpegPlan) -> Result<Vec<String>, FfmpegPlanError> {
    if p.width == 0 || p.height == 0 {
        return Err(FfmpegPlanError::FrameSizeRequired);
    }
    if p.width & 1 != 0 || p.height & 1 != 0 {
        return Err(FfmpegPlanError::FrameSizeOdd);
    }
    if p.fps == 0 {
        return Err(FfmpegPlanError::Fps);
    }
    if p.output_url.is_empty() {
        return Err(FfmpegPlanError::OutputUrl);
    }
    if p.pixel_format == CaptureFormat::Auto {
        return Err(FfmpegPlanError::PixelFormatUnresolved);
    }

    let info = encoder_profiles::info(p.encoder.encoder);
    if p.encoder.hdr && !info.supports_10bit {
        return Err(FfmpegPlanError::No10Bit(info.display_name));
    }
    if p.encoder.hdr != (p.pixel_format == CaptureFormat::P010) {
        return Err(FfmpegPlanError::HdrFormatMismatch);
    }

    let mut a: Vec<String> = Vec::with_capacity(96);
    push(
        &mut a,
        &["-hide_banner", "-loglevel", &p.log_level, "-nostdin"],
    );
    if p.progress {
        push(
            &mut a,
            &["-stats_period", "1", "-progress", &p.progress_url],
        );
    }

    // Video input: raw frames from the capture engine, paced by the engine itself.
    push(&mut a, &["-thread_queue_size", "1024"]);
    push(&mut a, &["-f", "rawvideo"]);
    push(
        &mut a,
        &[
            "-pix_fmt",
            if p.pixel_format == CaptureFormat::P010 {
                "p010le"
            } else {
                "nv12"
            },
        ],
    );
    push(
        &mut a,
        &["-video_size", &format!("{}x{}", p.width, p.height)],
    );
    push(&mut a, &["-framerate", &p.fps.to_string()]);
    // Tag the raw frames themselves. ffmpeg 7.1+ negotiates colour spaces in the filter graph and gives the
    // encoder the frames' colour properties, so tags on the output alone went wrong twice (v0.2.1, ffmpeg 9):
    // untagged input counted as BT.601 and was re-matrixed to BT.2020 (saturated colours shifted by up to
    // 23 code values), and hardware encoders wrote transfer / primaries "unknown", so every player showed
    // the PQ signal as plain SDR: wrong colours on HDR and SDR screens alike.
    a.extend(encoder_profiles::color_tags(p.encoder.hdr));
    push(&mut a, &["-i", &pipe_path(&p.video_pipe_name)]);

    let audio_pipe = match p.audio {
        AudioCodec::None => None,
        AudioCodec::Aac | AudioCodec::Opus => {
            p.audio_pipe_name.as_deref().filter(|name| !name.is_empty())
        }
    };
    if let Some(name) = audio_pipe {
        push(&mut a, &["-thread_queue_size", "1024"]);
        push(&mut a, &["-f", "f32le"]);
        push(&mut a, &["-ar", "48000"]);
        push(&mut a, &["-ac", "2"]);
        push(&mut a, &["-i", &pipe_path(name)]);
    }

    a.extend(encoder_profiles::build_video_args(&p.encoder));
    // The engine writes exactly `fps` frames per second; keep ffmpeg from resampling timestamps.
    push(&mut a, &["-fps_mode", "cfr"]);

    if audio_pipe.is_some() {
        push(
            &mut a,
            &[
                "-c:a",
                if p.audio == AudioCodec::Opus {
                    "libopus"
                } else {
                    "aac"
                },
            ],
        );
        push(&mut a, &["-b:a", &format!("{}k", p.audio_bitrate_kbps)]);
        push(&mut a, &["-ac", "2"]);
    }

    push(&mut a, &["-muxdelay", "0"]);
    push(&mut a, &["-muxpreload", "0"]);
    // +resend_headers: a viewer who joins mid-stream gets PAT/PMT again right away
    push(&mut a, &["-f", "mpegts"]);
    push(&mut a, &["-mpegts_flags", "+resend_headers"]);
    a.push(p.output_url.clone());
    Ok(a)
}

fn push(args: &mut Vec<String>, more: &[&str]) {
    args.extend(more.iter().map(|s| s.to_string()));
}

/// The full command line as the user would type it (for "copy command" and the log).
pub fn command_line(ffmpeg_exe: &Path, p: &FfmpegPlan) -> Result<String, FfmpegPlanError> {
    Ok(arg_quote::command_line(
        &ffmpeg_exe.to_string_lossy(),
        build(p)?,
    ))
}

/// Size of the synthetic picture [`build_probe`] encodes.
pub const PROBE_SIZE: (u32, u32) = (640, 480);

/// A one-second encode of a synthetic source, used to check that an encoder and its options really work on
/// this machine before streaming (design.md §2.5 "encoder probe"). Writes to null.
pub fn build_probe(encoder: &EncoderSettings) -> Vec<String> {
    build_probe_sized(encoder, PROBE_SIZE.0, PROBE_SIZE.1)
}

/// [`build_probe`] with another picture size.
pub fn build_probe_sized(encoder: &EncoderSettings, width: u32, height: u32) -> Vec<String> {
    let e = EncoderSettings {
        bitrate_kbps: 2000,
        fps: 30,
        ..encoder.clone()
    };
    // One second of source, not a few frames: SVT-AV1 fills a pipeline before it emits anything and ffmpeg
    // fails with "no packets" on a shorter clip. The pixel format comes from the encoder profile.
    let mut a: Vec<String> = Vec::with_capacity(64);
    push(&mut a, &["-hide_banner", "-loglevel", "error", "-nostdin"]);
    push(&mut a, &["-f", "lavfi"]);
    push(
        &mut a,
        &[
            "-i",
            &format!("testsrc2=size={width}x{height}:rate=30:duration=1"),
        ],
    );
    a.extend(encoder_profiles::build_video_args(&e));
    push(&mut a, &["-f", "null"]);
    a.push("-".to_string());
    a
}
