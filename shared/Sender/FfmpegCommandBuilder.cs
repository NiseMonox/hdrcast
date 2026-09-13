using System;
using System.Collections.Generic;
using System.Globalization;
using HdrCast.Capture;
using HdrCast.Common;

namespace HdrCast.Sender.Model
{
    /// <summary>Audio codec for the MPEG-TS stream. AAC is the safe default; Opus is smaller but less universally played.</summary>
    public enum AudioCodec
    {
        None,
        Aac,
        Opus,
    }

    /// <summary>Container ffmpeg muxes into. AV1 has no MPEG-TS mapping, so it must use Matroska (direct mode only).</summary>
    public enum Container
    {
        MpegTs,
        Matroska,
    }

    /// <summary>Everything the ffmpeg child process needs (design.md §2.5).</summary>
    public sealed class FfmpegPlan
    {
        public string VideoPipeName { get; set; } = CaptureArgs.DefaultVideoPipe;
        public string? AudioPipeName { get; set; }
        public CaptureFormat PixelFormat { get; set; } = CaptureFormat.P010;
        public int Width { get; set; }
        public int Height { get; set; }
        public int Fps { get; set; } = 60;
        public EncoderSettings Encoder { get; set; } = new EncoderSettings();
        public AudioCodec Audio { get; set; } = AudioCodec.Aac;
        public int AudioBitrateKbps { get; set; } = 192;
        public Container Container { get; set; } = Container.MpegTs;
        /// <summary>Complete srt:// URL from <see cref="SrtUrl.Build"/>.</summary>
        public string OutputUrl { get; set; } = "";
        /// <summary>Emit -progress pipe:1 so the GUI can read encoder statistics.</summary>
        public bool Progress { get; set; } = true;
        public string LogLevel { get; set; } = "warning";
    }

    /// <summary>Builds the ffmpeg argument vector. Pure logic: unit-tested on Linux.</summary>
    public static class FfmpegCommandBuilder
    {
        /// <summary>Windows named pipe path used by both the capture engine (server) and ffmpeg (client).</summary>
        public static string PipePath(string name) => @"\\.\pipe\" + name;

