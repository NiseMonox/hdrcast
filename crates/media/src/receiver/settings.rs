//! `receiver.json`: what the viewer chose (latency preset, display mode, decoder, buffer).
//!
//! The file is the one the C# receiver wrote with DataContractJsonSerializer: camelCase names, enums as
//! numbers, a missing field = its default, unknown fields ignored.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use hdrcast_core::common::{app_paths, json};
use hdrcast_core::{SrtRole, SrtTopology, SrtUrlOptions};
use serde::{Deserialize, Serialize};

use crate::settings_file::{self, null_as_default};
use crate::Platform;

int_enum! {
    /// What mpv is told about the screen.
    pub enum DisplayMode (fallback = DisplayMode::Auto) {
        /// `--target-colorspace-hint=auto` (passthrough only when the system's HDR is on).
        Auto = 0,
        /// `--target-colorspace-hint=yes` (PQ swapchain; on Windows its HDR must be on).
        HdrPassthrough = 1,
        /// `--target-colorspace-hint=no` plus `--tone-mapping=<algorithm>`.
        SdrToneMap = 2,
    }
}

int_enum! {
    /// Which decoder mpv uses. The names (and the numbers in receiver.json) are the Windows ones; on macOS the
    /// two D3D11 values select VideoToolbox, the hardware decoder there. See [`mpv_value`](Self::mpv_value).
    pub enum HwDecMode (fallback = HwDecMode::D3d11va) {
        AutoSafe = 0,
        /// The platform's hardware decoder, frames stay on the GPU.
        D3d11va = 1,
        /// The platform's hardware decoder, frames copied back to system memory.
        D3d11vaCopy = 2,
        /// Software decoding.
        No = 3,
    }
}

impl HwDecMode {
    /// The value of mpv's `--hwdec` on `platform`.
    pub fn mpv_value(self, platform: Platform) -> &'static str {
        match (self, platform) {
            (HwDecMode::AutoSafe, _) => "auto-safe",
            (HwDecMode::No, _) => "no",
            (HwDecMode::D3d11va, Platform::Windows) => "d3d11va",
            (HwDecMode::D3d11vaCopy, Platform::Windows) => "d3d11va-copy",
            (HwDecMode::D3d11va, Platform::MacOs) => "videotoolbox",
            (HwDecMode::D3d11vaCopy, Platform::MacOs) => "videotoolbox-copy",
            // No single API to name elsewhere (VA-API, NVDEC, Vulkan…): let mpv pick.
            (HwDecMode::D3d11va, Platform::Other) => "auto-safe",
            (HwDecMode::D3d11vaCopy, Platform::Other) => "auto-copy",
        }
    }

    /// What the UI calls this choice on `platform` (the C# receiver's wording, with the platform's decoder).
    pub fn display_name(self, platform: Platform) -> &'static str {
        match self {
            HwDecMode::No => "no (software)",
            other => other.mpv_value(platform),
        }
    }
}

int_enum! {
    pub enum ReceiverFrontend (fallback = ReceiverFrontend::MpvDirect) {
        /// mpv opens srt:// itself (needs a libsrt-enabled mpv build: shinchiro / zhongfly).
        MpvDirect = 0,
        /// `ffmpeg -i srt://127.0.0.1:… -c copy -f mpegts pipe:1 | mpv -` (when mpv lacks libsrt; the default on
        /// macOS, where Homebrew's mpv does).
        FfmpegPipe = 1,
    }
}

/// A named SRT latency: where the viewer is relative to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LatencyPreset {
    pub name: &'static str,
    pub latency_ms: i32,
}

impl fmt::Display for LatencyPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({} ms)", self.name, self.latency_ms)
    }
}

/// Persisted to `<config dir>/receiver.json` (%APPDATA%\HdrCast on Windows).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ReceiverSettings {
    // The old relay/direct fields (mode, host, port, path, user, password, passphrase) are ignored in existing files.
    /// SRT latency this viewer asks the host for (its region preset).
    pub latency_ms: i32,
    pub expected_bitrate_mbps: i32,
    pub display_mode: DisplayMode,
    #[serde(deserialize_with = "null_as_default")]
    pub tone_mapping: String,
    pub hw_dec: HwDecMode,
    pub buffer_seconds: f64,
    pub fullscreen: bool,
    pub fullscreen_screen: i32,
    pub frontend: ReceiverFrontend,
    #[serde(deserialize_with = "null_as_default")]
    pub mpv_path: String,
    #[serde(deserialize_with = "null_as_default")]
    pub ffmpeg_path: String,
    pub auto_catch_up: bool,
    /// 0 = let mpv / the system decide; otherwise `--target-peak=<nits>` (bogus display peaks).
    pub target_peak_nits: i32,
    /// The host (peer id) watched last, preselected in the live list.
    #[serde(deserialize_with = "null_as_default")]
    pub last_host: String,
}

impl Default for ReceiverSettings {
    /// [`defaults_for`](Self::defaults_for) the platform this was compiled for.
    fn default() -> Self {
        Self::defaults_for(Platform::CURRENT)
    }
}

impl ReceiverSettings {
    pub const FILE_NAME: &'static str = "receiver.json";
    pub const DEFAULT_TONE_MAPPING: &'static str = "bt.2390";

