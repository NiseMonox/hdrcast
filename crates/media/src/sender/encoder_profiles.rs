//! Encoder catalogue and the option strings the sender passes to ffmpeg.
//!
//! Every option name was checked against the ffmpeg 7.1 source option tables (libavcodec/nvenc_hevc.c,
//! amfenc_hevc.c, qsvenc_hevc.c, qsvenc.h, libx265.c), and the integration tests run the generated lists
//! through a real ffmpeg. The VideoToolbox profile is the exception: see [`VideoEncoder::HevcVideoToolbox`].

use serde::Serialize;

use super::settings::SenderSettings;
use crate::text::words;
use crate::Platform;

int_enum! {
    /// Video encoders the sender can drive (design.md §2.5). HEVC only: the P2P fan-out carries MPEG-TS, which
    /// has no AV1 mapping in ffmpeg.
    ///
    /// The numbers are what sender.json stores. 1 and 3 were AV1 encoders in files older than 0.2, and so was
    /// 6: such a file now reads as VideoToolbox, which Windows does not offer, so
    /// [`SenderSettings::normalize`](super::SenderSettings::normalize) still turns it into the default there.
    pub enum VideoEncoder (fallback = default_encoder(Platform::CURRENT)) {
        HevcNvenc = 0,
        HevcAmf = 2,
        HevcQsv = 4,
        LibX265 = 5,
        /// ffmpeg's `hevc_videotoolbox` (macOS). Its option list was written from libavcodec/videotoolboxenc.c;
        /// it could not be run on the development machine (Windows), so it still needs a run on a real Mac.
        HevcVideoToolbox = 6,
    }
}

int_enum! {
    /// Quality/speed trade-off. The whole point of this tool is a buffered livestream, so even "Speed" is far
    /// from a low-latency tune; "Quality" spends multipass + lookahead + B-frames on picture quality.
    pub enum QualityPreset (fallback = QualityPreset::Balanced) {
        Speed = 0,
        Balanced = 1,
        Quality = 2,
    }
}

/// Static facts about one encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EncoderInfo {
    pub id: VideoEncoder,
    /// ffmpeg encoder name, e.g. "hevc_nvenc".
    pub ffmpeg_name: &'static str,
    pub display_name: &'static str,
    pub vendor: &'static str,
    pub is_hardware: bool,
    #[serde(rename = "supports10Bit")]
    pub supports_10bit: bool,
}

const fn encoder(
    id: VideoEncoder,
    ffmpeg_name: &'static str,
    display_name: &'static str,
    vendor: &'static str,
    is_hardware: bool,
) -> EncoderInfo {
    EncoderInfo {
        id,
        ffmpeg_name,
        display_name,
        vendor,
        is_hardware,
        supports_10bit: true,
    }
}

const NVENC: EncoderInfo = encoder(
    VideoEncoder::HevcNvenc,
    "hevc_nvenc",
    "HEVC (NVIDIA NVENC)",
    "NVIDIA",
    true,
);
const AMF: EncoderInfo = encoder(
    VideoEncoder::HevcAmf,
    "hevc_amf",
    "HEVC (AMD AMF)",
    "AMD",
    true,
);
const QSV: EncoderInfo = encoder(
    VideoEncoder::HevcQsv,
    "hevc_qsv",
    "HEVC (Intel QSV)",
    "Intel",
    true,
);
const X265: EncoderInfo = encoder(
    VideoEncoder::LibX265,
    "libx265",
    "HEVC (CPU, x265)",
    "CPU",
    false,
);
const VIDEOTOOLBOX: EncoderInfo = encoder(
    VideoEncoder::HevcVideoToolbox,
    "hevc_videotoolbox",
    "HEVC (Apple VideoToolbox)",
    "Apple",
    true,
);

/// Every encoder HdrCast knows, on whatever platform, in the order of their numbers.
pub const ALL: [EncoderInfo; 5] = [NVENC, AMF, QSV, X265, VIDEOTOOLBOX];

const DESKTOP_PC: [EncoderInfo; 4] = [NVENC, AMF, QSV, X265];
const MAC: [EncoderInfo; 2] = [VIDEOTOOLBOX, X265];

/// The encoders the UI offers on `platform`, best first: this is also the order in which a probe result is
/// preferred ([`auto_order`]), and the first one is the encoder of fresh settings ([`default_encoder`]).
/// Windows (and Linux, which has the same GPU vendors): NVENC, AMF, QSV, x265. macOS: VideoToolbox, x265.
pub fn for_platform(platform: Platform) -> &'static [EncoderInfo] {
    match platform {
        Platform::MacOs => &MAC,
        Platform::Windows | Platform::Other => &DESKTOP_PC,
    }
}

