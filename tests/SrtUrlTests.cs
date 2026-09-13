using System;
using System.Collections.Generic;
using HdrCast.Common;
using Xunit;

namespace HdrCast.Tests
{
    public class SrtUrlTests
    {
        static SrtUrlOptions Relay(SrtRole role) => new SrtUrlOptions
        {
            Topology = SrtTopology.Relay,
            Role = role,
            Host = "relay.example.net",
            Port = 8890,
            StreamPath = "screen",
            User = "sender",
            Password = "pw",
            Passphrase = "0123456789abcdef",
            LatencyMs = 1500,
            BitrateBps = 40_000_000,
        };

        [Fact]
        public void Relay_publish_has_streamid_and_sndbuf()
        {
            string url = SrtUrl.Build(Relay(SrtRole.Publish));
            SrtUrlParts p = SrtUrl.Parse(url);
            Assert.Equal("relay.example.net", p.Host);
            Assert.Equal(8890, p.Port);
            Assert.Equal("publish:screen:sender:pw", SrtUrl.Decode(p.Get("streamid")!));
            Assert.True(p.Has("sndbuf"));
            Assert.False(p.Has("rcvbuf"));
            Assert.False(p.Has("recv_buffer_size"));
        }

        [Fact]
        public void Relay_read_has_rcvbuf_and_recv_buffer_size()
        {
            string url = SrtUrl.Build(Relay(SrtRole.Read));
            SrtUrlParts p = SrtUrl.Parse(url);
            Assert.Equal("read:screen:sender:pw", SrtUrl.Decode(p.Get("streamid")!));
            Assert.True(p.Has("rcvbuf"));
            Assert.Equal(SrtBufferSizing.RecvSocketBufferBytes.ToString(), p.Get("recv_buffer_size"));
        }

        [Fact]
        public void Latency_is_expressed_in_microseconds()
        {
            // ffmpeg's srt protocol takes latency in microseconds (libsrt.c divides by 1000 before SRTO_LATENCY).
            string url = SrtUrl.Build(Relay(SrtRole.Publish));
            Assert.Equal("1500000", SrtUrl.Parse(url).Get("latency"));
        }

        [Fact]
        public void Passphrase_adds_pbkeylen()
        {
            SrtUrlParts p = SrtUrl.Parse(SrtUrl.Build(Relay(SrtRole.Publish)));
            Assert.Equal("0123456789abcdef", SrtUrl.Decode(p.Get("passphrase")!));
            Assert.Equal("32", p.Get("pbkeylen"));
        }

        [Fact]
        public void No_passphrase_means_no_encryption_parameters()
        {
            SrtUrlOptions o = Relay(SrtRole.Publish);
            o.Passphrase = null;
            SrtUrlParts p = SrtUrl.Parse(SrtUrl.Build(o));
            Assert.False(p.Has("passphrase"));
            Assert.False(p.Has("pbkeylen"));
        }

        [Theory]
        [InlineData("short")]              // < 10 characters
        [InlineData("")]
        public void Invalid_passphrase_is_rejected(string passphrase)
        {
            SrtUrlOptions o = Relay(SrtRole.Publish);
            o.Passphrase = passphrase;
            if (passphrase.Length == 0)
            {
                // empty means "no encryption", which is allowed
                Assert.DoesNotContain("passphrase", SrtUrl.Build(o));
            }
            else
            {
                Assert.Throws<ArgumentException>(() => SrtUrl.Build(o));
            }
        }

        [Fact]
        public void Listener_uses_wildcard_host_and_infinite_listen_timeout()
        {
            SrtUrlOptions o = Relay(SrtRole.Publish);
            o.Topology = SrtTopology.DirectListener;
            SrtUrlParts p = SrtUrl.Parse(SrtUrl.Build(o));
            Assert.Equal(SrtUrl.ListenerHost, p.Host);
            Assert.Equal("listener", p.Get("mode"));
            Assert.Equal("-1", p.Get("listen_timeout"));
            Assert.False(p.Has("streamid"));     // direct mode has no MediaMTX stream id
            Assert.False(p.Has("connect_timeout"));
        }

        [Fact]
        public void Caller_mode_keeps_host_and_connect_timeout()
        {
            SrtUrlOptions o = Relay(SrtRole.Publish);
            o.Topology = SrtTopology.DirectCaller;
            SrtUrlParts p = SrtUrl.Parse(SrtUrl.Build(o));
            Assert.Equal("caller", p.Get("mode"));
            Assert.Equal("relay.example.net", p.Host);
            Assert.Equal("5000", p.Get("connect_timeout"));
        }

        [Fact]
        public void Rate_control_emits_maxbw_inputbw_oheadbw()
        {
            SrtUrlOptions o = Relay(SrtRole.Publish);
            o.RateControl = true;
            o.OverheadPercent = 50;
            SrtUrlParts p = SrtUrl.Parse(SrtUrl.Build(o));
            Assert.Equal("0", p.Get("maxbw"));
            Assert.Equal((40_000_000 / 8).ToString(), p.Get("inputbw"));   // inputbw is bytes/second
            Assert.Equal("50", p.Get("oheadbw"));
        }

        [Fact]
        public void Special_characters_in_credentials_are_percent_encoded()
        {
            SrtUrlOptions o = Relay(SrtRole.Publish);
            o.Password = "a&b c#d%e";
            o.Passphrase = "pass&phrase=with#specials";
            string url = SrtUrl.Build(o);
            Assert.DoesNotContain("a&b", url);
            Assert.DoesNotContain("#", url);
            SrtUrlParts p = SrtUrl.Parse(url);
            Assert.Equal("publish:screen:sender:a&b c#d%e", SrtUrl.Decode(p.Get("streamid")!));
            Assert.Equal("pass&phrase=with#specials", SrtUrl.Decode(p.Get("passphrase")!));
        }

