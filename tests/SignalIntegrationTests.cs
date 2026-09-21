using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Net;
using System.Net.Sockets;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using HdrCast.Common;
using HdrCast.Group;
using HdrCast.P2p;
using Xunit;

namespace HdrCast.Tests
{
    /// <summary>Runs signal/hdrcast_signal.py on loopback for the duration of a test. Skipped without python3.</summary>
    public sealed class SignalServer : IDisposable
    {
        readonly Process _p;
        readonly string _state;

        SignalServer(Process p, string state, int httpPort, int echoPort)
        {
            _p = p; _state = state; HttpPort = httpPort; EchoPort = echoPort;
        }

        public int HttpPort { get; }
        public int EchoPort { get; }
        public string Url => "http://127.0.0.1:" + HttpPort;

        public static SignalServer? TryStart()
        {
            string? python = FindOnPath("python3");
            string script = Path.Combine(Vectors.RepoRoot(), "signal", "hdrcast_signal.py");
            if (python == null || !File.Exists(script)) return null;
            int http = FreeTcpPort(), echo = RemuxChild.FreeLoopbackUdpPort();
            string state = Path.Combine(Path.GetTempPath(), "hdrcast-signal-" + Guid.NewGuid().ToString("N"));
            Directory.CreateDirectory(state);
            var psi = new ProcessStartInfo(python, ArgQuote.Join(new[]
            {
                script, "--listen", "127.0.0.1:" + http, "--echo-port", echo.ToString(), "--state-dir", state, "--quiet",
            }))
            { UseShellExecute = false, RedirectStandardOutput = true, RedirectStandardError = true };
            var p = Process.Start(psi)!;
            p.OutputDataReceived += (s, e) => { };
            p.ErrorDataReceived += (s, e) => { };
            p.BeginOutputReadLine();
            p.BeginErrorReadLine();
            var server = new SignalServer(p, state, http, echo);
            var client = new SignalingClient(server.Url, GroupKeys.NewId(), new GroupKeys(GroupKeys.RandomBytes(32)), 1000);
            var sw = Stopwatch.StartNew();
            while (sw.ElapsedMilliseconds < 8000)
            {
                if (p.HasExited) break;
                if (client.Health(CancellationToken.None)) return server;
                Thread.Sleep(100);
            }
            server.Dispose();
            throw new InvalidOperationException("signal server did not come up");
        }

        static int FreeTcpPort()
        {
            var l = new TcpListener(IPAddress.Loopback, 0);
            l.Start();
            int port = ((IPEndPoint)l.LocalEndpoint).Port;
            l.Stop();
            return port;
        }

        static string? FindOnPath(string exe)
        {
            foreach (string dir in (Environment.GetEnvironmentVariable("PATH") ?? "").Split(Path.PathSeparator))
            {
                if (dir.Length == 0) continue;
                string c = Path.Combine(dir, exe);
                if (File.Exists(c)) return c;
            }
            return null;
        }

        public void Dispose()
        {
            try { if (!_p.HasExited) _p.Kill(); } catch (Exception) { }
            try { _p.WaitForExit(3000); } catch (Exception) { }
            _p.Dispose();
            try { Directory.Delete(_state, true); } catch (Exception) { }
        }
    }

    /// <summary>The C# client against the real Python server: the two halves of docs/design.md §8.5.</summary>
    public class SignalingClientIntegrationTests
    {
        static GroupConfig NewGroup(SignalServer server, string me = "tester")
        {
            Invite invite = Invite.Create("integration", server.Url);
            return new GroupConfig().Normalize().Adopt(invite, me);
        }

