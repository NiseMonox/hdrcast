using System;
using System.IO;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using HdrCast.P2p;
using HdrCast.Receiver;
using HdrCast.Sender;
using HdrCast.Sender.Model;
using Xunit;

namespace HdrCast.Tests
{
    public class SwitchableStreamTests
    {
        [Fact]
        public async Task Reads_across_a_source_change_and_ends_only_when_disposed()
        {
            var s = new SwitchableStream();
            var buf = new byte[16];
            Task<int> first = Task.Run(() => s.Read(buf, 0, buf.Length));
            await Task.Delay(100);
            Assert.False(first.IsCompleted);                          // nothing attached yet: waits

            s.Attach(new MemoryStream(Encoding.ASCII.GetBytes("abc")));
            Assert.Equal(3, await first);
            Assert.Equal("abc", Encoding.ASCII.GetString(buf, 0, 3));

            // the first encoder ended: the next Read waits for the restarted one instead of returning 0
            Task<int> second = Task.Run(() => s.Read(buf, 0, buf.Length));
            await Task.Delay(100);
            Assert.False(second.IsCompleted);
            s.Attach(new MemoryStream(Encoding.ASCII.GetBytes("defg")));
            Assert.Equal(4, await second);

            Task<int> third = Task.Run(() => s.Read(buf, 0, buf.Length));
            await Task.Delay(100);
            s.Dispose();
            Assert.Equal(0, await third);
            Assert.Equal(0, s.Read(buf, 0, buf.Length));
        }

        [Fact]
        public async Task A_broken_source_counts_as_ended()
        {
            var s = new SwitchableStream();
            s.Attach(new ThrowingStream());
            var buf = new byte[4];
            Task<int> t = Task.Run(() => s.Read(buf, 0, 4));
            await Task.Delay(100);
            s.Attach(new MemoryStream(new byte[] { 1, 2 }));
            Assert.Equal(2, await t);
        }

        sealed class ThrowingStream : Stream
        {
            public override int Read(byte[] buffer, int offset, int count) => throw new IOException("broken pipe");
            public override bool CanRead => true;
            public override bool CanSeek => false;
            public override bool CanWrite => false;
            public override long Length => 0;
            public override long Position { get => 0; set { } }
            public override void Flush() { }
            public override long Seek(long offset, SeekOrigin origin) => 0;
            public override void SetLength(long value) { }
            public override void Write(byte[] buffer, int offset, int count) { }
        }
    }

    public class ProgressOnStderrTests
    {
        [Theory]
        [InlineData("frame=120", true)]
        [InlineData("out_time_us=4000000", true)]
        [InlineData("stream_0_0_q=23.0", true)]
        [InlineData("progress=continue", true)]
        [InlineData("[hevc_nvenc @ 000001] Lookahead depth set to 20 = too deep", false)]
        [InlineData("Last message repeated 3 times", false)]
        [InlineData("Some Message=with caps", false)]
        [InlineData("=nothing", false)]
        [InlineData("", false)]
        public void Progress_lines_are_told_apart_from_log_messages(string line, bool progress)
        {
            Assert.Equal(progress, FfmpegProgressParser.IsProgressLine(line));
        }

        [Fact]
        public void P2p_plans_send_the_stream_to_stdout_and_progress_to_stderr()
        {
            var plan = new FfmpegPlan
            {
                Width = 1920, Height = 1080, Fps = 60, PixelFormat = HdrCast.Capture.CaptureFormat.P010,
                Encoder = new EncoderSettings { Encoder = VideoEncoder.LibX265, Hdr = true, BitrateKbps = 20000, Fps = 60 },
                OutputUrl = "pipe:1", ProgressUrl = "pipe:2",
            };
            var args = FfmpegCommandBuilder.Build(plan);
            int i = args.IndexOf("-progress");
            Assert.Equal("pipe:2", args[i + 1]);
            Assert.Equal("pipe:1", args[args.Count - 1]);
            Assert.Contains("mpegts", args);
        }

