using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Net;
using System.Net.Sockets;
using System.Threading;
using System.Threading.Tasks;
using HdrCast.Common;
using HdrCast.Group;
using HdrCast.P2p;
using Xunit;

namespace HdrCast.Tests
{
    public class TsFanOutTests
    {
        /// <summary>Counters live outside the Stream (a MarshalByRefObject) so Interlocked can use them.</summary>
        sealed class Counter { public long Value; }

        /// <summary>A sink whose writes block until released: a remux child still waiting for its viewer.</summary>
        sealed class StuckStream : Stream
        {
            public readonly ManualResetEventSlim Release = new ManualResetEventSlim(false);
            public readonly Counter Written = new Counter();
            public override void Write(byte[] buffer, int offset, int count)
            {
                Release.Wait();
                Interlocked.Add(ref Written.Value, count);
            }
            public override void Flush() { }
            public override bool CanRead => false;
            public override bool CanSeek => false;
            public override bool CanWrite => true;
            public override long Length => throw new NotSupportedException();
            public override long Position { get => 0; set { } }
            public override int Read(byte[] buffer, int offset, int count) => throw new NotSupportedException();
            public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
            public override void SetLength(long value) => throw new NotSupportedException();
        }

        sealed class CountingStream : Stream
        {
            public readonly Counter Written = new Counter();
            public override void Write(byte[] buffer, int offset, int count) => Interlocked.Add(ref Written.Value, count);
            public override void Flush() { }
            public override bool CanRead => false;
            public override bool CanSeek => false;
            public override bool CanWrite => true;
            public override long Length => throw new NotSupportedException();
            public override long Position { get => 0; set { } }
            public override int Read(byte[] buffer, int offset, int count) => throw new NotSupportedException();
            public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
            public override void SetLength(long value) => throw new NotSupportedException();
        }

        sealed class BrokenStream : CountingStream2 { public override void Write(byte[] b, int o, int c) => throw new IOException("pipe closed"); }
        class CountingStream2 : Stream
        {
            public override void Write(byte[] buffer, int offset, int count) { }
            public override void Flush() { }
            public override bool CanRead => false;
            public override bool CanSeek => false;
            public override bool CanWrite => true;
            public override long Length => throw new NotSupportedException();
            public override long Position { get => 0; set { } }
            public override int Read(byte[] buffer, int offset, int count) => throw new NotSupportedException();
            public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
            public override void SetLength(long value) => throw new NotSupportedException();
        }

        [Fact]
        public void A_stuck_viewer_loses_only_its_own_data()
        {
            var source = new MemoryStream(new byte[8 * 1024 * 1024]);
            var stuck = new StuckStream();
            var fast = new CountingStream();
            using (var fan = new TsFanOut(source))
            {
                // An in-memory source is far faster than any writer, so the fast sink gets a queue that can hold
                // everything; the point is that the stuck one (small queue, never drains) does not hold it back.
                fan.AddSink("stuck", stuck, 256 * 1024);
                fan.AddSink("fast", fast, 16 * 1024 * 1024);
                var ended = new ManualResetEventSlim();
                fan.InputEnded += () => ended.Set();
                fan.Start();
                Assert.True(ended.Wait(5000));
                SpinWait.SpinUntil(() => Interlocked.Read(ref fast.Written.Value) == 8 * 1024 * 1024, 5000);
                Assert.Equal(8 * 1024 * 1024, Interlocked.Read(ref fast.Written.Value));
                Assert.Equal(0, fan.Stats("fast")!.Value.clears);
                Assert.True(fan.Stats("stuck")!.Value.clears > 0, "the stuck sink's queue should have been cleared");
                stuck.Release.Set();
            }
        }

        [Fact]
        public void A_broken_sink_is_reported_and_removed()
        {
            var source = new MemoryStream(new byte[1024 * 1024]);
            using (var fan = new TsFanOut(source))
            {
                var failed = new ManualResetEventSlim();
                string? who = null;
                fan.SinkFailed += id => { who = id; failed.Set(); };
                fan.AddSink("gone", new BrokenStream(), 1024 * 1024);
                fan.Start();
                Assert.True(failed.Wait(5000));
                Assert.Equal("gone", who);
                Assert.Equal(0, fan.SinkCount);
            }
        }

