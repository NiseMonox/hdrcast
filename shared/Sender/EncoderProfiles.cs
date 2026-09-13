using System;
using System.Collections.Generic;
using System.Globalization;

namespace HdrCast.Sender.Model
{
    /// <summary>Video encoders the sender can drive (design.md §2.5).</summary>
    public enum VideoEncoder
    {
        HevcNvenc,
        Av1Nvenc,
        HevcAmf,
        Av1Amf,
        HevcQsv,
        LibX265,
        LibSvtAv1,
    }

    /// <summary>
    /// Quality/speed trade-off. The whole point of this tool is a buffered livestream, so even "Speed"
    /// is far from a low-latency tune; "Quality" spends multipass + lookahead + B-frames on picture quality.
    /// </summary>
    public enum QualityPreset
    {
        Speed,
        Balanced,
        Quality,
    }

    /// <summary>Static facts about one encoder.</summary>
    public sealed class EncoderInfo
    {
        public VideoEncoder Id { get; }
        /// <summary>ffmpeg encoder name, e.g. "hevc_nvenc".</summary>
        public string FfmpegName { get; }
        public string DisplayName { get; }
        public string Vendor { get; }
        public bool IsHardware { get; }
        /// <summary>AV1 cannot be muxed into MPEG-TS (verified: ffmpeg writes stream_type 0x06 and MediaMTX rejects it).</summary>
        public bool MpegTsCompatible { get; }
        public bool Supports10Bit { get; }

        public EncoderInfo(VideoEncoder id, string ffmpegName, string displayName, string vendor,
                           bool isHardware, bool mpegTsCompatible, bool supports10Bit)
        {
            Id = id;
            FfmpegName = ffmpegName;
            DisplayName = displayName;
            Vendor = vendor;
            IsHardware = isHardware;
            MpegTsCompatible = mpegTsCompatible;
            Supports10Bit = supports10Bit;
        }

        public bool IsAv1 => Id == VideoEncoder.Av1Nvenc || Id == VideoEncoder.Av1Amf || Id == VideoEncoder.LibSvtAv1;
    }

    /// <summary>
    /// Encoder catalogue and the option strings the sender passes to ffmpeg.
    /// Every option name was checked against the ffmpeg 7.1 source option tables
    /// (libavcodec/nvenc_hevc.c, nvenc_av1.c, amfenc_hevc.c, qsvenc_hevc.c, qsvenc.h, libx265.c, libsvtav1.c).
    /// </summary>
    public static class EncoderProfiles
    {
        public static readonly EncoderInfo[] All =
        {
            new EncoderInfo(VideoEncoder.HevcNvenc, "hevc_nvenc", "HEVC (NVIDIA NVENC)", "NVIDIA", true,  true,  true),
            new EncoderInfo(VideoEncoder.Av1Nvenc,  "av1_nvenc",  "AV1 (NVIDIA NVENC)",  "NVIDIA", true,  false, true),
            new EncoderInfo(VideoEncoder.HevcAmf,   "hevc_amf",   "HEVC (AMD AMF)",      "AMD",    true,  true,  true),
            new EncoderInfo(VideoEncoder.Av1Amf,    "av1_amf",    "AV1 (AMD AMF)",       "AMD",    true,  false, true),
            new EncoderInfo(VideoEncoder.HevcQsv,   "hevc_qsv",   "HEVC (Intel QSV)",    "Intel",  true,  true,  true),
            new EncoderInfo(VideoEncoder.LibX265,   "libx265",    "HEVC (CPU, x265)",    "CPU",    false, true,  true),
            new EncoderInfo(VideoEncoder.LibSvtAv1, "libsvtav1",  "AV1 (CPU, SVT-AV1)",  "CPU",    false, false, true),
        };

        public static EncoderInfo Info(VideoEncoder e)
        {
            foreach (EncoderInfo i in All) if (i.Id == e) return i;
            throw new ArgumentOutOfRangeException(nameof(e));
        }

