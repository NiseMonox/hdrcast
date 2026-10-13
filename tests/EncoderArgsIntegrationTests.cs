using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using HdrCast.Capture;
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
    [Collection("media")]
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

        [Theory]
        [InlineData(VideoEncoder.LibX265)]
        [InlineData(VideoEncoder.HevcNvenc)]
        public void Hdr_colours_come_out_of_the_encoder_unchanged_and_tagged(VideoEncoder encoder)
        {
            // v0.2.1 with ffmpeg 9: the raw input was untagged, so ffmpeg treated it as BT.601 and re-matrixed it to
            // BT.2020 (blue lost 23 luma code values) and NVENC wrote transfer / primaries "unknown": viewers saw the
            // PQ picture as SDR with odd colours. Encodes known HDR10 patches with the sender's own arguments.
            string? ffmpeg = FindFfmpeg();
            if (ffmpeg == null) return;
            if (!HasEncoder(ffmpeg, EncoderProfiles.Info(encoder).FfmpegName)) return;

            const int w = 320, h = 160, pw = 64, ph = 80;          // 5 x 2 patches
            var rgb = new[] { new[] { 255, 255, 255 }, new[] { 128, 128, 128 }, new[] { 255, 0, 0 }, new[] { 0, 255, 0 }, new[] { 0, 0, 255 },
                              new[] { 0, 255, 255 }, new[] { 255, 0, 255 }, new[] { 255, 255, 0 }, new[] { 230, 180, 140 }, new[] { -1, -1, -1 } };
            var expected = new int[rgb.Length][];
            for (int i = 0; i < rgb.Length; i++)
            {
                // an SDR window on an HDR desktop at 200 nit SDR brightness (scRGB 2.5); the last one a 1000 nit highlight
                double[] c = rgb[i][0] < 0 ? new[] { 12.5, 12.5, 12.5 } : Array.ConvertAll(rgb[i], v => ColorMath.SrgbToLinear(v / 255.0) * 2.5);
                ColorMath.ScRgbToYuv10(c[0], c[1], c[2], 80, out int y, out int cb, out int cr);
                expected[i] = new[] { y, cb, cr };
            }
            var frame = new byte[w * h * 3];
            for (int i = 0; i < rgb.Length; i++)
            {
                int x0 = (i % 5) * pw, y0 = (i / 5) * ph;
                for (int y = y0; y < y0 + ph; y++)
                    for (int x = x0; x < x0 + pw; x++)
                        Put(frame, (y * w + x) * 2, expected[i][0]);
                for (int y = y0 / 2; y < (y0 + ph) / 2; y++)
                    for (int x = x0; x < x0 + pw; x += 2)
                    {
                        Put(frame, w * h * 2 + (y * w + x) * 2, expected[i][1]);
                        Put(frame, w * h * 2 + (y * w + x) * 2 + 2, expected[i][2]);
                    }
            }

            string dir = Path.Combine(Path.GetTempPath(), "hdrcast-colour-" + Guid.NewGuid().ToString("N"));
            Directory.CreateDirectory(dir);
            try
            {
                string raw = Path.Combine(dir, "in.p010"), ts = Path.Combine(dir, "out.ts"), back = Path.Combine(dir, "back.p010");
                using (FileStream f = File.Create(raw))
                    for (int n = 0; n < 30; n++) f.Write(frame, 0, frame.Length);

                List<string> args = FfmpegCommandBuilder.Build(new FfmpegPlan
                {
                    VideoPipeName = "colour-test", PixelFormat = HdrCast.Capture.CaptureFormat.P010, Width = w, Height = h, Fps = 30,
                    Encoder = new EncoderSettings { Encoder = encoder, Preset = QualityPreset.Speed, Hdr = true, BitrateKbps = 8000, Fps = 30, GopSeconds = 1 },
                    Audio = AudioCodec.None, Progress = false, OutputUrl = ts, LogLevel = "error",
                });
                args[args.IndexOf(FfmpegCommandBuilder.PipePath("colour-test"))] = raw;
                string log = Run(ffmpeg, args, out int exit);
                if (exit != 0 && encoder != VideoEncoder.LibX265) return;     // no such GPU here
                Assert.True(exit == 0, "encode failed:\n" + ArgQuote.Join(args) + "\n" + Tail(log));

                string probe = Run(ProbeTool(ffmpeg), new[] { "-hide_banner", "-loglevel", "error", "-show_streams", "-select_streams", "v:0", ts }, out int probeExit);
                if (probeExit == 0)
                {
                    Assert.Contains("color_transfer=smpte2084", probe);
                    Assert.Contains("color_primaries=bt2020", probe);
                    Assert.Contains("color_space=bt2020nc", probe);
                    Assert.Contains("color_range=tv", probe);
                }

                Run(ffmpeg, new[] { "-hide_banner", "-loglevel", "error", "-i", ts, "-frames:v", "20", "-f", "rawvideo", "-pix_fmt", "p010le", back }, out int decodeExit);
                Assert.Equal(0, decodeExit);
                byte[] all = File.ReadAllBytes(back);
                int offset = all.Length - frame.Length;                          // the last decoded frame
                for (int i = 0; i < rgb.Length; i++)
                {
                    int cx = (i % 5) * pw + pw / 2, cy = (i / 5) * ph + ph / 2;   // patch centre, away from the edges
                    int y = Get(all, offset + (cy * w + cx) * 2);
                    int cb = Get(all, offset + w * h * 2 + ((cy / 2) * w + cx) * 2);
                    int cr = Get(all, offset + w * h * 2 + ((cy / 2) * w + cx) * 2 + 2);
                    string what = "patch " + i + ": expected " + string.Join("/", expected[i]) + ", got " + y + "/" + cb + "/" + cr;
                    Assert.True(Math.Abs(y - expected[i][0]) <= 3 && Math.Abs(cb - expected[i][1]) <= 3 && Math.Abs(cr - expected[i][2]) <= 3, what);
                }
            }
            finally
            {
                try { Directory.Delete(dir, true); } catch (Exception) { }
            }
        }

        static void Put(byte[] b, int i, int code10)
        {
            ushort s = ColorMath.ToP010Sample(code10);
            b[i] = (byte)s;
            b[i + 1] = (byte)(s >> 8);
        }

        static int Get(byte[] b, int i) => ColorMath.FromP010Sample((ushort)(b[i] | (b[i + 1] << 8)));

        [Fact]
        public void Av1_really_cannot_be_muxed_into_mpegts()
        {
            // This is why HdrCast is HEVC-only: the P2P fan-out carries MPEG-TS. If a future ffmpeg gains
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
