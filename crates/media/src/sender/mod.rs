//! The host's side: which encoders exist, what ffmpeg is told, what it reports back, and `sender.json`.
//! A port of the C# `shared/Sender`.

pub mod encoder_probe;
pub mod encoder_profiles;
pub mod ffmpeg_command;
pub mod ffmpeg_progress;
pub mod settings;

pub use encoder_probe::{EncoderProbeResult, FfmpegRun};
pub use encoder_profiles::{EncoderInfo, EncoderSettings, QualityPreset, VideoEncoder};
pub use ffmpeg_command::{AudioCodec, FfmpegPlan, FfmpegPlanError};
pub use ffmpeg_progress::{FfmpegProgress, FfmpegProgressParser};
pub use settings::{HdrMode, ScaleMode, SenderSettings};