/// The encoder of fresh settings on `platform`.
pub fn default_encoder(platform: Platform) -> VideoEncoder {
    for_platform(platform)[0].id
}

/// True when `platform` has `encoder` at all (a sender.json carried over from another OS may name one it has not).
pub fn is_offered(encoder: VideoEncoder, platform: Platform) -> bool {
    for_platform(platform).iter().any(|info| info.id == encoder)
}

/// Preference order used when auto-selecting an encoder from the probe results.
pub fn auto_order(platform: Platform) -> impl Iterator<Item = VideoEncoder> {
    for_platform(platform).iter().map(|info| info.id)
}

pub fn info(encoder: VideoEncoder) -> &'static EncoderInfo {
    match encoder {
        VideoEncoder::HevcNvenc => &NVENC,
        VideoEncoder::HevcAmf => &AMF,
        VideoEncoder::HevcQsv => &QSV,
        VideoEncoder::LibX265 => &X265,
        VideoEncoder::HevcVideoToolbox => &VIDEOTOOLBOX,
    }
}

pub fn by_ffmpeg_name(name: &str) -> Option<&'static EncoderInfo> {
    ALL.iter()
        .find(|info| info.ffmpeg_name.eq_ignore_ascii_case(name))
}

/// Pixel format the encoder actually wants. The capture engine always produces p010/nv12, so the CPU encoders
/// need a conversion: naming it explicitly keeps ffmpeg from negotiating 8-bit for an HDR stream.
pub fn pixel_format(encoder: VideoEncoder, hdr: bool) -> &'static str {
    match (encoder, hdr) {
        // x265 does not accept p010/nv12
        (VideoEncoder::LibX265, true) => "yuv420p10le",
        (VideoEncoder::LibX265, false) => "yuv420p",
        // NVENC / AMF / QSV / VideoToolbox take the capture format as is
        (_, true) => "p010le",
        (_, false) => "nv12",
    }
}

/// Suggested bitrate in Mbps for a resolution/frame rate (a starting point for the UI, not a limit).
pub fn suggested_mbps(width: u32, height: u32, fps: u32) -> u32 {
    let pixels = f64::from(width) * f64::from(height);
    let per_frame = pixels / (2560.0 * 1440.0); // 1440p == 1.0
    let rate = f64::from(fps) / 60.0;
    let mbps = 40.0 * per_frame.powf(0.85) * rate.powf(0.6);
    let rounded = (mbps / 5.0).round_ties_even() as u32 * 5;
    rounded.clamp(10, 100)
}

/// Input of [`build_video_args`].
#[derive(Debug, Clone, PartialEq)]
pub struct EncoderSettings {
    pub encoder: VideoEncoder,
    pub preset: QualityPreset,
    pub hdr: bool,
    /// Target (constant) bitrate.
    pub bitrate_kbps: u32,
    pub fps: u32,
    pub gop_seconds: f64,
    /// -bufsize as a multiple of the bitrate (seconds of VBV buffer).
    pub buffer_seconds: f64,
    pub b_frames: bool,
    pub lookahead: bool,
    /// Display peak luminance for the x265 mastering-display metadata (0 = default 1000).
    pub mastering_max_nits: u32,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        EncoderSettings {
            encoder: default_encoder(Platform::CURRENT),
            preset: QualityPreset::Balanced,
            hdr: false,
            bitrate_kbps: 40000,
            fps: 60,
            gop_seconds: SenderSettings::DEFAULT_GOP_SECONDS,
            buffer_seconds: SenderSettings::DEFAULT_BUFFER_SECONDS,
            b_frames: true,
            lookahead: true,
            mastering_max_nits: 0,
        }
    }
}

