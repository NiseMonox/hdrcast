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
        /// <summary>Where the MPEG-TS goes: stdout, into the P2P fan-out (design.md §8.8).</summary>
        public string OutputUrl { get; set; } = "pipe:1";
        /// <summary>Emit -progress so the GUI can read encoder statistics.</summary>
        public bool Progress { get; set; } = true;
        /// <summary>Where -progress writes: stderr, because stdout carries the stream (lines are told apart by shape).</summary>
        public string ProgressUrl { get; set; } = "pipe:2";
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
            if (p.Progress) { a.Add("-stats_period"); a.Add("1"); a.Add("-progress"); a.Add(p.ProgressUrl); }

            // Video input: raw frames from the capture engine, paced by the engine itself.
            a.Add("-thread_queue_size"); a.Add("1024");
            a.Add("-f"); a.Add("rawvideo");
            a.Add("-pix_fmt"); a.Add(p.PixelFormat == CaptureFormat.P010 ? "p010le" : "nv12");
            a.Add("-video_size"); a.Add(p.Width.ToString(CultureInfo.InvariantCulture) + "x" + p.Height.ToString(CultureInfo.InvariantCulture));
            a.Add("-framerate"); a.Add(p.Fps.ToString(CultureInfo.InvariantCulture));
            // Tag the raw frames themselves. ffmpeg 7.1+ negotiates colour spaces in the filter graph and gives the
            // encoder the frames' colour properties, so tags on the output alone went wrong twice (v0.2.1, ffmpeg 9):
            // untagged input counted as BT.601 and was re-matrixed to BT.2020 (saturated colours shifted by up to
            // 23 code values), and hardware encoders wrote transfer / primaries "unknown", so every player showed
            // the PQ signal as plain SDR: wrong colours on HDR and SDR screens alike.
            a.AddRange(EncoderProfiles.ColorTags(p.Encoder.Hdr));
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
            // +resend_headers: a viewer who joins mid-stream gets PAT/PMT again right away
            a.Add("-f"); a.Add("mpegts");
            a.Add("-mpegts_flags"); a.Add("+resend_headers");
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
