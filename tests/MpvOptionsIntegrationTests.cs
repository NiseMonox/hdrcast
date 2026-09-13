using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using HdrCast.Receiver;
using Xunit;

namespace HdrCast.Tests
{
    /// <summary>
    /// Checks that every option the receiver passes to mpv actually exists. A typo here does not fail a unit
    /// test — it makes mpv exit immediately on the viewer's machine with "Option not found", which is a bad
    /// way to find out. The Linux box has mpv 0.40; option names are shared across platforms.
    ///
    /// Skipped when mpv is not installed.
    /// </summary>
    public class MpvOptionsIntegrationTests
    {
        static string? FindMpv()
        {
            string? path = Environment.GetEnvironmentVariable("PATH");
            if (path == null) return null;
            foreach (string dir in path.Split(Path.PathSeparator))
            {
                if (dir.Length == 0) continue;
                string candidate = Path.Combine(dir, "mpv");
                if (File.Exists(candidate)) return candidate;
            }
            return null;
        }

        static HashSet<string> KnownOptions(string mpv)
        {
            var psi = new ProcessStartInfo(mpv, "--list-options")
            {
                UseShellExecute = false,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
            };
            var names = new HashSet<string>(StringComparer.Ordinal);
            using (var p = Process.Start(psi)!)
            {
                string output = p.StandardOutput.ReadToEnd();
                p.WaitForExit(30000);
                foreach (string line in output.Split('\n'))
                {
                    string t = line.TrimStart();
                    if (!t.StartsWith("--", StringComparison.Ordinal)) continue;
                    int end = t.IndexOfAny(new[] { ' ', '\t', '=' }, 2);
                    string name = end > 0 ? t.Substring(2, end - 2) : t.Substring(2);
                    if (name.Length > 0) names.Add(name.Trim());
                }
            }
            return names;
        }

        static IEnumerable<ReceiverSettings> Variants()
        {
            foreach (DisplayMode display in new[] { DisplayMode.Auto, DisplayMode.HdrPassthrough, DisplayMode.SdrToneMap })
            {
                foreach (HwDecMode hwdec in new[] { HwDecMode.AutoSafe, HwDecMode.D3d11va, HwDecMode.D3d11vaCopy, HwDecMode.No })
                {
                    foreach (bool fullscreen in new[] { false, true })
                    {
                        var s = new ReceiverSettings
                        {
                            Mode = ConnectionMode.Relay,
                            Host = "relay.example.net",
                            Port = 8890,
                            StreamPath = "screen",
                            User = "viewer",
                            Password = "pw",
                            Passphrase = "0123456789abcdef",
                            LatencyMs = 1500,
                            ExpectedBitrateMbps = 60,
                            DisplayMode = display,
                            HwDec = hwdec,
                            Fullscreen = fullscreen,
                            FullscreenScreen = 1,
                            TargetPeakNits = display == DisplayMode.SdrToneMap ? 400 : 0,
                        };
                        yield return s.Normalize();
                    }
                }
            }
        }

        [Fact]
        public void Every_generated_mpv_option_exists()
        {
            string? mpv = FindMpv();
            if (mpv == null) return;
            HashSet<string> known = KnownOptions(mpv);
            Assert.True(known.Count > 100, "could not read mpv's option list");

            var unknown = new SortedSet<string>(StringComparer.Ordinal);
            foreach (ReceiverSettings s in Variants())
            {
                MpvLaunchPlan plan = MpvCommandBuilder.Build(s, 1234, "mpv", "ffmpeg");
                foreach (string arg in plan.MpvArgs)
                {
                    if (!arg.StartsWith("--", StringComparison.Ordinal)) continue;
                    int eq = arg.IndexOf('=');
                    string name = eq > 0 ? arg.Substring(2, eq - 2) : arg.Substring(2);
                    // mpv accepts --no-<flag> for every boolean option.
                    string bare = name.StartsWith("no-", StringComparison.Ordinal) ? name.Substring(3) : name;
                    if (!known.Contains(name) && !known.Contains(bare)) unknown.Add(name);
                }
            }
            Assert.True(unknown.Count == 0, "mpv does not know these options: " + string.Join(", ", unknown));
        }

        [Fact]
        public void Mpv_accepts_the_generated_command_line()
        {
            string? mpv = FindMpv();
            if (mpv == null) return;

            // Point mpv at a file that does not exist: option parsing still happens first, so a bad option or a
            // bad value is reported, while the missing file is an expected, harmless failure.
            var s = new ReceiverSettings
            {
                Mode = ConnectionMode.Relay, Host = "127.0.0.1", Port = 8890, StreamPath = "screen",
                User = "viewer", Password = "pw", Passphrase = "0123456789abcdef",
                LatencyMs = 1500, ExpectedBitrateMbps = 60, DisplayMode = DisplayMode.HdrPassthrough,
            }.Normalize();
            MpvLaunchPlan plan = MpvCommandBuilder.Build(s, 4321, mpv);

            var args = new List<string>();
            foreach (string a in plan.MpvArgs)
            {
                if (a.StartsWith("srt://", StringComparison.Ordinal)) continue;   // do not actually connect
                if (a.StartsWith("--input-ipc-server=", StringComparison.Ordinal)) continue;
                if (a.StartsWith("--vo=", StringComparison.Ordinal)) continue;    // no GPU/display on this box
                if (a.StartsWith("--gpu-api=", StringComparison.Ordinal)) continue;
                if (a.StartsWith("--hwdec=", StringComparison.Ordinal)) continue;
                args.Add(a);
            }
            args.Add("--vo=null");
            args.Add("--ao=null");
            args.Add("/nonexistent/hdrcast-option-check.ts");

            var psi = new ProcessStartInfo(mpv, HdrCast.Common.ArgQuote.Join(args))
            {
                UseShellExecute = false,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
            };
            using (var p = Process.Start(psi)!)
            {
                string output = p.StandardOutput.ReadToEnd() + p.StandardError.ReadToEnd();
                p.WaitForExit(30000);
                Assert.DoesNotContain("Option not found", output);
                Assert.DoesNotContain("is not a valid value", output);
                Assert.DoesNotContain("Error parsing option", output);
                Assert.DoesNotContain("Unknown option", output);
            }
        }
    }
}