/// `(int)Math.Round(x)`: half to even, as the C# apps computed every size in here.
fn round(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// Builds the "-c:v …" option list: the encoder plus its private options, rate control, GOP and colour tags.
pub fn build_video_args(s: &EncoderSettings) -> Vec<String> {
    let mut a: Vec<String> = Vec::with_capacity(48);
    let mut opt = |name: &str, value: &str| {
        a.push(name.to_string());
        a.push(value.to_string());
    };

    opt("-c:v", info(s.encoder).ffmpeg_name);
    opt("-pix_fmt", pixel_format(s.encoder, s.hdr));

    let gop = round(f64::from(s.fps) * s.gop_seconds).max(1).to_string();
    let kbps = format!("{}k", s.bitrate_kbps);
    let buffer_kbits = round(f64::from(s.bitrate_kbps) * s.buffer_seconds);
    let bufsize = format!("{buffer_kbits}k");
    let b_frames = if s.b_frames {
        b_frame_count(s.preset)
    } else {
        0
    };
    let lookahead = if s.lookahead {
        lookahead_frames(s.preset)
    } else {
        0
    };
    let preset = s.preset;

    match s.encoder {
        VideoEncoder::HevcNvenc => {
            opt("-preset", pick(preset, "p3", "p5", "p7"));
            opt("-tune", "hq");
            opt("-rc", "cbr");
            opt("-multipass", pick(preset, "disabled", "qres", "fullres"));
            opt("-rc-lookahead", &lookahead.to_string());
            opt("-bf", &b_frames.to_string());
            if b_frames > 0 {
                opt("-b_ref_mode", "middle");
            }
            opt("-spatial-aq", "1");
            opt("-aq-strength", "8");
            if lookahead > 0 {
                opt(
                    "-temporal-aq",
                    if preset == QualityPreset::Quality {
                        "1"
                    } else {
                        "0"
                    },
                );
            }
            opt("-profile:v", if s.hdr { "main10" } else { "main" });
            opt("-tier", "high");
            opt("-b:v", &kbps);
            opt("-maxrate", &kbps);
            opt("-bufsize", &bufsize);
            opt("-g", &gop);
            opt("-forced-idr", "1");
            if lookahead > 0 {
                opt("-no-scenecut", "0");
            }
        }
        VideoEncoder::HevcAmf => {
            opt("-quality", pick(preset, "speed", "balanced", "quality"));
            opt("-rc", "cbr");
            opt("-b:v", &kbps);
            opt("-maxrate", &kbps);
            opt("-bufsize", &bufsize);
            opt("-g", &gop);
            opt("-latency", "0"); // buffered stream: do not pick the low-latency path
            if preset != QualityPreset::Speed {
                opt("-preanalysis", "1");
                opt(
                    "-pa_lookahead_buffer_depth",
                    &lookahead.clamp(1, 40).to_string(),
                );
            }
            opt("-vbaq", "1");
            if s.hdr {
                opt("-profile:v", "main10");
            }
        }
        VideoEncoder::HevcQsv => {
            opt("-preset", pick(preset, "faster", "slow", "veryslow"));
            opt("-b:v", &kbps);
            opt("-maxrate", &kbps);
            opt("-bufsize", &bufsize);
            opt("-g", &gop);
            opt("-bf", &b_frames.to_string());
            if preset != QualityPreset::Speed {
                opt("-extbrc", "1");
                opt("-look_ahead_depth", &lookahead.clamp(10, 100).to_string());
                opt("-b_strategy", "1");
            }
            if s.hdr {
                opt("-profile:v", "main10");
            }
        }
        VideoEncoder::LibX265 => {
            opt("-preset", pick(preset, "veryfast", "fast", "medium"));
            opt("-b:v", &kbps);
            opt(
                "-x265-params",
                &x265_params(s, &gop, b_frames, lookahead, buffer_kbits),
            );
        }
        VideoEncoder::HevcVideoToolbox => {
            // NOT RUN on the development machine (a Windows PC: its ffmpeg has no VideoToolbox), so this
            // profile still needs a run on a real Mac. Every name below was read in the option tables and in
            // vtenc_create_encoder() of libavcodec/videotoolboxenc.c (FFmpeg master, October 2026):
            //   -profile:v main|main10  private option; Main10 needs the 10-bit input (p010le) named above
            //   -b:v                    kVTCompressionPropertyKey_AverageBitRate
            //   -maxrate                kVTCompressionPropertyKey_DataRateLimits (bytes over one second);
            //                           ffmpeg ignores a refusal of it for HEVC on old macOS versions
            //   -bufsize                a generic codec option VideoToolbox has no use for; kept so the three
            //                           rate options read the same for every hardware encoder
            //   -g                      kVTCompressionPropertyKey_MaxKeyFrameInterval
            //   -bf                     only "more than 0" matters: it allows frame reordering
            //   -realtime 1             kVTCompressionPropertyKey_RealTime: a live capture must not fall behind
            //   -allow_sw 0             hardware only: the software HEVC encoder cannot keep up with a desktop,
            //                           and failing here lets the probe fall back to x265 instead
            // There is no preset, lookahead or multipass to map the quality setting to; only B-frames follow
            // it. -constant_bit_rate (macOS 13+, errors where unsupported) and -prio_speed are left alone.
            opt("-profile:v", if s.hdr { "main10" } else { "main" });
            opt("-b:v", &kbps);
            opt("-maxrate", &kbps);
            opt("-bufsize", &bufsize);
            opt("-g", &gop);
            opt("-bf", &b_frames.to_string());
            opt("-realtime", "1");
            opt("-allow_sw", "0");
        }
    }

    // Colour signalling. The capture engine already produced PQ/BT.2020 (HDR) or BT.709 (SDR) samples, these
    // flags only write the VUI so the player knows what it received. On their own they are not enough with
    // ffmpeg 7.1+: ffmpeg_command::build also puts color_tags on the raw input (see there).
    a.extend(color_tags(s.hdr));
    a.push("-chroma_sample_location".to_string());
    a.push("left".to_string());
    a
}

/// What the capture engine's samples are: PQ / BT.2020 (HDR) or BT.709 (SDR), limited range.
pub fn color_tags(hdr: bool) -> Vec<String> {
    words(if hdr {
        "-color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv"
    } else {
        "-color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv"
    })
}

fn pick(
    preset: QualityPreset,
    speed: &'static str,
    balanced: &'static str,
    quality: &'static str,
) -> &'static str {
    match preset {
        QualityPreset::Speed => speed,
        QualityPreset::Balanced => balanced,
        QualityPreset::Quality => quality,
    }
}