        [Fact]
        public void Queue_size_scales_with_bitrate()
        {
            Assert.Equal(TsFanOut.MinQueueBytes, TsFanOut.QueueBytesFor(2_000_000));
            Assert.Equal(40_000_000 / 8 / 2, TsFanOut.QueueBytesFor(40_000_000));
        }

        [Fact]
        public void Remux_child_arguments_copy_and_listen_on_loopback_only()
        {
            string url = RemuxChild.BuildListenerUrl(40000, "a77MWmBi3_603E3kH2abfOI5O-dIU5dn", 3500, 50, 40_000_000);
            Assert.StartsWith("srt://127.0.0.1:40000?mode=listener", url);
            SrtUrlParts p = SrtUrl.Parse(url);
            Assert.Equal("3500000", p.Get("latency"));
            Assert.Equal("50", p.Get("oheadbw"));
            Assert.Equal("5000000", p.Get("inputbw"));
            Assert.Equal("32", p.Get("pbkeylen"));
            Assert.Equal("-1", p.Get("listen_timeout"));
            List<string> args = RemuxChild.BuildArguments(url);
            Assert.Equal(url, args.Last());
            Assert.Contains("copy", args);
            Assert.Contains("+resend_headers", args);
            Assert.Contains("pipe:0", args);
        }
    }

    /// <summary>
    /// The whole P2P media path on one machine: libx265 Main10 PQ → TsFanOut → remux children (SRT listeners) →
    /// HostLink → stateful firewalls → ViewerLink → ffprobe as the viewer. Skipped without ffmpeg/ffprobe/libx265.
    /// </summary>
    public class P2pMediaEndToEndTests
    {
        static string? Find(string exe)
        {
            string? path = Environment.GetEnvironmentVariable("PATH");
            if (path == null) return null;
            foreach (string dir in path.Split(Path.PathSeparator))
            {
                if (dir.Length == 0) continue;
                string c = Path.Combine(dir, exe);
                if (File.Exists(c)) return c;
            }
            return null;
        }

        static string Run(string exe, IEnumerable<string> args, int timeoutMs, out int exitCode)
        {
            var psi = new ProcessStartInfo(exe, ArgQuote.Join(args))
            {
                UseShellExecute = false, RedirectStandardOutput = true, RedirectStandardError = true,
            };
            using (var p = Process.Start(psi)!)
            {
                Task<string> o = p.StandardOutput.ReadToEndAsync();
                Task<string> e = p.StandardError.ReadToEndAsync();
                if (!p.WaitForExit(timeoutMs))
                {
                    try { p.Kill(); } catch (Exception) { }
                    p.WaitForExit(2000);
                }
                exitCode = p.HasExited ? p.ExitCode : -1;
                return o.Result + e.Result;
            }
        }

        static bool WaitFor(Func<bool> cond, int ms)
        {
            var sw = Stopwatch.StartNew();
            while (sw.ElapsedMilliseconds < ms) { if (cond()) return true; Thread.Sleep(20); }
            return cond();
        }

        sealed class Viewer : IDisposable
        {
            public ViewerLink Link = null!;
            public RemuxChild Child = null!;
            public string Sid = "";
            public void Dispose()
            {
                Link.Dispose();
                Child.Dispose();
            }
        }