        [Fact]
        public async Task Register_live_join_answer_round_trip()
        {
            using (SignalServer? server = SignalServer.TryStart())
            {
                if (server == null) return;
                GroupConfig host = NewGroup(server, "小明");
                GroupConfig viewer = new GroupConfig().Normalize().Adopt(host.ToInvite(), "小红");
                GroupKeys keys = host.Keys();
                var hc = new SignalingClient(host);
                var vc = new SignalingClient(viewer);
                CancellationToken ct = CancellationToken.None;

                // nobody has registered the group yet
                var ex404 = Assert.Throws<SignalingException>(() => vc.GetLive(ct));
                Assert.Equal(404, ex404.Status);

                hc.RegisterGroup(ct);
                hc.RegisterGroup(ct);                                          // idempotent
                Assert.Empty(vc.GetLive(ct));

                var rec = new LiveRecord
                {
                    HostId = host.PeerId, Name = host.DisplayName, TimeMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(),
                    SinceMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(), Candidates = { "[2001:db8::1]:5000", "192.168.1.5:5001" },
                    Mbps = 40, MaxViewers = 4,
                };
                hc.PutLive(host.PeerId, SignedEnvelope.Seal(keys, SignedEnvelope.KindLive, host.GroupId, host.PeerId, rec.ToPayload()), ct);
                LiveEntry entry = Assert.Single(vc.GetLive(ct));
                Assert.Equal("小明", entry.Record.Name);
                Assert.Equal(2, entry.Record.Candidates.Count);

                // a record signed with the wrong key is dropped by the reader, even though the server stored it
                string fake = GroupKeys.NewId();
                var forged = SignedEnvelope.Seal(new GroupKeys(GroupKeys.RandomBytes(32)), SignedEnvelope.KindLive, host.GroupId, fake,
                                                 new LiveRecord { HostId = fake, Name = "evil" }.ToPayload());
                hc.PutLive(fake, forged, ct);
                Assert.Single(vc.GetLive(ct));

                // join → long-poll wakes up → answer → long-poll wakes up
                string sid = GroupKeys.NewId();
                var join = new JoinRequest { SessionId = sid, ViewerId = viewer.PeerId, Name = "小红", TimeMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(),
                                             Candidates = { "[2001:db8::2]:6000" }, LatencyMs = 3500, OverheadPercent = 50 };
                Task<List<KeyValuePair<long, SignedEnvelope>>> poll = Task.Run(() => hc.PollJoins(host.PeerId, 0, 10, out _, ct));
                Thread.Sleep(300);
                var sw = Stopwatch.StartNew();
                long seq = vc.PostJoin(host.PeerId, SignedEnvelope.Seal(keys, SignedEnvelope.KindJoin, host.GroupId, host.PeerId, join.ToPayload()), ct);
                Assert.True(seq >= 1);
                var joins = await poll;
                Assert.True(sw.ElapsedMilliseconds < 3000, "the long-poll should wake up at once");
                Assert.True(Assert.Single(joins).Value.TryOpen(keys, SignedEnvelope.KindJoin, host.GroupId, host.PeerId, out var jp));
                Assert.Equal(3500, JoinRequest.FromPayload(jp!)!.LatencyMs);

                Assert.Null(vc.WaitAnswer(sid, 0, ct));                             // nothing yet
                var answer = new JoinAnswer { SessionId = sid, Ok = true, TimeMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(), Candidates = { "[2001:db8::1]:5000" } };
                Task<SignedEnvelope?> wait = Task.Run(() => vc.WaitAnswer(sid, 10, ct));
                Thread.Sleep(300);
                hc.PutAnswer(sid, SignedEnvelope.Seal(keys, SignedEnvelope.KindAnswer, host.GroupId, sid, answer.ToPayload()), ct);
                SignedEnvelope? got = await wait;
                Assert.NotNull(got);
                Assert.True(got!.TryOpen(keys, SignedEnvelope.KindAnswer, host.GroupId, sid, out var ap));
                Assert.True(JoinAnswer.FromPayload(ap!, sid)!.Ok);

                // offline: joins are refused
                hc.DeleteLive(host.PeerId, ct);
                Assert.DoesNotContain(vc.GetLive(ct), e => e.Record.HostId == host.PeerId);
                var exOffline = Assert.Throws<SignalingException>(() =>
                    vc.PostJoin(host.PeerId, SignedEnvelope.Seal(keys, SignedEnvelope.KindJoin, host.GroupId, host.PeerId, join.ToPayload()), ct));
                Assert.Equal(404, exOffline.Status);
            }
        }

        /// <summary>A full live list (32 hosts, 8 candidates each) is ~30 KB of JSON: the reader used to stop at 16 KB.</summary>
        [Fact]
        public void A_full_live_list_is_read_completely()
        {
            using (SignalServer? server = SignalServer.TryStart())
            {
                if (server == null) return;
                GroupConfig g = NewGroup(server);
                GroupKeys keys = g.Keys();
                var c = new SignalingClient(g);
                c.RegisterGroup(CancellationToken.None);
                for (int i = 0; i < 32; i++)
                {
                    string host = GroupKeys.NewId();
                    var rec = new LiveRecord
                    {
                        HostId = host, Name = "host number " + i, TimeMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(),
                        SinceMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(), Mbps = 40, MaxViewers = 4,
                    };
                    for (int k = 0; k < 8; k++) rec.Candidates.Add("[2400:aaaa:bbbb:cccc:1122:33ff:fe44:" + (0x9000 + k).ToString("x") + "]:" + (50000 + k));
                    c.PutLive(host, SignedEnvelope.Seal(keys, SignedEnvelope.KindLive, g.GroupId, host, rec.ToPayload()), CancellationToken.None);
                }
                List<LiveEntry> live = c.GetLive(CancellationToken.None);
                Assert.Equal(32, live.Count);
                Assert.All(live, e => Assert.Equal(8, e.Record.Candidates.Count));
            }
        }