        public static EncoderInfo? ByFfmpegName(string name)
        {
            foreach (EncoderInfo i in All) if (string.Equals(i.FfmpegName, name, StringComparison.OrdinalIgnoreCase)) return i;
            return null;
        }

        /// <summary>Preference order used when auto-selecting an encoder from the probe results.</summary>
        public static readonly VideoEncoder[] AutoOrder =
        {
            VideoEncoder.HevcNvenc, VideoEncoder.HevcAmf, VideoEncoder.HevcQsv, VideoEncoder.LibX265,
        };

        /// <summary>Suggested bitrate in Mbps for a resolution/frame rate (a starting point for the GUI, not a limit).</summary>
        public static int SuggestedMbps(int width, int height, int fps)
        {
            long pixels = (long)width * height;
            double perFrame = pixels / (2560.0 * 1440.0);      // 1440p == 1.0
            double rate = fps / 60.0;
            double mbps = 40.0 * Math.Pow(perFrame, 0.85) * Math.Pow(rate, 0.6);
            int rounded = (int)Math.Round(mbps / 5.0) * 5;
            return Math.Max(10, Math.Min(100, rounded));
        }

        /// <summary>
        /// Builds the "-c:v … " option list (encoder plus its private options, rate control, GOP and colour tags).
        /// <paramref name="bitrateKbps"/> is the target CBR bitrate; <paramref name="bufferSeconds"/> sets -bufsize.
        /// </summary>
        public static List<string> BuildVideoArgs(EncoderSettings s)
        {
            if (s == null) throw new ArgumentNullException(nameof(s));
            EncoderInfo info = Info(s.Encoder);
            var a = new List<string> { "-c:v", info.FfmpegName };
            int gop = Math.Max(1, (int)Math.Round(s.Fps * s.GopSeconds));
            string kbps = s.BitrateKbps.ToString(CultureInfo.InvariantCulture) + "k";
            string bufsize = ((int)Math.Round(s.BitrateKbps * s.BufferSeconds)).ToString(CultureInfo.InvariantCulture) + "k";
            int bFrames = s.BFrames ? BFrameCount(s.Encoder, s.Preset) : 0;
            int lookahead = s.Lookahead ? LookaheadFrames(s.Preset) : 0;

            switch (s.Encoder)
            {
                case VideoEncoder.HevcNvenc:
                case VideoEncoder.Av1Nvenc:
                {
                    a.Add("-preset"); a.Add(NvencPreset(s.Preset));
                    a.Add("-tune"); a.Add("hq");
                    a.Add("-rc"); a.Add("cbr");
                    a.Add("-multipass"); a.Add(NvencMultipass(s.Preset));
                    a.Add("-rc-lookahead"); a.Add(lookahead.ToString(CultureInfo.InvariantCulture));
                    a.Add("-bf"); a.Add(bFrames.ToString(CultureInfo.InvariantCulture));
                    if (bFrames > 0) { a.Add("-b_ref_mode"); a.Add("middle"); }
                    a.Add("-spatial-aq"); a.Add("1");
                    a.Add("-aq-strength"); a.Add("8");
                    if (lookahead > 0) { a.Add("-temporal-aq"); a.Add(s.Preset == QualityPreset.Quality ? "1" : "0"); }
                    if (s.Encoder == VideoEncoder.HevcNvenc)
                    {
                        a.Add("-profile:v"); a.Add(s.Hdr ? "main10" : "main");
                        a.Add("-tier"); a.Add("high");
                    }
                    a.Add("-b:v"); a.Add(kbps);
                    a.Add("-maxrate"); a.Add(kbps);
                    a.Add("-bufsize"); a.Add(bufsize);
                    a.Add("-g"); a.Add(gop.ToString(CultureInfo.InvariantCulture));
                    a.Add("-forced-idr"); a.Add("1");
                    if (lookahead > 0) { a.Add("-no-scenecut"); a.Add("0"); }
                    break;
                }
                case VideoEncoder.HevcAmf:
                case VideoEncoder.Av1Amf:
                {
                    a.Add("-quality"); a.Add(AmfQuality(s.Preset));
                    a.Add("-rc"); a.Add("cbr");
                    a.Add("-b:v"); a.Add(kbps);
                    a.Add("-maxrate"); a.Add(kbps);
                    a.Add("-bufsize"); a.Add(bufsize);
                    a.Add("-g"); a.Add(gop.ToString(CultureInfo.InvariantCulture));
                    a.Add("-latency"); a.Add("0");                       // buffered stream: do not pick the low-latency path
                    if (s.Preset != QualityPreset.Speed)
                    {
                        a.Add("-preanalysis"); a.Add("1");
                        a.Add("-pa_lookahead_buffer_depth"); a.Add(Math.Min(40, Math.Max(1, lookahead)).ToString(CultureInfo.InvariantCulture));
                    }
                    if (s.Encoder == VideoEncoder.HevcAmf)
                    {
                        a.Add("-vbaq"); a.Add("1");
                        if (s.Hdr) { a.Add("-profile:v"); a.Add("main10"); }
                    }
                    break;
                }
                case VideoEncoder.HevcQsv:
                {
                    a.Add("-preset"); a.Add(QsvPreset(s.Preset));
                    a.Add("-b:v"); a.Add(kbps);
                    a.Add("-maxrate"); a.Add(kbps);
                    a.Add("-bufsize"); a.Add(bufsize);
                    a.Add("-g"); a.Add(gop.ToString(CultureInfo.InvariantCulture));
                    a.Add("-bf"); a.Add(bFrames.ToString(CultureInfo.InvariantCulture));
                    if (s.Preset != QualityPreset.Speed)
                    {
                        a.Add("-extbrc"); a.Add("1");
                        a.Add("-look_ahead_depth"); a.Add(Math.Min(100, Math.Max(10, lookahead)).ToString(CultureInfo.InvariantCulture));
                        a.Add("-b_strategy"); a.Add("1");
                    }
                    if (s.Hdr) { a.Add("-profile:v"); a.Add("main10"); }
                    break;
                }
                case VideoEncoder.LibX265:
                {
                    a.Add("-preset"); a.Add(X265Preset(s.Preset));
                    a.Add("-b:v"); a.Add(kbps);
                    a.Add("-x265-params"); a.Add(X265Params(s, gop, bFrames, lookahead));
                    break;
                }
                case VideoEncoder.LibSvtAv1:
                {
                    a.Add("-preset"); a.Add(SvtPreset(s.Preset));
                    a.Add("-b:v"); a.Add(kbps);
                    a.Add("-maxrate"); a.Add(kbps);
                    a.Add("-bufsize"); a.Add(bufsize);
                    a.Add("-g"); a.Add(gop.ToString(CultureInfo.InvariantCulture));
                    a.Add("-svtav1-params");
                    a.Add("rc=2:lookahead=" + Math.Max(0, lookahead).ToString(CultureInfo.InvariantCulture) +
                          ":keyint=" + gop.ToString(CultureInfo.InvariantCulture));
                    break;
                }
                default:
                    throw new ArgumentOutOfRangeException(nameof(s));
            }

            // Colour signalling. The capture engine already produced PQ/BT.2020 (HDR) or BT.709 (SDR) samples,
            // these flags only write the VUI so the player knows what it received.
            if (s.Hdr)
            {
                a.Add("-color_primaries"); a.Add("bt2020");
                a.Add("-color_trc"); a.Add("smpte2084");
                a.Add("-colorspace"); a.Add("bt2020nc");
            }
            else
            {
                a.Add("-color_primaries"); a.Add("bt709");
                a.Add("-color_trc"); a.Add("bt709");
                a.Add("-colorspace"); a.Add("bt709");
            }
            a.Add("-color_range"); a.Add("tv");
            a.Add("-chroma_sample_location"); a.Add("left");
            return a;
        }

