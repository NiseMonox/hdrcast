using System;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Text;
using HdrCast.Capture;
using Xunit;

namespace HdrCast.Tests
{
    /// <summary>
    /// Checks the raw frame layout the capture engine writes against a real ffmpeg decode. The compute shader
    /// and <see cref="ColorMath"/> agree with each other by construction; what they cannot prove on their own is
    /// that "p010le" / "nv12" really mean what the engine assumes (plane order, 10 bits in the HIGH bits,
    /// interleaved UV, limited range). A silent mistake here would show up only as wrong colours on the viewer's
    /// screen, so it is worth an integration test.
    ///
    /// Skipped automatically when ffmpeg is not installed (it is on the Linux build box).
    /// </summary>
    public class P010LayoutTests
    {
        const int Width = 64;
        const int Height = 16;

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

        static string Run(string exe, string args, out int exitCode)
        {
            var psi = new ProcessStartInfo(exe, args)
            {
                UseShellExecute = false,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
            };
            using (var p = Process.Start(psi)!)
            {
                string stdout = p.StandardOutput.ReadToEnd();
                string stderr = p.StandardError.ReadToEnd();
                p.WaitForExit(60000);
                exitCode = p.ExitCode;
                return stdout + stderr;
            }
        }

        /// <summary>Builds a P010 frame whose columns hold known scRGB grey levels.</summary>
        static byte[] BuildP010(double[] scRgbPerColumn, double sdrWhite)
        {
            var frame = new byte[ColorMath.FrameBytes(CaptureFormat.P010, Width, Height)];
            long uv = ColorMath.UvPlaneOffset(CaptureFormat.P010, Width, Height);
            int columnWidth = Width / scRgbPerColumn.Length;

            for (int y = 0; y < Height; y++)
            {
                for (int x = 0; x < Width; x++)
                {
                    double v = scRgbPerColumn[Math.Min(scRgbPerColumn.Length - 1, x / columnWidth)];
                    ColorMath.ScRgbToYuv10(v, v, v, sdrWhite, out int y10, out int cb10, out int cr10);
                    int yOffset = (y * Width + x) * 2;
                    ushort ySample = ColorMath.ToP010Sample(y10);
                    frame[yOffset] = (byte)ySample;
                    frame[yOffset + 1] = (byte)(ySample >> 8);

                    if ((y & 1) == 0 && (x & 1) == 0)
                    {
                        long c = uv + (y / 2) * (long)Width * 2 + x * 2;
                        ushort cb = ColorMath.ToP010Sample(cb10), cr = ColorMath.ToP010Sample(cr10);
                        frame[c] = (byte)cb;
                        frame[c + 1] = (byte)(cb >> 8);
                        frame[c + 2] = (byte)cr;
                        frame[c + 3] = (byte)(cr >> 8);
                    }
                }
            }
            return frame;
        }