        [Fact]
        public void Wrong_keys_and_unreachable_servers_are_reported_distinctly()
        {
            using (SignalServer? server = SignalServer.TryStart())
            {
                if (server == null) return;
                GroupConfig g = NewGroup(server);
                new SignalingClient(g).RegisterGroup(CancellationToken.None);

                // same group id, different key: the server refuses to re-register it and rejects requests
                var impostor = new SignalingClient(server.Url, g.GroupId, new GroupKeys(GroupKeys.RandomBytes(32)));
                var conflict = Assert.Throws<SignalingException>(() => impostor.RegisterGroup(CancellationToken.None));
                Assert.Equal(409, conflict.Status);
                var denied = Assert.Throws<SignalingException>(() => impostor.GetLive(CancellationToken.None));
                Assert.Equal(401, denied.Status);
            }
            var nowhere = new SignalingClient("http://127.0.0.1:9", GroupKeys.NewId(), new GroupKeys(GroupKeys.RandomBytes(32)), 1500);
            var down = Assert.Throws<SignalingException>(() => nowhere.GetLive(CancellationToken.None));
            Assert.True(down.Unreachable);
        }

        [Fact]
        public void Cancellation_aborts_a_long_poll_quickly()
        {
            using (SignalServer? server = SignalServer.TryStart())
            {
                if (server == null) return;
                GroupConfig g = NewGroup(server);
                var c = new SignalingClient(g);
                c.RegisterGroup(CancellationToken.None);
                using (var cts = new CancellationTokenSource())
                {
                    var sw = Stopwatch.StartNew();
                    Task t = Task.Run(() => c.WaitAnswer(GroupKeys.NewId(), 20, cts.Token));
                    Thread.Sleep(300);
                    cts.Cancel();
                    Assert.ThrowsAny<OperationCanceledException>(() => { try { t.Wait(); } catch (AggregateException ae) { throw ae.InnerException!; } });
                    Assert.True(sw.ElapsedMilliseconds < 5000);
                }
            }
        }

        [Fact]
        public void The_udp_echo_reports_the_sockets_address()
        {
            using (SignalServer? server = SignalServer.TryStart())
            {
                if (server == null) return;
                using (var link = new ViewerLink(new GroupKeys(GroupKeys.RandomBytes(32)), new LinkAddresses { Ipv6 = null, Ipv4Bind = IPAddress.Loopback }))
                {
                    IPEndPoint? seen = link.DiscoverPublicIpv4(new IPEndPoint(IPAddress.Loopback, server.EchoPort), 2000);
                    Assert.NotNull(seen);
                    Assert.Equal(IPAddress.Loopback, seen!.Address);
                    Assert.Equal(link.Ipv4Port, seen.Port);
                }
            }
        }
    }

    /// <summary>
    /// Everything but the GUI and the capture engine: real signaling server, real HEVC Main10 PQ encoder, P2pHost,
    /// two ViewerSessions, ffprobe standing in for mpv. Skipped without python3 / ffmpeg / ffprobe / libx265.
    /// </summary>
    public class P2pFullStackTests
    {
        static string? Find(string exe)
        {
            foreach (string dir in (Environment.GetEnvironmentVariable("PATH") ?? "").Split(Path.PathSeparator))
            {
                if (dir.Length == 0) continue;
                string c = Path.Combine(dir, exe);
                if (File.Exists(c)) return c;
            }
            return null;
        }

        static string Run(string exe, IEnumerable<string> args, int timeoutMs)
        {
            var psi = new ProcessStartInfo(exe, ArgQuote.Join(args)) { UseShellExecute = false, RedirectStandardOutput = true, RedirectStandardError = true };
            using (var p = Process.Start(psi)!)
            {
                Task<string> o = p.StandardOutput.ReadToEndAsync(), e = p.StandardError.ReadToEndAsync();
                if (!p.WaitForExit(timeoutMs)) { try { p.Kill(); } catch (Exception) { } p.WaitForExit(2000); }
                return o.Result + e.Result;
            }
        }

        static bool WaitFor(Func<bool> cond, int ms)
        {
            var sw = Stopwatch.StartNew();
            while (sw.ElapsedMilliseconds < ms) { if (cond()) return true; Thread.Sleep(50); }
            return cond();
        }

        static LinkAddresses Loopback() => new LinkAddresses
        {
            Ipv6 = IPAddress.IPv6Loopback, Ipv4Bind = IPAddress.Loopback,
            FilterV6 = new StatefulFirewall(), FilterV4 = new StatefulFirewall(),
        };