        static string NvencPreset(QualityPreset p) => p == QualityPreset.Speed ? "p3" : p == QualityPreset.Balanced ? "p5" : "p7";
        static string NvencMultipass(QualityPreset p) => p == QualityPreset.Speed ? "disabled" : p == QualityPreset.Balanced ? "qres" : "fullres";
        static string AmfQuality(QualityPreset p) => p == QualityPreset.Speed ? "speed" : p == QualityPreset.Balanced ? "balanced" : "quality";
        static string QsvPreset(QualityPreset p) => p == QualityPreset.Speed ? "faster" : p == QualityPreset.Balanced ? "slow" : "veryslow";
        static string X265Preset(QualityPreset p) => p == QualityPreset.Speed ? "veryfast" : p == QualityPreset.Balanced ? "fast" : "medium";
        static string SvtPreset(QualityPreset p) => p == QualityPreset.Speed ? "10" : p == QualityPreset.Balanced ? "8" : "6";

        static int BFrameCount(VideoEncoder e, QualityPreset p)
        {
            if (e == VideoEncoder.LibSvtAv1) return 0;                  // SVT-AV1 manages its own pyramid
            switch (p)
            {
                case QualityPreset.Speed: return 0;
                case QualityPreset.Balanced: return 2;
                default: return 3;
            }
        }