    /// Fresh settings. On macOS mpv is fed through the ffmpeg pipe: Homebrew's mpv has no libsrt (its ffmpeg-full
    /// has).
    pub fn defaults_for(platform: Platform) -> Self {
        ReceiverSettings {
            latency_ms: 1500,
            expected_bitrate_mbps: 100,
            display_mode: DisplayMode::Auto,
            tone_mapping: Self::DEFAULT_TONE_MAPPING.to_string(),
            hw_dec: HwDecMode::D3d11va,
            buffer_seconds: 1.5,
            fullscreen: false,
            fullscreen_screen: 0,
            frontend: match platform {
                Platform::MacOs => ReceiverFrontend::FfmpegPipe,
                Platform::Windows | Platform::Other => ReceiverFrontend::MpvDirect,
            },
            mpv_path: String::new(),
            ffmpeg_path: String::new(),
            auto_catch_up: true,
            target_peak_nits: 0,
            last_host: String::new(),
        }
    }

    /// "Fast" is for watching while controlling the host's PC (remote control); the regions are sized for the
    /// retransmissions their distance from the host needs (design.md §5).
    pub const LATENCY_PRESETS: [LatencyPreset; 4] = [
        LatencyPreset {
            name: "Fast",
            latency_ms: 300,
        },
        LatencyPreset {
            name: "Japan",
            latency_ms: 1500,
        },
        LatencyPreset {
            name: "Australia",
            latency_ms: 2000,
        },
        LatencyPreset {
            name: "China",
            latency_ms: 3500,
        },
    ];

    pub const TONE_MAPPING_ALGORITHMS: [&'static str; 10] = [
        "bt.2390",
        "auto",
        "spline",
        "bt.2446a",
        "st2094-40",
        "st2094-10",
        "hable",
        "mobius",
        "reinhard",
        "clip",
    ];

    /// Repairs out-of-range values after loading a hand-edited file.
    pub fn normalize(&mut self) -> &mut Self {
        self.last_host = self.last_host.trim().to_string();
        self.tone_mapping = match self.tone_mapping.trim() {
            "" => Self::DEFAULT_TONE_MAPPING.to_string(),
            algorithm => algorithm.to_string(),
        };
        if self.latency_ms < 20 {
            self.latency_ms = 1500;
        }
        self.latency_ms = self.latency_ms.min(30000);
        if self.expected_bitrate_mbps < 1 {
            self.expected_bitrate_mbps = 100;
        }
        self.expected_bitrate_mbps = self.expected_bitrate_mbps.min(1000);
        if self.buffer_seconds.is_nan() || self.buffer_seconds < 0.1 {
            self.buffer_seconds = 1.5;
        }
        self.buffer_seconds = self.buffer_seconds.min(30.0);
        self.fullscreen_screen = self.fullscreen_screen.max(0);
        // mpv accepts "auto" or 10..10000; anything in between makes it exit with
        // "Invalid value for option target-peak", so treat a too-small number as auto.
        if self.target_peak_nits < 10 {
            self.target_peak_nits = 0;
        }
        self.target_peak_nits = self.target_peak_nits.min(10000);
        self
    }

    /// [`normalize`](Self::normalize), consuming: `ReceiverSettings { .. }.normalized()`.
    pub fn normalized(mut self) -> Self {
        self.normalize();
        self
    }

    /// Problems that prevent playing (empty when valid).
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.latency_ms < 20 {
            problems.push("Latency must be at least 20 ms.".to_string());
        }
        if self.expected_bitrate_mbps < 1 {
            problems.push("Expected bitrate must be at least 1 Mbps.".to_string());
        }
        if self.buffer_seconds <= 0.0 {
            problems.push("Buffer seconds must be positive.".to_string());
        }
        problems
    }

    /// oheadbw this viewer asks the host for: the cross-border (China) preset gets more retransmission headroom.
    pub fn overhead_percent(&self) -> u32 {
        if self.latency_ms >= 3000 {
            50
        } else {
            25
        }
    }

    /// mpv's SRT URL: a caller to the local end of the punched path (the viewer link's port on 127.0.0.1) with
    /// the group's passphrase; the receive buffer is sized for this viewer's latency and expected bitrate.
    pub fn to_srt_options(&self, local_port: u16, passphrase: &str) -> SrtUrlOptions {
        SrtUrlOptions {
            topology: SrtTopology::Caller,
            role: SrtRole::Read,
            host: "127.0.0.1".to_string(),
            port: local_port,
            passphrase: (!passphrase.is_empty()).then(|| passphrase.to_string()),
            pb_key_len: 32,
            latency_ms: u32::try_from(self.latency_ms).unwrap_or(0),
            bitrate_bps: u64::try_from(self.expected_bitrate_mbps).unwrap_or(0) * 1_000_000,
            rate_control: false,
            connect_timeout_ms: 5000,
            include_recv_socket_buffer: true,
            ..SrtUrlOptions::default()
        }
    }

    pub fn default_path() -> PathBuf {
        app_paths::settings_file(Self::FILE_NAME)
    }

    /// The settings in the text of a receiver.json, exactly as written (not normalised); `None` when the text
    /// is not a JSON object or a member has the wrong type.
    pub fn from_json(text: &str) -> Option<ReceiverSettings> {
        settings_file::from_json(text)
    }

    /// Never fails: a missing, unreadable or corrupt file gives the defaults, and whatever was read is
    /// normalised.
    pub fn load(path: &Path) -> ReceiverSettings {
        settings_file::load::<ReceiverSettings>(path)
            .unwrap_or_default()
            .normalized()
    }

    /// Writes the file atomically (a reader sees the old or the new file, never half of one).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        json::save_file(path, self)
    }
}
