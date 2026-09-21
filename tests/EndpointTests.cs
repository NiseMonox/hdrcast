using System;
using System.IO;
using HdrCast.Common;
using HdrCast.Receiver;
using HdrCast.Sender.Model;
using Xunit;

namespace HdrCast.Tests
{
    public class EndpointTests
    {
        [Theory]
        [InlineData("relay.example.net", "relay.example.net", null, EndpointKind.Hostname)]
        [InlineData("relay.example.net:9000", "relay.example.net", 9000, EndpointKind.Hostname)]
        [InlineData("192.168.1.201", "192.168.1.201", null, EndpointKind.IPv4)]
        [InlineData("192.168.1.201:8890", "192.168.1.201", 8890, EndpointKind.IPv4)]
        [InlineData("2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566", "2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566", null, EndpointKind.IPv6)]
        [InlineData("[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]", "2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566", null, EndpointKind.IPv6)]
        [InlineData("[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:8890", "2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566", 8890, EndpointKind.IPv6)]
        [InlineData("2001:DB8:0:0:0:0:0:1", "2001:db8::1", null, EndpointKind.IPv6)]          // canonical compressed form
        [InlineData("::1", "::1", null, EndpointKind.IPv6)]
        [InlineData("[::1]:5000", "::1", 5000, EndpointKind.IPv6)]
        [InlineData("::ffff:192.0.2.7", "192.0.2.7", null, EndpointKind.IPv4)]              // IPv4-mapped → IPv4
        [InlineData("  \"[2001:db8::1]:8890\"  ", "2001:db8::1", 8890, EndpointKind.IPv6)]  // quotes and spaces
        [InlineData("「2001:db8::1」", "2001:db8::1", null, EndpointKind.IPv6)]
        [InlineData("srt://[2001:db8::1]:8890?streamid=read:screen", "2001:db8::1", 8890, EndpointKind.IPv6)]
        [InlineData("srt://relay.example.net:8890?latency=1500000", "relay.example.net", 8890, EndpointKind.Hostname)]
        [InlineData("１９２．１６８．１．２０１：８８９０", "192.168.1.201", 8890, EndpointKind.IPv4)]   // typed with a CJK IME
        [InlineData("192。168。1。201", "192.168.1.201", null, EndpointKind.IPv4)]
        [InlineData("［2001:db8::1］：８８９０", "2001:db8::1", 8890, EndpointKind.IPv6)]
        [InlineData("relay.example.net.", "relay.example.net", null, EndpointKind.Hostname)]
        public void Accepts_what_people_type(string input, string host, int? port, EndpointKind kind)
        {
            Assert.True(Endpoint.TryParse(input, out HostPort? hp, out string error), error);
            Assert.Equal(host, hp!.Host);
            Assert.Equal(port, hp.Port);
            Assert.Equal(kind, hp.Kind);
        }

        [Theory]
        [InlineData("", "empty")]
        [InlineData("   ", "empty")]
        [InlineData("fe80::1", "link-local")]
        [InlineData("fe80::1%12", "zone")]
        [InlineData("[fe80::1%eth0]:8890", "zone")]
        [InlineData("ff02::1", "multicast")]
        [InlineData("::", "connect")]
        [InlineData("0.0.0.0", "connect")]
        [InlineData("[2001:db8::1", "closing")]
        [InlineData("[2001:db8::1]8890", "port")]
        [InlineData("[192.168.1.1]:8890", "brackets")]
        [InlineData("192.168.1", "IPv4")]
        [InlineData("192.168.1.256", "IPv4")]
        [InlineData("192.168.01.1", "IPv4")]
        [InlineData("relay:0", "port")]
        [InlineData("relay:70000", "port")]
        [InlineData("relay:abc", "port")]
        [InlineData("under_score.example", "host name")]
        [InlineData("2001:db8::g", "IPv6")]
        public void Rejects_what_cannot_work_with_a_readable_reason(string input, string reasonFragment)
        {
            Assert.False(Endpoint.TryParse(input, out _, out string error));
            Assert.Contains(reasonFragment, error, StringComparison.OrdinalIgnoreCase);
        }

        [Fact]
        public void Unspecified_addresses_are_allowed_only_when_asked()
        {
            Assert.True(Endpoint.TryParse("::", out HostPort? a, out _, allowUnspecified: true));
            Assert.Equal("::", a!.Host);
            Assert.True(Endpoint.TryParse("0.0.0.0", out _, out _, allowUnspecified: true));
        }

