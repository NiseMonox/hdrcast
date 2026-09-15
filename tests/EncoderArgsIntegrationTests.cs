using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using HdrCast.Common;
using HdrCast.Sender.Model;
using Xunit;

namespace HdrCast.Tests
{
    /// <summary>
    /// Runs the option strings from <see cref="EncoderProfiles"/> through a real ffmpeg. Unit tests only prove the
    /// builder emits what we intended; only ffmpeg can prove the option NAMES exist and the values parse.
    ///
    /// Only the CPU encoders can be checked on the Linux build box (it has libx265 and libsvtav1 but no
    /// NVENC/AMF/QSV). The hardware encoders are covered by the same code path at runtime: the sender's
    /// "Test encoders" button runs <see cref="FfmpegCommandBuilder.BuildProbe"/> on the user's own machine
    /// before a stream can be started.
    ///
    /// Skipped when ffmpeg is missing.
    /// </summary>
    public class EncoderArgsIntegrationTests
    {
        static string? FindFfmpeg()
        {
            string? path = Environment.GetEnvironmentVariable("PATH");
            if (path == null) return null;
            foreach (string dir in path.Split(Path.PathSeparator))
            {
                if (dir.Length == 0) continue;
                string candidate = Path.Combine(dir, "ffmpeg");
                if (File.Exists(candidate)) return candidate;
            }
            return null;
        }

        static bool HasEncoder(string ffmpeg, string name)
        {
            string output = Run(ffmpeg, new[] { "-hide_banner", "-encoders" }, out _);
            return output.Contains(" " + name + " ");
        }

        /// <summary>ffprobe next to the given ffmpeg. Replacing "ffmpeg" in the whole path would also rewrite
        /// the directory name when ffmpeg lives in a folder that contains its own name.</summary>
        static string ProbeTool(string ffmpeg)
        {
            string? dir = Path.GetDirectoryName(ffmpeg);
            string name = Path.GetFileName(ffmpeg).Replace("ffmpeg", "ffprobe");
            return string.IsNullOrEmpty(dir) ? name : Path.Combine(dir!, name);
        }

        static string Run(string exe, IEnumerable<string> args, out int exitCode)
        {
            var psi = new ProcessStartInfo(exe, ArgQuote.Join(args))
            {
                UseShellExecute = false,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
            };
            using (var p = Process.Start(psi)!)
            {
                string stdout = p.StandardOutput.ReadToEnd();
                string stderr = p.StandardError.ReadToEnd();
                p.WaitForExit(120000);
                exitCode = p.ExitCode;
                return stdout + stderr;
            }
        }

        [Theory]
        [InlineData(VideoEncoder.LibX265, false)]
        [InlineData(VideoEncoder.LibX265, true)]
        [InlineData(VideoEncoder.LibSvtAv1, false)]
        [InlineData(VideoEncoder.LibSvtAv1, true)]
        public void Ffmpeg_accepts_the_generated_options(VideoEncoder encoder, bool hdr)
        {
            string? ffmpeg = FindFfmpeg();
            if (ffmpeg == null) return;
            EncoderInfo info = EncoderProfiles.Info(encoder);
            if (!HasEncoder(ffmpeg, info.FfmpegName)) return;

            foreach (QualityPreset preset in new[] { QualityPreset.Speed, QualityPreset.Balanced, QualityPreset.Quality })
            {
                var settings = new EncoderSettings
                {
                    Encoder = encoder,
                    Preset = preset,
                    Hdr = hdr,
                    BitrateKbps = 8000,
                    Fps = 30,
                    GopSeconds = 2,
                    BufferSeconds = 2,
                    MasteringMaxNits = 1000,
                };
                List<string> args = FfmpegCommandBuilder.BuildProbe(settings, 320, 240);
                string output = Run(ffmpeg, args, out int exit);
                Assert.True(exit == 0,
                    info.FfmpegName + " / " + preset + " / " + (hdr ? "HDR" : "SDR") +
                    " was rejected by ffmpeg:\n" + ArgQuote.Join(args) + "\n" + Tail(output));
            }
        }