        [Fact]
        public void Sender_settings_need_no_connection_fields()
        {
            var s = new SenderSettings().Normalize();
            Assert.Empty(s.Validate());
            Assert.Equal(4, s.MaxViewers);
            Assert.Equal(16, new SenderSettings { MaxViewers = 99 }.Normalize().MaxViewers);
        }

        [Fact]
        public void Receiver_settings_become_a_local_caller()
        {
            var r = new ReceiverSettings { LatencyMs = 3500 }.Normalize();
            Assert.Empty(r.Validate());
            Assert.Equal(50, r.OverheadPercent);
            string url = HdrCast.Common.SrtUrl.Build(r.ToSrtOptions(40123, "a77MWmBi3_603E3kH2abfOI5O-dIU5dn"));
            Assert.StartsWith("srt://127.0.0.1:40123?mode=caller", url);
            Assert.Contains("latency=3500000", url);
            Assert.Contains("pbkeylen=32", url);
        }
    }

    public class P2pDiagnosticsTests
    {
        [Fact]
        public void Explains_whose_side_lacks_ipv6()
        {
            var hostV6 = new[] { "[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:51234", "192.168.1.5:51235" };
            var hostV4Only = new[] { "192.168.1.5:51235", "203.0.113.7:40000" };
            var hostLanOnly = new[] { "192.168.1.5:51235" };
            var me = new[] { "192.168.5.14:59252" };
            Assert.Contains("This PC has no IPv6", P2pDiagnostics.Explain(false, me, hostV6));
            Assert.Contains("host's PC has no IPv6", P2pDiagnostics.Explain(true, me, hostV4Only));
            Assert.Contains("IPv4 attempt did not get through", P2pDiagnostics.Explain(false, me, hostV4Only));
            Assert.Contains("Neither side has IPv6", P2pDiagnostics.Explain(false, me, hostLanOnly));
            Assert.Contains("path is blocked", P2pDiagnostics.Explain(true, me, hostV6));
        }

        [Fact]
        public void Does_not_call_a_host_ipv6_only_when_its_public_ipv4_was_tried()
        {
            // the host's UDP echo got no answer: it offered IPv6 and its LAN address, so IPv4 was never tried
            var hostNoEcho = new[] { "[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:51234", "192.168.1.3:57546" };
            string noEcho = P2pDiagnostics.Explain(false, new[] { "192.168.5.14:59252" }, hostNoEcho);
            Assert.Contains("the host offered no public IPv4 address", noEcho);

            // with its public mapping the IPv4 attempt did happen and failed
            var hostDual = new[] { "[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:51234", "192.168.1.3:57546", "203.0.113.7:57546" };
            string meBlind = P2pDiagnostics.Explain(false, new[] { "192.168.5.14:59252" }, hostDual);
            Assert.Contains("IPv4 attempt did not get through", meBlind);
            Assert.Contains("this PC's public IPv4 address could not be found", meBlind);
            string bothKnown = P2pDiagnostics.Explain(false, new[] { "192.168.5.14:59252", "198.51.100.9:59252" }, hostDual);
            Assert.Contains("IPv4 attempt did not get through", bothKnown);
            Assert.DoesNotContain("could not be found", bothKnown);
        }

        [Theory]
        [InlineData("203.0.113.7:40000", true)]
        [InlineData("192.168.1.3:57546", false)]
        [InlineData("10.1.2.3:1", false)]
        [InlineData("172.20.0.1:1", false)]
        [InlineData("100.64.0.1:40000", false)]                // shared (CGNAT) range
        [InlineData("169.254.1.1:1", false)]
        [InlineData("[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:51234", false)]
        public void Tells_public_ipv4_candidates_from_private_ones(string candidate, bool isPublic)
        {
            Assert.Equal(isPublic, P2pDiagnostics.HasPublicIpv4(new[] { candidate }));
        }

        [Fact]
        public void Upload_estimate_counts_every_viewer_and_the_retransmission_headroom()
        {
            Assert.Equal(150.0, P2pHost.UploadMbpsFor(40_000_000, 3, 25), 3);
            Assert.Equal(0.0, P2pHost.UploadMbpsFor(40_000_000, 0, 25), 3);
        }
    }
}