        [Fact]
        public void A_bare_ipv6_address_never_loses_its_last_group_to_a_port()
        {
            // "2400::1:8890" is the IPv6 address 2400::1:8890, not host 2400::1 port 8890; a port needs brackets.
            Assert.True(Endpoint.TryParse("2400::1:8890", out HostPort? hp, out _));
            Assert.Equal("2400::1:8890", hp!.Host);
            Assert.Null(hp.Port);
        }

        [Fact]
        public void Formatting_brackets_ipv6_only()
        {
            Assert.Equal("[2001:db8::1]", Endpoint.FormatHost("2001:db8::1"));
            Assert.Equal("[2001:db8::1]", Endpoint.FormatHost("[2001:db8::1]"));
            Assert.Equal("1.2.3.4", Endpoint.FormatHost("1.2.3.4"));
            Assert.Equal("[2001:db8::1]:8890", Endpoint.Parse("2001:db8::1").ToAuthority(8890));
            Assert.Equal("relay:9000", Endpoint.Parse("relay:9000").ToAuthority(8890));
            Assert.Equal("[2001:db8::1]:8890", Endpoint.Format(new System.Net.IPEndPoint(System.Net.IPAddress.Parse("2001:db8::1"), 8890)));
            Assert.Equal("192.0.2.7:40", Endpoint.Format(new System.Net.IPEndPoint(System.Net.IPAddress.Parse("::ffff:192.0.2.7"), 40)));
        }
    }

    public class SrtUrlIpv6Tests
    {
        static SrtUrlOptions Caller(string host, int port = 8890) => new SrtUrlOptions
        {
            Topology = SrtTopology.DirectCaller, Role = SrtRole.Read, Host = host, Port = port, LatencyMs = 1500,
        };

        [Theory]
        [InlineData("2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566", "srt://[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:8890?")]
        [InlineData("[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]", "srt://[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:8890?")]
        [InlineData("[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:8890", "srt://[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:8890?")]
        [InlineData("192.168.1.201:8890", "srt://192.168.1.201:8890?")]
        [InlineData("relay.example.net:8890", "srt://relay.example.net:8890?")]
        public void Every_way_of_typing_the_host_gives_a_valid_url(string host, string expectedStart)
        {
            string url = SrtUrl.Build(Caller(host));
            Assert.StartsWith(expectedStart, url);
            SrtUrlParts parts = SrtUrl.Parse(url);
            Assert.Equal(8890, parts.Port);
        }

        [Fact]
        public void A_port_in_the_host_that_disagrees_with_the_port_box_is_an_error()
        {
            Assert.Throws<ArgumentException>(() => SrtUrl.Build(Caller("[2001:db8::1]:9000", 8890)));
            Assert.False(SrtUrl.TryCallerHost("[2001:db8::1]:9000", 8890, out _, out string error));
            Assert.Contains("9000", error);
        }

        [Fact]
        public void Listener_binds_ipv4_by_default_and_a_concrete_ipv6_address_when_given()
        {
            var o = new SrtUrlOptions { Topology = SrtTopology.DirectListener, Port = 9000, LatencyMs = 1500 };
            Assert.StartsWith("srt://0.0.0.0:9000?", SrtUrl.Build(o));
            o.ListenAddress = "2001:db8::5";
            Assert.StartsWith("srt://[2001:db8::5]:9000?", SrtUrl.Build(o));
            o.ListenAddress = "127.0.0.1";
            Assert.StartsWith("srt://127.0.0.1:9000?", SrtUrl.Build(o));
        }

        [Theory]
        [InlineData("::")]
        [InlineData("[::]")]
        public void Listener_refuses_the_ipv6_wildcard_with_an_explanation(string address)
        {
            // Verified with ffmpeg 7.1.5 + libsrt 1.5.4: "Operation not supported: Bad parameters".
            Assert.False(SrtUrl.TryListenHost(address, out _, out string error));
            Assert.Contains("::", error);
            Assert.Throws<ArgumentException>(() => SrtUrl.Build(new SrtUrlOptions
            {
                Topology = SrtTopology.DirectListener, Port = 9000, LatencyMs = 1500, ListenAddress = address,
            }));
        }

        [Fact]
        public void Listener_address_must_be_a_literal_without_port()
        {
            Assert.False(SrtUrl.TryListenHost("my-pc.local", out _, out _));
            Assert.False(SrtUrl.TryListenHost("[2001:db8::5]:9000", out _, out _));
        }

        [Fact]
        public void Parse_insists_on_brackets_for_ipv6()
        {
            Assert.Throws<FormatException>(() => SrtUrl.Parse("srt://2001:db8::1:8890?mode=caller"));
            SrtUrlParts p = SrtUrl.Parse("srt://[2001:db8::1]:8890?mode=caller");
            Assert.Equal("2001:db8::1", p.Host);
            Assert.Equal(8890, p.Port);
        }

