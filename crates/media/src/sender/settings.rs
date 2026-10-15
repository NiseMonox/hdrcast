//! `sender.json`: what the host chose (monitor, picture, encoder, audio, group limits).
//!
//! The file is the one the C# sender wrote with DataContractJsonSerializer: camelCase names, enums as
//! numbers, a missing field = its default, unknown fields ignored.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use hdrcast_capture::{app_audio, CaptureArgs, CaptureFormat, OutputInfo};
use hdrcast_core::common::{app_paths, json};
use serde::{Deserialize, Serialize};

use super::encoder_profiles::{self, EncoderSettings, QualityPreset, VideoEncoder};
use super::ffmpeg_command::AudioCodec;
use crate::settings_file::{self, null_as_default};
use crate::Platform;

int_enum! {
    /// How the output resolution is chosen.
    pub enum ScaleMode (fallback = ScaleMode::Native) {
        Native = 0,
        H1440 = 1,
        H1080 = 2,
        H720 = 3,
    }
}

int_enum! {
    /// HDR handling. Auto follows the monitor; ForceSdr streams BT.709 even from an HDR desktop.
    pub enum HdrMode (fallback = HdrMode::Auto) {
        Auto = 0,
        ForceSdr = 1,
    }
}

/// Persisted to `<config dir>/sender.json` (%APPDATA%\HdrCast on Windows).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SenderSettings {
    // ---- capture ----
    pub output_index: i32,
    /// Remembered monitor identity so a changed output order can be corrected on the next start.
    #[serde(deserialize_with = "null_as_default")]
    pub output_device_name: String,
    #[serde(deserialize_with = "null_as_default")]
    pub output_friendly_name: String,
    pub fps: i32,
    pub scale: ScaleMode,
    pub hdr_mode: HdrMode,
    pub cursor: bool,
    pub sdr_white: f64,

    // ---- encoding ----
    pub encoder: VideoEncoder,
    pub quality: QualityPreset,
    pub bitrate_mbps: i32,
    pub gop_seconds: f64,
    pub buffer_seconds: f64,
    pub b_frames: bool,
    pub lookahead: bool,

    // ---- audio ----
    pub audio: bool,
    #[serde(deserialize_with = "null_as_default")]
    pub audio_device: String,
    pub audio_codec: AudioCodec,
    pub audio_bitrate_kbps: i32,
    /// Program whose audio is left out of the stream, e.g. the voice chat ("" = none).
    #[serde(deserialize_with = "null_as_default")]
    pub audio_exclude_app: String,

    // ---- group ----
    // The SRT settings of each viewer (latency, overhead, passphrase) come from the group key and that viewer's
    // join request; the old relay/direct fields (mode, host, port, …) are simply ignored in existing files.
    /// How many friends may watch at once (each one costs a full stream of upload).
    pub max_viewers: i32,
    /// This line's upload in Mbps, for the "upload needed" warning (0 = unknown).
    pub upload_mbps: i32,

    // ---- tools ----
    #[serde(deserialize_with = "null_as_default")]
    pub ffmpeg_path: String,
}

impl Default for SenderSettings {
    /// [`defaults_for`](Self::defaults_for) the platform this was compiled for.
    fn default() -> Self {
        Self::defaults_for(Platform::CURRENT)
    }
}

impl SenderSettings {
    pub const FILE_NAME: &'static str = "sender.json";

    /// Fresh settings: the encoder is the first one `platform` offers.
    pub fn defaults_for(platform: Platform) -> Self {
        SenderSettings {
            output_index: 0,
            output_device_name: String::new(),
            output_friendly_name: String::new(),
            fps: 60,
            scale: ScaleMode::Native,
            hdr_mode: HdrMode::Auto,
            cursor: true,
            sdr_white: f64::from(CaptureArgs::DEFAULT_SDR_WHITE),

            encoder: encoder_profiles::default_encoder(platform),
            quality: QualityPreset::Balanced,
            bitrate_mbps: 40,
            gop_seconds: 2.0,
            buffer_seconds: 2.0,
            b_frames: true,
            lookahead: true,

            audio: true,
            audio_device: "default".to_string(),
            audio_codec: AudioCodec::Aac,
            audio_bitrate_kbps: 192,
            audio_exclude_app: String::new(),

            max_viewers: 4,
            upload_mbps: 0,

            ffmpeg_path: String::new(),
        }
    }

    /// Clamps everything that came from a hand-edited file, for the platform this was compiled for.
    pub fn normalize(&mut self) -> &mut Self {
        self.normalize_for(Platform::CURRENT)
    }

    /// [`normalize`](Self::normalize), consuming: `SenderSettings { .. }.normalized()`.
    pub fn normalized(mut self) -> Self {
        self.normalize();
        self
    }

    /// Clamps everything that came from a hand-edited file. An encoder `platform` does not have (a file
    /// carried over from another OS, or one of the AV1 encoders of old files) becomes its default.
    pub fn normalize_for(&mut self, platform: Platform) -> &mut Self {
        self.output_index = self.output_index.max(0);
        if !(1..=480).contains(&self.fps) {
            self.fps = 60;
        }
        if !(1.0..=10000.0).contains(&self.sdr_white) {
            self.sdr_white = f64::from(CaptureArgs::DEFAULT_SDR_WHITE);
        }
        self.bitrate_mbps = self.bitrate_mbps.clamp(1, 500);
        self.gop_seconds = self.gop_seconds.clamp(0.25, 10.0);
        self.buffer_seconds = self.buffer_seconds.clamp(0.5, 10.0);
        self.audio_bitrate_kbps = self.audio_bitrate_kbps.clamp(32, 512);
        if self.audio_device.trim().is_empty() {
            self.audio_device = "default".to_string();
        }
        self.audio_exclude_app = app_audio::normalize_exe_name(&self.audio_exclude_app);
        if !encoder_profiles::is_offered(self.encoder, platform) {
            self.encoder = encoder_profiles::default_encoder(platform);
        }
        self.max_viewers = self.max_viewers.clamp(1, 16);
        self.upload_mbps = self.upload_mbps.clamp(0, 100_000);
        self
    }