        public static List<string> Build(FfmpegPlan p)
        {
            if (p == null) throw new ArgumentNullException(nameof(p));
            if (p.Width <= 0 || p.Height <= 0) throw new ArgumentException("frame size is required", nameof(p));
            if ((p.Width & 1) != 0 || (p.Height & 1) != 0) throw new ArgumentException("frame size must be even", nameof(p));
            if (p.Fps <= 0) throw new ArgumentException("fps must be positive", nameof(p));
            if (string.IsNullOrEmpty(p.OutputUrl)) throw new ArgumentException("output url is required", nameof(p));
            if (p.PixelFormat == CaptureFormat.Auto) throw new ArgumentException("pixel format must be resolved", nameof(p));

            EncoderInfo info = EncoderProfiles.Info(p.Encoder.Encoder);
            if (p.Container == Container.MpegTs && !info.MpegTsCompatible)
                throw new ArgumentException(info.DisplayName + " cannot be muxed into MPEG-TS; use direct mode with Matroska", nameof(p));
            if (p.Encoder.Hdr && !info.Supports10Bit)
                throw new ArgumentException(info.DisplayName + " has no 10-bit mode", nameof(p));
            if (p.Encoder.Hdr != (p.PixelFormat == CaptureFormat.P010))
                throw new ArgumentException("HDR encoding needs p010 input (and SDR needs nv12)", nameof(p));

            var a = new List<string>
            {
                "-hide_banner",
                "-loglevel", p.LogLevel,
                "-nostdin",
            };
            if (p.Progress) { a.Add("-stats_period"); a.Add("1"); a.Add("-progress"); a.Add("pipe:1"); }

            // Video input: raw frames from the capture engine, paced by the engine itself.
            a.Add("-thread_queue_size"); a.Add("1024");
            a.Add("-f"); a.Add("rawvideo");
            a.Add("-pix_fmt"); a.Add(p.PixelFormat == CaptureFormat.P010 ? "p010le" : "nv12");
            a.Add("-video_size"); a.Add(p.Width.ToString(CultureInfo.InvariantCulture) + "x" + p.Height.ToString(CultureInfo.InvariantCulture));
            a.Add("-framerate"); a.Add(p.Fps.ToString(CultureInfo.InvariantCulture));
            a.Add("-i"); a.Add(PipePath(p.VideoPipeName));

            bool hasAudio = p.Audio != AudioCodec.None && !string.IsNullOrEmpty(p.AudioPipeName);
            if (hasAudio)
            {
                a.Add("-thread_queue_size"); a.Add("1024");
                a.Add("-f"); a.Add("f32le");
                a.Add("-ar"); a.Add("48000");
                a.Add("-ac"); a.Add("2");
                a.Add("-i"); a.Add(PipePath(p.AudioPipeName!));
            }

            a.AddRange(EncoderProfiles.BuildVideoArgs(p.Encoder));
            // The engine writes exactly Fps frames per second; keep ffmpeg from resampling timestamps.
            a.Add("-fps_mode"); a.Add("cfr");

            if (hasAudio)
            {
                a.Add("-c:a"); a.Add(p.Audio == AudioCodec.Opus ? "libopus" : "aac");
                a.Add("-b:a"); a.Add(p.AudioBitrateKbps.ToString(CultureInfo.InvariantCulture) + "k");
                a.Add("-ac"); a.Add("2");
            }

            a.Add("-muxdelay"); a.Add("0");
            a.Add("-muxpreload"); a.Add("0");
            if (p.Container == Container.MpegTs)
            {
                a.Add("-f"); a.Add("mpegts");
                a.Add("-mpegts_flags"); a.Add("+resend_headers");
            }
            else
            {
                a.Add("-f"); a.Add("matroska");
                a.Add("-live"); a.Add("1");
            }
            a.Add(p.OutputUrl);
            return a;
        }

        /// <summary>The full command line as the user would type it (for "copy command" and the log).</summary>
        public static string CommandLine(string ffmpegExe, FfmpegPlan p)
        {
            var argv = new List<string> { ffmpegExe };
            argv.AddRange(Build(p));
            return ArgQuote.Join(argv);
        }

        /// <summary>
        /// A one-frame encode of a synthetic source, used to check that an encoder and its options really work
        /// on this machine before streaming (design.md §2.5 "encoder probe"). Writes to null.
        /// </summary>
        public static List<string> BuildProbe(EncoderSettings encoder, int width = 640, int height = 480)
        {
            var e = new EncoderSettings
            {
                Encoder = encoder.Encoder,
                Preset = encoder.Preset,
                Hdr = encoder.Hdr,
                BitrateKbps = 2000,
                Fps = 30,
                GopSeconds = encoder.GopSeconds,
                BufferSeconds = encoder.BufferSeconds,
                BFrames = encoder.BFrames,
                Lookahead = encoder.Lookahead,
                MasteringMaxNits = encoder.MasteringMaxNits,
            };
            // One second of source, not a few frames: SVT-AV1 fills a pipeline before it emits anything and
            // ffmpeg fails with "no packets" on a shorter clip. The pixel format comes from the encoder profile.
            var a = new List<string>
            {
                "-hide_banner", "-loglevel", "error", "-nostdin",
                "-f", "lavfi",
                "-i", "testsrc2=size=" + width.ToString(CultureInfo.InvariantCulture) + "x" +
                      height.ToString(CultureInfo.InvariantCulture) + ":rate=30:duration=1",
            };
            a.AddRange(EncoderProfiles.BuildVideoArgs(e));
            a.Add("-f"); a.Add("null");
            a.Add("-");
            return a;
        }
    }
}