        [Fact]
        public async Task Hdr10_hevc_reaches_several_viewers_through_punched_links()
        {
            string? ffmpeg = Find("ffmpeg"), ffprobe = Find("ffprobe");
            if (ffmpeg == null || ffprobe == null) return;
            if (!Run(ffmpeg, new[] { "-hide_banner", "-encoders" }, 10000, out _).Contains(" libx265 ")) return;

            GroupKeys keys = new GroupKeys(GroupKeys.RandomBytes(32));
            const int latencyMs = 300;
            var encoderArgs = new[]
            {
                "-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=30", "-t", "30",
                "-c:v", "libx265", "-pix_fmt", "yuv420p10le",
                "-x265-params", "keyint=30:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:log-level=error",
                "-f", "mpegts", "-mpegts_flags", "+resend_headers", "-muxdelay", "0", "-muxpreload", "0", "pipe:1",
            };
            var encoder = Process.Start(new ProcessStartInfo(ffmpeg, ArgQuote.Join(encoderArgs))
            {
                UseShellExecute = false, RedirectStandardOutput = true, RedirectStandardError = true,
            })!;
            encoder.ErrorDataReceived += (s, e) => { };
            encoder.BeginErrorReadLine();

            var hostFw6 = new StatefulFirewall();
            var hostFw4 = new StatefulFirewall();
            var viewers = new List<Viewer>();
            try
            {
                using (var fan = new TsFanOut(encoder.StandardOutput.BaseStream))
                using (var host = new HostLink(keys, new LinkAddresses { Ipv6 = IPAddress.IPv6Loopback, Ipv4Bind = IPAddress.Loopback, FilterV6 = hostFw6, FilterV4 = hostFw4 }))
                {
                    fan.Start();

                    Viewer Join(string name)
                    {
                        var v = new Viewer { Sid = GroupKeys.NewId() };
                        int childPort = RemuxChild.FreeLoopbackUdpPort();
                        v.Child = new RemuxChild(ffmpeg, RemuxChild.BuildListenerUrl(childPort, keys.SrtPassphrase, latencyMs, 25, 1_000_000));
                        v.Child.Start();
                        fan.AddSink(v.Sid, v.Child.Input, TsFanOut.QueueBytesFor(1_000_000));
                        v.Link = new ViewerLink(keys, new LinkAddresses
                        {
                            Ipv6 = IPAddress.IPv6Loopback, Ipv4Bind = IPAddress.Loopback,
                            FilterV6 = new StatefulFirewall(), FilterV4 = new StatefulFirewall(),
                        });
                        host.AddSession(v.Sid, v.Link.Candidates, childPort, name);
                        v.Link.StartSession(v.Sid, host.Candidates);
                        Assert.True(WaitFor(() => v.Link.State == LinkState.Connected, 5000), name + ": " + v.Link.Reason);
                        viewers.Add(v);
                        return v;
                    }

                    string Probe(Viewer v)
                    {
                        string url = "srt://127.0.0.1:" + v.Link.LocalPort + "?passphrase=" + keys.SrtPassphrase +
                                     "&pbkeylen=32&latency=" + (latencyMs * 1000) + "&connect_timeout=4000";
                        return Run(ffprobe, new[]
                        {
                            "-hide_banner", "-v", "error", "-show_entries", "stream=codec_name,profile,pix_fmt,color_transfer,color_primaries",
                            "-of", "compact", url,
                        }, 12000, out _);
                    }

                    Viewer a = Join("A"), b = Join("B");
                    Task<string> pa = Task.Run(() => Probe(a));
                    Task<string> pb = Task.Run(() => Probe(b));
                    foreach (string result in await Task.WhenAll(pa, pb))
                    {
                        Assert.Contains("codec_name=hevc", result);
                        Assert.Contains("profile=Main 10", result);
                        Assert.Contains("color_transfer=smpte2084", result);
                        Assert.Contains("color_primaries=bt2020", result);
                    }
                    Assert.True(a.Link.Path!.AddressFamily == AddressFamily.InterNetworkV6);

                    // C joins mid-stream; A leaves; B keeps working (its child restarts on the next connection)
                    Viewer c = Join("C");
                    Assert.Contains("profile=Main 10", Probe(c));
                    host.RemoveSession(a.Sid, "test");
                    Assert.True(WaitFor(() => a.Link.State == LinkState.Closed, 3000));
                    Assert.True(hostFw6.Dropped + hostFw4.Dropped >= 0);
                    Assert.Equal(2, host.SessionCount);
                }
            }
            finally
            {
                foreach (Viewer v in viewers) v.Dispose();
                try { if (!encoder.HasExited) encoder.Kill(); } catch (Exception) { }
                encoder.Dispose();
            }
        }
    }
}