        [Fact]
        public void Stream_id_components_must_not_contain_colon()
        {
            Assert.Throws<ArgumentException>(() => SrtUrl.BuildStreamId(SrtRole.Publish, "scr:een", null, null));
            Assert.Throws<ArgumentException>(() => SrtUrl.BuildStreamId(SrtRole.Read, "screen", "us:er", "pw"));
        }

        [Fact]
        public void Stream_id_without_credentials_is_two_parts()
        {
            Assert.Equal("read:screen", SrtUrl.BuildStreamId(SrtRole.Read, "screen", null, null));
        }

        [Fact]
        public void Ipv6_host_is_bracketed()
        {
            SrtUrlOptions o = Relay(SrtRole.Read);
            o.Host = "2001:db8::1";
            Assert.StartsWith("srt://[2001:db8::1]:8890?", SrtUrl.Build(o));
        }

        [Fact]
        public void Pkt_size_is_1316_so_srt_payload_is_seven_ts_packets()
        {
            Assert.Equal("1316", SrtUrl.Parse(SrtUrl.Build(Relay(SrtRole.Publish))).Get("pkt_size"));
            Assert.Equal(0, SrtBufferSizing.PayloadBytes % 188);
        }
    }

    public class SrtBufferSizingTests
    {
        [Fact]
        public void Buffer_covers_latency_times_bitrate()
        {
            // 50 Mbps for 2 s = 100 Mbit = 12.5 MB ≈ 9500 packets of 1316 bytes.
            SrtBufferPlan plan = SrtBufferSizing.Compute(2000, 50_000_000);
            Assert.InRange(plan.Packets, 9000, 10000);
            Assert.True(plan.FlowWindow >= plan.Packets, "flow window must cover the packets in flight");
            Assert.True(plan.BufferBytes >= (long)plan.Packets * SrtBufferSizing.PayloadBytes);
        }

        [Fact]
        public void Flow_window_never_below_libsrt_default()
        {
            SrtBufferPlan plan = SrtBufferSizing.Compute(200, 1_000_000);
            Assert.True(plan.FlowWindow >= SrtBufferSizing.DefaultFlowWindow);
        }

        [Fact]
        public void Buffer_bytes_stay_within_flow_window_capacity()
        {
            // libsrt requires SRTO_RCVBUF <= FC * (MSS - 28); violating it makes srt_setsockopt fail.
            foreach (int latency in new[] { 500, 1500, 3500 })
            {
                foreach (long bitrate in new long[] { 10_000_000, 50_000_000, 100_000_000 })
                {
                    SrtBufferPlan plan = SrtBufferSizing.Compute(latency, bitrate);
                    Assert.True(plan.BufferBytes <= (long)plan.FlowWindow * SrtBufferSizing.PacketSlotBytes,
                                latency + " ms / " + bitrate + " bps exceeds the flow window capacity");
                }
            }
        }

        [Fact]
        public void Higher_latency_or_bitrate_needs_more_buffer()
        {
            Assert.True(SrtBufferSizing.Compute(3000, 50_000_000).Packets > SrtBufferSizing.Compute(1500, 50_000_000).Packets);
            Assert.True(SrtBufferSizing.Compute(1500, 100_000_000).Packets > SrtBufferSizing.Compute(1500, 50_000_000).Packets);
        }
    }

    public class ArgQuoteTests
    {
        [Theory]
        [InlineData("simple", "simple")]
        [InlineData("with space", "\"with space\"")]
        [InlineData("", "\"\"")]
        [InlineData("has\"quote", "\"has\\\"quote\"")]
        [InlineData(@"C:\path\to\file", @"C:\path\to\file")]
        [InlineData(@"C:\path with space\", "\"C:\\path with space\\\\\"")]
        public void Quotes_windows_style(string input, string expected)
        {
            Assert.Equal(expected, ArgQuote.Quote(input));
        }

        [Fact]
        public void Join_separates_with_spaces()
        {
            Assert.Equal("a \"b c\" d", ArgQuote.Join(new[] { "a", "b c", "d" }));
        }

        [Fact]
        public void Srt_url_with_ampersands_is_quoted_for_cmd()
        {
            string url = "srt://h:8890?streamid=publish:screen&latency=1500000";
            string quoted = ArgQuote.Quote(url);
            Assert.StartsWith("\"", quoted);
            Assert.EndsWith("\"", quoted);
        }
    }

    public class ToolLocatorTests
    {
        [Fact]
        public void Prefers_app_directory_then_tools_then_path()
        {
            var existing = new HashSet<string>(StringComparer.OrdinalIgnoreCase)
            {
                "/app/tools/ffmpeg.exe",
                "/somewhere/ffmpeg.exe",
            };
            string? found = ToolLocator.Find("ffmpeg.exe", "/app", "/somewhere", p => existing.Contains(p));
            Assert.Equal("/app/tools/ffmpeg.exe".Replace('/', System.IO.Path.DirectorySeparatorChar),
                         found!.Replace('/', System.IO.Path.DirectorySeparatorChar));
        }

        [Fact]
        public void Returns_null_when_nothing_exists()
        {
            Assert.Null(ToolLocator.Find("ffmpeg.exe", "/app", "/somewhere", p => false));
        }

        [Fact]
        public void Candidates_start_with_the_app_directory()
        {
            var list = new List<string>(ToolLocator.Candidates("mpv.exe", "/app", null));
            Assert.NotEmpty(list);
            Assert.Contains("mpv.exe", list[0]);
        }
    }
}