        [Fact]
        public void P010_frame_decodes_to_the_expected_luminance()
        {
            string? ffmpeg = FindFfmpeg();
            if (ffmpeg == null) return;   // not available: nothing to check

            // 40, 80, 200 and 1000 nit columns.
            double[] columns = { 0.5, 1.0, 2.5, 12.5 };
            string dir = Path.Combine(Path.GetTempPath(), "hdrcast-p010-" + Guid.NewGuid().ToString("N"));
            Directory.CreateDirectory(dir);
            try
            {
                string raw = Path.Combine(dir, "frame.p010");
                File.WriteAllBytes(raw, BuildP010(columns, 80.0));

                // Decode as p010le tagged PQ/BT.2020 and convert back to linear light with zscale, then read the
                // pixel values out as 16-bit RGB. ffmpeg has to agree with us about the plane layout for this to work.
                string txt = Path.Combine(dir, "out.rgb");
                string args = string.Format(CultureInfo.InvariantCulture,
                    "-hide_banner -loglevel error -y -f rawvideo -pix_fmt p010le -video_size {0}x{1} " +
                    "-color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv -i {2} " +
                    "-frames:v 1 -vf zscale=transferin=smpte2084:primariesin=bt2020:matrixin=bt2020nc:rangein=limited:" +
                    "npl=10000:transfer=linear:primaries=bt709:matrix=gbr:range=full -pix_fmt gbrpf32le -f rawvideo {3}",
                    Width, Height, raw, txt);
                string log = Run(ffmpeg, args, out int exit);
                Assert.True(exit == 0, "ffmpeg failed: " + log);

                byte[] planar = File.ReadAllBytes(txt);
                Assert.Equal(Width * Height * 4 * 3, planar.Length);

                // gbrpf32le: G plane, then B, then R. Read the green channel at the centre of each column.
                int columnWidth = Width / columns.Length;
                for (int i = 0; i < columns.Length; i++)
                {
                    int x = i * columnWidth + columnWidth / 2;
                    int index = (Height / 2 * Width + x) * 4;
                    float green = BitConverter.ToSingle(planar, index);
                    double expectedNits = columns[i] * 80.0;
                    double gotNits = green * 10000.0;      // linear output normalised to npl = 10000
                    Assert.True(Math.Abs(gotNits - expectedNits) <= Math.Max(2.0, expectedNits * 0.03),
                        "column " + i + " (" + columns[i] + " scRGB): expected ~" + expectedNits.ToString("0.#") +
                        " nit, ffmpeg decoded " + gotNits.ToString("0.#") + " nit");
                }
            }
            finally
            {
                try { Directory.Delete(dir, true); } catch (Exception) { }
            }
        }

        [Fact]
        public void Nv12_frame_decodes_to_the_expected_srgb()
        {
            string? ffmpeg = FindFfmpeg();
            if (ffmpeg == null) return;

            var frame = new byte[ColorMath.FrameBytes(CaptureFormat.Nv12, Width, Height)];
            long uv = ColorMath.UvPlaneOffset(CaptureFormat.Nv12, Width, Height);
            double[] columns = { 0.0, 0.25, 0.5, 1.0 };     // sRGB-encoded grey levels
            int columnWidth = Width / columns.Length;
            for (int y = 0; y < Height; y++)
            {
                for (int x = 0; x < Width; x++)
                {
                    double v = columns[Math.Min(columns.Length - 1, x / columnWidth)];
                    ColorMath.SrgbToYuv8(v, v, v, out int y8, out int cb8, out int cr8);
                    frame[y * Width + x] = (byte)y8;
                    if ((y & 1) == 0 && (x & 1) == 0)
                    {
                        long c = uv + (y / 2) * (long)Width + x;
                        frame[c] = (byte)cb8;
                        frame[c + 1] = (byte)cr8;
                    }
                }
            }

            string dir = Path.Combine(Path.GetTempPath(), "hdrcast-nv12-" + Guid.NewGuid().ToString("N"));
            Directory.CreateDirectory(dir);
            try
            {
                string raw = Path.Combine(dir, "frame.nv12");
                File.WriteAllBytes(raw, frame);
                string outRgb = Path.Combine(dir, "out.rgb");
                string args = string.Format(CultureInfo.InvariantCulture,
                    "-hide_banner -loglevel error -y -f rawvideo -pix_fmt nv12 -video_size {0}x{1} " +
                    "-color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv -i {2} " +
                    "-frames:v 1 -pix_fmt rgb24 -f rawvideo {3}", Width, Height, raw, outRgb);
                string log = Run(ffmpeg, args, out int exit);
                Assert.True(exit == 0, "ffmpeg failed: " + log);

                byte[] rgb = File.ReadAllBytes(outRgb);
                Assert.Equal(Width * Height * 3, rgb.Length);
                for (int i = 0; i < columns.Length; i++)
                {
                    int x = i * columnWidth + columnWidth / 2;
                    int index = (Height / 2 * Width + x) * 3;
                    int got = rgb[index + 1];                       // green
                    int expected = (int)Math.Round(columns[i] * 255.0);
                    Assert.True(Math.Abs(got - expected) <= 3,
                        "column " + i + ": expected sRGB ~" + expected + ", ffmpeg decoded " + got);
                }
            }
            finally
            {
                try { Directory.Delete(dir, true); } catch (Exception) { }
            }
        }
    }
}