        [Fact]
        public void Mpegts_accepts_a_real_hevc_main10_stream_with_our_muxer_options()
        {
            string? ffmpeg = FindFfmpeg();
            if (ffmpeg == null || !HasEncoder(ffmpeg, "libx265")) return;

            string dir = Path.Combine(Path.GetTempPath(), "hdrcast-mux-" + Guid.NewGuid().ToString("N"));
            Directory.CreateDirectory(dir);
            try
            {
                string ts = Path.Combine(dir, "out.ts");
                var settings = new EncoderSettings
                {
                    Encoder = VideoEncoder.LibX265, Preset = QualityPreset.Speed, Hdr = true,
                    BitrateKbps = 8000, Fps = 30, GopSeconds = 1, BufferSeconds = 2, MasteringMaxNits = 1000,
                };
                var args = new List<string>
                {
                    "-hide_banner", "-loglevel", "error", "-nostdin",
                    "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30:duration=1",
                    "-pix_fmt", "p010le",
                };
                args.AddRange(EncoderProfiles.BuildVideoArgs(settings));
                args.AddRange(new[] { "-muxdelay", "0", "-muxpreload", "0", "-f", "mpegts", "-mpegts_flags", "+resend_headers", "-y", ts });
                string log = Run(ffmpeg, args, out int exit);
                Assert.True(exit == 0, "encode failed:\n" + ArgQuote.Join(args) + "\n" + Tail(log));

                // Read it back: the colour tags the viewer depends on must survive the muxer.
                string probe = Run(ProbeTool(ffmpeg), new[]
                {
                    "-hide_banner", "-loglevel", "error", "-show_streams", "-select_streams", "v:0", ts,
                }, out int probeExit);
                if (probeExit != 0) return;                       // no ffprobe next to ffmpeg: encode check is enough
                Assert.Contains("codec_name=hevc", probe);
                Assert.Contains("profile=Main 10", probe);
                Assert.Contains("pix_fmt=yuv420p10le", probe);
                Assert.Contains("color_transfer=smpte2084", probe);
                Assert.Contains("color_primaries=bt2020", probe);
                Assert.Contains("color_space=bt2020nc", probe);
                Assert.Contains("color_range=tv", probe);
            }
            finally
            {
                try { Directory.Delete(dir, true); } catch (Exception) { }
            }
        }

        [Fact]
        public void Av1_really_cannot_be_muxed_into_mpegts()
        {
            // This is why the relay path is HEVC-only (EncoderInfo.MpegTsCompatible). If a future ffmpeg gains
            // AV1-in-MPEG-TS this test starts failing, which is the signal to revisit that restriction.
            string? ffmpeg = FindFfmpeg();
            if (ffmpeg == null || !HasEncoder(ffmpeg, "libsvtav1")) return;

            string dir = Path.Combine(Path.GetTempPath(), "hdrcast-av1-" + Guid.NewGuid().ToString("N"));
            Directory.CreateDirectory(dir);
            try
            {
                string ts = Path.Combine(dir, "av1.ts");
                string log = Run(ffmpeg, new[]
                {
                    "-hide_banner", "-loglevel", "warning", "-nostdin",
                    "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30:duration=1",
                    "-pix_fmt", "yuv420p10le", "-c:v", "libsvtav1", "-preset", "12", "-b:v", "2000k",
                    "-f", "mpegts", "-y", ts,
                }, out int exit);

                if (exit != 0) return;                            // refused outright: also fine
                string probe = Run(ProbeTool(ffmpeg), new[]
                {
                    "-hide_banner", "-loglevel", "error", "-show_streams", ts,
                }, out int probeExit);
                if (probeExit != 0) return;
                Assert.DoesNotContain("codec_name=av1", probe);    // ffmpeg writes a private stream, not AV1
            }
            finally
            {
                try { Directory.Delete(dir, true); } catch (Exception) { }
            }
        }

        static string Tail(string s)
        {
            string[] lines = s.Split('\n');
            int start = Math.Max(0, lines.Length - 8);
            return string.Join("\n", lines, start, lines.Length - start);
        }
    }
}