    /// Output height for the selected scale mode (0 = native).
    pub fn scale_height(&self) -> i32 {
        match self.scale {
            ScaleMode::H1440 => 1440,
            ScaleMode::H1080 => 1080,
            ScaleMode::H720 => 720,
            ScaleMode::Native => 0,
        }
    }

    /// Output size for a source of `src_width` x `src_height`: keeps the aspect ratio, rounds to even numbers
    /// and never upscales.
    pub fn resolve_size(&self, src_width: i32, src_height: i32) -> (u32, u32) {
        let target = self.scale_height();
        if target <= 0 || src_height <= 0 || target >= src_height {
            return (even(src_width), even(src_height));
        }
        let scale = f64::from(target) / f64::from(src_height);
        (
            even((f64::from(src_width) * scale).round_ties_even() as i32),
            even(target),
        )
    }

    /// Resolved capture pixel format for a monitor that is (or is not) in HDR mode.
    pub fn resolve_format(&self, monitor_is_hdr: bool) -> CaptureFormat {
        if self.hdr_mode == HdrMode::ForceSdr || !monitor_is_hdr {
            CaptureFormat::Nv12
        } else {
            CaptureFormat::P010
        }
    }

    pub fn bitrate_bps(&self) -> u64 {
        u64::from(non_negative(self.bitrate_mbps)) * 1_000_000
    }

    /// Problems that prevent streaming (empty when valid), for the platform this was compiled for.
    pub fn validate(&self) -> Vec<String> {
        self.validate_for(Platform::CURRENT)
    }

    pub fn validate_for(&self, platform: Platform) -> Vec<String> {
        let mut problems = Vec::new();
        if !(1..=500).contains(&self.bitrate_mbps) {
            problems.push("Bitrate must be 1..500 Mbps.".to_string());
        }
        if !(1..=480).contains(&self.fps) {
            problems.push("Frame rate must be 1..480.".to_string());
        }
        if !encoder_profiles::is_offered(self.encoder, platform) {
            problems.push("Pick an encoder.".to_string());
        }
        problems
    }

    /// What the encoder profile needs; `mastering_max_nits` is the monitor's peak (0 = unknown).
    pub fn to_encoder_settings(&self, hdr: bool, mastering_max_nits: u32) -> EncoderSettings {
        EncoderSettings {
            encoder: self.encoder,
            preset: self.quality,
            hdr,
            bitrate_kbps: non_negative(self.bitrate_mbps).saturating_mul(1000),
            fps: non_negative(self.fps),
            gop_seconds: self.gop_seconds,
            buffer_seconds: self.buffer_seconds,
            b_frames: self.b_frames,
            lookahead: self.lookahead,
            mastering_max_nits,
        }
    }

    /// Capture engine arguments for the selected monitor and size.
    #[allow(clippy::too_many_arguments)] // the C# signature; every argument is a different thing
    pub fn to_capture_args(
        &self,
        output: &OutputInfo,
        width: u32,
        height: u32,
        format: CaptureFormat,
        video_pipe: &str,
        audio_pipe: &str,
        parent_pid: u32,
    ) -> CaptureArgs {
        CaptureArgs {
            output_index: i32::try_from(output.index).unwrap_or(i32::MAX),
            fps: non_negative(self.fps),
            format,
            width,
            height,
            sdr_white: self.sdr_white as f32,
            cursor: self.cursor,
            video_pipe: video_pipe.to_string(),
            audio_pipe: audio_pipe.to_string(),
            audio: self.audio,
            audio_device: self.audio_device.clone(),
            audio_exclude: app_audio::normalize_exe_name(&self.audio_exclude_app),
            parent_pid,
            ..CaptureArgs::default()
        }
    }

    pub fn default_path() -> PathBuf {
        app_paths::settings_file(Self::FILE_NAME)
    }

    /// The settings in the text of a sender.json, exactly as written (not normalised); `None` when the text is
    /// not a JSON object or a member has the wrong type.
    pub fn from_json(text: &str) -> Option<SenderSettings> {
        settings_file::from_json(text)
    }

    /// Never fails: a missing, unreadable or corrupt file gives the defaults, and whatever was read is
    /// normalised.
    pub fn load(path: &Path) -> SenderSettings {
        settings_file::load::<SenderSettings>(path)
            .unwrap_or_default()
            .normalized()
    }

    /// Writes the file atomically (a reader sees the old or the new file, never half of one).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        json::save_file(path, self)
    }
}

impl fmt::Display for SenderSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} Mbps {}fps, up to {} viewers",
            encoder_profiles::info(self.encoder).ffmpeg_name,
            self.bitrate_mbps,
            self.fps,
            self.max_viewers
        )
    }
}

fn even(v: i32) -> u32 {
    if v < 2 {
        2
    } else {
        (v & !1) as u32
    }
}

fn non_negative(v: i32) -> u32 {
    u32::try_from(v).unwrap_or(0)
}