        static int LookaheadFrames(QualityPreset p)
        {
            switch (p)
            {
                case QualityPreset.Speed: return 0;
                case QualityPreset.Balanced: return 20;
                default: return 32;
            }
        }

        static string X265Params(EncoderSettings s, int gop, int bFrames, int lookahead)
        {
            var parts = new List<string>
            {
                "keyint=" + gop.ToString(CultureInfo.InvariantCulture),
                "min-keyint=" + gop.ToString(CultureInfo.InvariantCulture),
                "scenecut=0",
                "repeat-headers=1",                                     // MPEG-TS: VPS/SPS/PPS before every IDR
                "bframes=" + bFrames.ToString(CultureInfo.InvariantCulture),
                "rc-lookahead=" + Math.Max(10, lookahead).ToString(CultureInfo.InvariantCulture),
                "vbv-maxrate=" + s.BitrateKbps.ToString(CultureInfo.InvariantCulture),
                "vbv-bufsize=" + ((int)Math.Round(s.BitrateKbps * s.BufferSeconds)).ToString(CultureInfo.InvariantCulture),
            };
            if (s.Hdr)
            {
                int maxNits = s.MasteringMaxNits > 0 ? s.MasteringMaxNits : 1000;
                parts.Add("hdr-opt=1");
                // BT.2020 primaries, D65 white, in the units x265 expects (chromaticity x 50000, luminance x 10000).
                parts.Add("master-display=G(8500,39850)B(6550,2300)R(35400,14600)WP(15635,16450)L(" +
                          (maxNits * 10000L).ToString(CultureInfo.InvariantCulture) + ",1)");
                parts.Add("max-cll=" + maxNits.ToString(CultureInfo.InvariantCulture) + "," +
                          Math.Max(50, maxNits / 4).ToString(CultureInfo.InvariantCulture));
            }
            return string.Join(":", parts.ToArray());
        }
    }

    /// <summary>Input of <see cref="EncoderProfiles.BuildVideoArgs"/>.</summary>
    public sealed class EncoderSettings
    {
        public VideoEncoder Encoder { get; set; } = VideoEncoder.HevcNvenc;
        public QualityPreset Preset { get; set; } = QualityPreset.Balanced;
        public bool Hdr { get; set; }
        public int BitrateKbps { get; set; } = 40000;
        public int Fps { get; set; } = 60;
        public double GopSeconds { get; set; } = 2.0;
        /// <summary>-bufsize as a multiple of the bitrate (seconds of VBV buffer).</summary>
        public double BufferSeconds { get; set; } = 2.0;
        public bool BFrames { get; set; } = true;
        public bool Lookahead { get; set; } = true;
        /// <summary>Display peak luminance for the x265 mastering-display metadata (0 = default 1000).</summary>
        public int MasteringMaxNits { get; set; }
    }
}