        [Fact]
        public void Settings_validation_explains_bad_hosts_and_keeps_listen_address_separate()
        {
            var r = new ReceiverSettings { Host = "fe80::1", Port = 8890, Mode = ConnectionMode.DirectDial }.Normalize();
            Assert.Contains(r.Validate(), p => p.Contains("link-local"));

            // Switching to listen mode must not reinterpret the relay host as a bind address.
            r = new ReceiverSettings { Host = "relay.example.net", Port = 8890, Mode = ConnectionMode.DirectListen }.Normalize();
            Assert.Empty(r.Validate());
            Assert.StartsWith("srt://0.0.0.0:8890?", SrtUrl.Build(r.ToSrtOptions()));
            r.ListenAddress = "::";
            Assert.Contains(r.Validate(), p => p.Contains("'::'"));

            var s = new SenderSettings { Host = "[2001:db8::1]:8890", Port = 8890, Mode = SendMode.DirectDial }.Normalize();
            Assert.Empty(s.Validate());
            Assert.StartsWith("srt://[2001:db8::1]:8890?", SrtUrl.Build(s.ToSrtOptions()));
            s.Mode = SendMode.DirectListen;
            s.ListenAddress = "2001:db8::2";
            Assert.Empty(s.Validate());
            Assert.StartsWith("srt://[2001:db8::2]:8890?", SrtUrl.Build(s.ToSrtOptions()));
        }
    }

    public class InviteRedactionTests
    {
        [Fact]
        public void Invite_codes_never_reach_a_log()
        {
            const string code = "hdrcast:eyJ2IjoxLCJzIjoiaHR0cDovLzEwMS40My4yMy45NDoxODY1MCJ9.TaKdIhZQ";
            string line = "[app] imported " + code + " from the clipboard";
            string safe = SecretRedactor.Redact(line);
            Assert.DoesNotContain("eyJ2", safe);
            Assert.Contains("hdrcast:" + SecretRedactor.Mask + " from the clipboard", safe);
            Assert.Equal("HDRCAST:" + SecretRedactor.Mask, SecretRedactor.Redact("HDRCAST:abc.def"));
            Assert.Equal("see hdrcast: docs", SecretRedactor.Redact("see hdrcast: docs"));
        }
    }

    public class ScriptHygieneTests
    {
        static string RepoRoot()
        {
            var dir = new DirectoryInfo(AppContext.BaseDirectory);
            while (dir != null && !File.Exists(Path.Combine(dir.FullName, "build.sh"))) dir = dir.Parent;
            return dir?.FullName ?? "";
        }

        static string[] WindowsScripts(string root)
        {
            string scripts = Path.Combine(root, "scripts");
            if (!Directory.Exists(scripts)) return new string[0];
            var list = new System.Collections.Generic.List<string>();
            foreach (string pattern in new[] { "*.cmd", "*.bat", "*.ps1" }) list.AddRange(Directory.GetFiles(scripts, pattern));
            return list.ToArray();
        }

        /// <summary>cmd.exe seeks to :labels; in an LF file a label straddling a 512-byte block can be missed.</summary>
        [Fact]
        public void Windows_scripts_use_crlf_only()
        {
            string root = RepoRoot();
            if (root.Length == 0) return;
            foreach (string f in WindowsScripts(root))
            {
                byte[] b = File.ReadAllBytes(f);
                for (int i = 0; i < b.Length; i++)
                {
                    if (b[i] == (byte)'\n')
                        Assert.True(i > 0 && b[i - 1] == (byte)'\r', Path.GetFileName(f) + " has a bare LF at byte " + i);
                }
            }
        }

        /// <summary>Windows PowerShell 5.1 reads a BOM-less script in the ANSI code page: a UTF-8 em dash becomes a quote.</summary>
        [Fact]
        public void Windows_scripts_are_pure_ascii()
        {
            string root = RepoRoot();
            if (root.Length == 0) return;
            foreach (string f in WindowsScripts(root))
            {
                byte[] b = File.ReadAllBytes(f);
                for (int i = 0; i < b.Length; i++)
                    Assert.True(b[i] < 0x80, Path.GetFileName(f) + " has a non-ASCII byte at " + i);
            }
        }

        [Fact]
        public void Gitattributes_pins_crlf_for_windows_scripts()
        {
            string root = RepoRoot();
            if (root.Length == 0) return;
            string text = File.ReadAllText(Path.Combine(root, ".gitattributes"));
            Assert.Matches(@"(?m)^\*\.cmd\s+.*eol=crlf", text);
            Assert.Matches(@"(?m)^\*\.ps1\s+.*eol=crlf", text);
        }
    }
}