        [Fact]
        public void A_friend_goes_live_and_two_friends_watch_through_the_group()
        {
            string? ffmpeg = Find("ffmpeg"), ffprobe = Find("ffprobe");
            if (ffmpeg == null || ffprobe == null) return;
            if (!Run(ffmpeg, new[] { "-hide_banner", "-encoders" }, 10000).Contains(" libx265 ")) return;
            using (SignalServer? server = SignalServer.TryStart())
            {
                if (server == null) return;
                Invite invite = Invite.Create("full stack", server.Url);
                GroupConfig hostGroup = new GroupConfig().Normalize().Adopt(invite, "小明");
                var logs = new List<string>();
                Action<string> log = line => { lock (logs) logs.Add(line); };

                var encoder = Process.Start(new ProcessStartInfo(ffmpeg, ArgQuote.Join(new[]
                {
                    "-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=30", "-t", "40",
                    "-c:v", "libx265", "-pix_fmt", "yuv420p10le",
                    "-x265-params", "keyint=30:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:log-level=error",
                    "-f", "mpegts", "-mpegts_flags", "+resend_headers", "-muxdelay", "0", "-muxpreload", "0", "pipe:1",
                })) { UseShellExecute = false, RedirectStandardOutput = true, RedirectStandardError = true })!;
                encoder.ErrorDataReceived += (s, e) => { };
                encoder.BeginErrorReadLine();
                var ts = new SwitchableStream();
                ts.Attach(encoder.StandardOutput.BaseStream);

                var host = new P2pHost(new P2pHostOptions
                {
                    Group = hostGroup, FfmpegPath = ffmpeg, BitrateBps = 1_000_000, MaxViewers = 2, Addresses = Loopback(),
                    EchoPort = server.EchoPort, HeartbeatMs = 1000,
                }, ts, log);
                var sessions = new List<ViewerSession>();
                try
                {
                    host.Start();
                    Assert.True(WaitFor(() => host.Status.StartsWith("Live"), 8000), host.Status);

                    ViewerSession Watch(string name)
                    {
                        GroupConfig g = new GroupConfig().Normalize().Adopt(invite, name);
                        var vs = new ViewerSession(new ViewerSessionOptions
                        {
                            Group = g, HostId = hostGroup.PeerId, LatencyMs = 300, OverheadPercent = 25, Addresses = Loopback(),
                            EchoPort = server.EchoPort,
                        }, log);
                        vs.Start();
                        sessions.Add(vs);
                        Assert.True(WaitFor(() => vs.State == ViewerSessionState.Connected, 20000), name + ": " + vs.Status);
                        return vs;
                    }

                    string Probe(ViewerSession vs) => Run(ffprobe, new[]
                    {
                        "-hide_banner", "-v", "error", "-show_entries", "stream=codec_name,profile,color_transfer", "-of", "compact",
                        "srt://127.0.0.1:" + vs.LocalPort + "?passphrase=" + vs.SrtPassphrase + "&pbkeylen=32&latency=300000&connect_timeout=4000",
                    }, 15000);

                    ViewerSession red = Watch("小红"), blue = Watch("小蓝");
                    foreach (string r in new[] { Probe(red), Probe(blue) })
                    {
                        Assert.Contains("codec_name=hevc", r);
                        Assert.Contains("profile=Main 10", r);
                        Assert.Contains("color_transfer=smpte2084", r);
                    }
                    Assert.Contains("IPv6", red.PathDescription);
                    Assert.True(WaitFor(() => host.Viewers.Count(v => v.State == ViewerState.Watching) == 2, 5000));
                    Assert.Contains(host.Viewers, v => v.Name == "小红");
                    Assert.True(host.UploadMbps > 1.5);

                    // a third friend is refused: max viewers is 2
                    GroupConfig g3 = new GroupConfig().Normalize().Adopt(invite, "小绿");
                    var third = new ViewerSession(new ViewerSessionOptions { Group = g3, HostId = hostGroup.PeerId, LatencyMs = 300, Addresses = Loopback(), EchoPort = server.EchoPort }, log);
                    third.Start();
                    sessions.Add(third);
                    Assert.True(WaitFor(() => third.Status.Contains("full"), 15000), third.Status);

                    // the host stops: viewers learn it (BYE) and wait for it to come back
                    host.Stop();
                    Assert.True(WaitFor(() => red.State == ViewerSessionState.Waiting, 5000), red.Status);
                }
                finally
                {
                    foreach (ViewerSession vs in sessions) vs.Dispose();
                    host.Dispose();
                    try { if (!encoder.HasExited) encoder.Kill(); } catch (Exception) { }
                    encoder.Dispose();
                }
            }
        }
    }
}
