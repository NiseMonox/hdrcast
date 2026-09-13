using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Text;
using HdrCast.Capture;
using HdrCast.Common;
using HdrCast.Sender.Model;
using Xunit;

namespace HdrCast.Tests
{
    /// <summary>
    /// Publishes to a real MediaMTX with the argument vector and SRT URL this code actually generates, then reads
    /// the stream back. relay/selftest.sh covers the relay with a hand-written command; this covers the part that
    /// can regress silently — <see cref="SrtUrl"/>, <see cref="EncoderProfiles"/> and
    /// <see cref="FfmpegCommandBuilder"/> working together.
    ///
    /// The two Windows named pipes are the only substitution: they are replaced by raw files holding the same
    /// p010le / f32le data the capture engine would have written.
    ///
    /// Opt in with HDRCAST_E2E=1 (needs docker, ffmpeg and a free UDP port); ./build.sh --e2e does that.
    /// </summary>
    public class RelayEndToEndTests
    {
        const int Port = 18899;
        const string Password = "testpw123";
        const string PublishPassphrase = "abcdefghij1234567890";
        const string ReadPassphrase = "klmnopqrst0987654321";
        const string Container = "hdrcast-e2e-test";

        static bool Enabled => Environment.GetEnvironmentVariable("HDRCAST_E2E") == "1";

        static string? Which(string exe)
        {
            string? path = Environment.GetEnvironmentVariable("PATH");
            if (path == null) return null;
            foreach (string dir in path.Split(Path.PathSeparator))
            {
                if (dir.Length == 0) continue;
                string candidate = Path.Combine(dir, exe);
                if (File.Exists(candidate)) return candidate;
            }
            return null;
        }

        static int Run(string exe, IEnumerable<string> args, out string output, int timeoutMs = 120000)
        {
            var psi = new ProcessStartInfo(exe, ArgQuote.Join(args))
            {
                UseShellExecute = false,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
            };
            using (var p = Process.Start(psi)!)
            {
                var sb = new StringBuilder();
                p.OutputDataReceived += (s, e) => { if (e.Data != null) lock (sb) sb.AppendLine(e.Data); };
                p.ErrorDataReceived += (s, e) => { if (e.Data != null) lock (sb) sb.AppendLine(e.Data); };
                p.BeginOutputReadLine();
                p.BeginErrorReadLine();
                if (!p.WaitForExit(timeoutMs))
                {
                    try { p.Kill(true); } catch (Exception) { }
                    lock (sb) output = sb + "\n(timed out)";
                    return -1;
                }
                lock (sb) output = sb.ToString();
                return p.ExitCode;
            }
        }

        static Process Start(string exe, IEnumerable<string> args, string logPath)
        {
            var psi = new ProcessStartInfo(exe, ArgQuote.Join(args))
            {
                UseShellExecute = false,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
            };
            var p = Process.Start(psi)!;
            var writer = new StreamWriter(logPath) { AutoFlush = true };
            p.OutputDataReceived += (s, e) => { if (e.Data != null) lock (writer) writer.WriteLine(e.Data); };
            p.ErrorDataReceived += (s, e) => { if (e.Data != null) lock (writer) writer.WriteLine(e.Data); };
            p.BeginOutputReadLine();
            p.BeginErrorReadLine();
            return p;
        }

        const string MediaMtxConfig = @"logLevel: info
writeQueueSize: 2048
udpMaxPayloadSize: 1452
rtsp: no
rtmp: no
webrtc: no
hls: no
srt: yes
srtAddress: :8890
authMethod: internal
authInternalUsers:
  - user: sender
    pass: {PW}
    permissions: [{action: publish, path: screen}]
  - user: viewer
    pass: {PW}
    permissions: [{action: read, path: screen}]
paths:
  screen:
    source: publisher
    srtPublishPassphrase: {PUB}
    srtReadPassphrase: {READ}
";