fn b_frame_count(preset: QualityPreset) -> u32 {
    match preset {
        QualityPreset::Speed => 0,
        QualityPreset::Balanced => 2,
        QualityPreset::Quality => 3,
    }
}

fn lookahead_frames(preset: QualityPreset) -> u32 {
    match preset {
        QualityPreset::Speed => 0,
        QualityPreset::Balanced => 20,
        QualityPreset::Quality => 32,
    }
}

fn x265_params(
    s: &EncoderSettings,
    gop: &str,
    b_frames: u32,
    lookahead: u32,
    buffer_kbits: i64,
) -> String {
    let mut parts = vec![
        format!("keyint={gop}"),
        format!("min-keyint={gop}"),
        "scenecut=0".to_string(),
        "repeat-headers=1".to_string(), // MPEG-TS: VPS/SPS/PPS before every IDR
        format!("bframes={b_frames}"),
        format!("rc-lookahead={}", lookahead.max(10)),
        format!("vbv-maxrate={}", s.bitrate_kbps),
        format!("vbv-bufsize={buffer_kbits}"),
    ];
    // ffmpeg's libx265 wrapper forwards -colorspace and -color_range to x265 but NOT -color_primaries or
    // -color_trc (verified on 7.1.5: they come back as "unknown"), so the VUI has to be set here or the
    // viewer's player sees an untagged stream and shows HDR as washed-out SDR.
    parts.push(format!(
        "colorprim={}",
        if s.hdr { "bt2020" } else { "bt709" }
    ));
    parts.push(format!(
        "transfer={}",
        if s.hdr { "smpte2084" } else { "bt709" }
    ));
    parts.push(format!(
        "colormatrix={}",
        if s.hdr { "bt2020nc" } else { "bt709" }
    ));
    parts.push("range=limited".to_string());
    if s.hdr {
        let max_nits = if s.mastering_max_nits > 0 {
            s.mastering_max_nits
        } else {
            1000
        };
        parts.push("hdr-opt=1".to_string());
        // BT.2020 primaries, D65 white, in the units x265 expects (chromaticity x 50000, luminance x 10000).
        parts.push(format!(
            "master-display=G(8500,39850)B(6550,2300)R(35400,14600)WP(15635,16450)L({},1)",
            u64::from(max_nits) * 10000
        ));
        parts.push(format!("max-cll={},{}", max_nits, (max_nits / 4).max(50)));
    }
    parts.join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_round_half_to_even_like_the_csharp_apps() {
        // 25 fps x 0.5 s = 12.5 frames: Math.Round gave 12, not 13
        let s = EncoderSettings {
            encoder: VideoEncoder::HevcNvenc,
            fps: 25,
            gop_seconds: 0.5,
            ..Default::default()
        };
        let a = build_video_args(&s);
        assert_eq!(a[a.iter().position(|x| x == "-g").unwrap() + 1], "12");
        // 1001 kbit/s x 0.5 s = 500.5 -> 500
        let s = EncoderSettings {
            encoder: VideoEncoder::HevcNvenc,
            bitrate_kbps: 1001,
            buffer_seconds: 0.5,
            ..Default::default()
        };
        let a = build_video_args(&s);
        assert_eq!(
            a[a.iter().position(|x| x == "-bufsize").unwrap() + 1],
            "500k"
        );
        // a GOP is never shorter than one frame
        let s = EncoderSettings {
            encoder: VideoEncoder::HevcNvenc,
            fps: 1,
            gop_seconds: 0.25,
            ..Default::default()
        };
        let a = build_video_args(&s);
        assert_eq!(a[a.iter().position(|x| x == "-g").unwrap() + 1], "1");
    }
}