        [Fact]
        public void Generated_command_publishes_to_mediamtx_and_reads_back_as_hdr10()
        {
            if (!Enabled) return;
            string? docker = Which("docker");
            string? ffmpeg = Which("ffmpeg");
            string? ffprobe = Which("ffprobe");
            if (docker == null || ffmpeg == null || ffprobe == null) return;

            string dir = Path.Combine(Path.GetTempPath(), "hdrcast-e2e-" + Guid.NewGuid().ToString("N"));
            Directory.CreateDirectory(dir);
            Process? publisher = null;
            try
            {
                string config = Path.Combine(dir, "mediamtx.yml");
                File.WriteAllText(config, MediaMtxConfig
                    .Replace("{PW}", Password)
                    .Replace("{PUB}", PublishPassphrase)
                    .Replace("{READ}", ReadPassphrase));

                Run(docker, new[] { "rm", "-f", Container }, out _, 30000);
                int rc = Run(docker, new[]
                {
                    "run", "-d", "--name", Container,
                    "-p", Port.ToString(CultureInfo.InvariantCulture) + ":8890/udp",
                    "-v", config + ":/mediamtx.yml:ro",
                    "bluenviron/mediamtx:1.21.0",
                }, out string dockerOut, 120000);
                Assert.True(rc == 0, "could not start MediaMTX: " + dockerOut);
                System.Threading.Thread.Sleep(3000);

                // 15 s of synthetic input in exactly the formats the capture engine produces.
                string video = Path.Combine(dir, "src.p010");
                string audio = Path.Combine(dir, "src.f32");
                Assert.Equal(0, Run(ffmpeg, new[]
                {
                    "-hide_banner", "-loglevel", "error", "-f", "lavfi",
                    "-i", "testsrc2=size=1280x720:rate=30:duration=15",
                    "-pix_fmt", "p010le", "-f", "rawvideo", "-y", video,
                }, out _));
                Assert.Equal(0, Run(ffmpeg, new[]
                {
                    "-hide_banner", "-loglevel", "error", "-f", "lavfi",
                    "-i", "sine=frequency=440:duration=15", "-ar", "48000", "-ac", "2",
                    "-f", "f32le", "-y", audio,
                }, out _));

                // The real thing: settings -> SRT URL + encoder profile + muxer options.
                var settings = new SenderSettings
                {
                    Mode = SendMode.Relay, Host = "127.0.0.1", Port = Port, StreamPath = "screen",
                    User = "sender", Password = Password, Passphrase = PublishPassphrase,
                    Encoder = VideoEncoder.LibX265, Quality = QualityPreset.Speed,
                    BitrateMbps = 20, Fps = 30, GopSeconds = 1, BufferSeconds = 2,
                    Audio = true, AudioCodec = AudioCodec.Aac,
                    LatencyMs = SenderSettings.RelayPublishLatencyMs, RateControl = true,
                }.Normalize();
                Assert.Empty(settings.Validate());

                var plan = new FfmpegPlan
                {
                    VideoPipeName = "e2e-video", AudioPipeName = "e2e-audio",
                    PixelFormat = CaptureFormat.P010, Width = 1280, Height = 720, Fps = 30,
                    Encoder = settings.ToEncoderSettings(true, 1000),
                    Audio = AudioCodec.Aac, AudioBitrateKbps = 192,
                    Container = settings.ResolveContainer(),
                    OutputUrl = SrtUrl.Build(settings.ToSrtOptions()),
                    Progress = false,
                };

                // Only the pipe paths are substituted; every other argument is used exactly as generated.
                List<string> args = FfmpegCommandBuilder.Build(plan);
                for (int i = 0; i < args.Count; i++)
                {
                    if (args[i] == FfmpegCommandBuilder.PipePath("e2e-video")) args[i] = video;
                    else if (args[i] == FfmpegCommandBuilder.PipePath("e2e-audio")) args[i] = audio;
                }

                publisher = Start(ffmpeg, args, Path.Combine(dir, "publish.log"));
                System.Threading.Thread.Sleep(3000);

                var readOptions = settings.ToSrtOptions();
                readOptions.Role = SrtRole.Read;
                readOptions.User = "viewer";
                readOptions.Passphrase = ReadPassphrase;
                readOptions.LatencyMs = 1500;
                readOptions.RateControl = false;
                readOptions.IncludeRecvSocketBuffer = true;
                string readUrl = SrtUrl.Build(readOptions);

                int probeRc = Run(ffprobe, new[] { "-v", "error", "-show_streams", readUrl }, out string probe, 60000);
                Assert.True(probeRc == 0, "reader failed: " + probe);

                Assert.Contains("codec_name=hevc", probe);
                Assert.Contains("profile=Main 10", probe);
                Assert.Contains("pix_fmt=yuv420p10le", probe);
                Assert.Contains("color_transfer=smpte2084", probe);
                Assert.Contains("color_primaries=bt2020", probe);
                Assert.Contains("color_space=bt2020nc", probe);
                Assert.Contains("color_range=tv", probe);
                Assert.Contains("codec_name=aac", probe);

                Assert.True(publisher.WaitForExit(60000), "publisher did not finish");
                Assert.True(publisher.ExitCode == 0,
                    "publisher exited " + publisher.ExitCode + ":\n" + File.ReadAllText(Path.Combine(dir, "publish.log")));
            }
            finally
            {
                try { if (publisher != null && !publisher.HasExited) publisher.Kill(true); } catch (Exception) { }
                try { publisher?.Dispose(); } catch (Exception) { }
                string? docker2 = Which("docker");
                if (docker2 != null) Run(docker2, new[] { "rm", "-f", Container }, out _, 60000);
                try { Directory.Delete(dir, true); } catch (Exception) { }
            }
        }
    }
}
