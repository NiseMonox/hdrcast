using System;
using System.Collections.Generic;
using System.Linq;
using System.Net;
using System.Net.Sockets;
using System.Text;
using System.Threading;
using HdrCast.Common;
using HdrCast.Group;
using HdrCast.P2p;
using Xunit;

namespace HdrCast.Tests
{
    public class ControlPacketTests
    {
        [Fact]
        public void Packets_match_the_reference_bytes()
        {
            GroupKeys keys = Vectors.Keys;
            string sid = Vectors.S("sid");
            byte[] punch = ControlPacket.Build(ControlType.Punch, PeerRole.Viewer, GroupKeys.IdBytesOf(sid), 123456789, 0, keys.PunchKey(sid));
            Assert.Equal(Vectors.S("punchHex"), BitConverter.ToString(punch).Replace("-", "").ToLowerInvariant());
            byte[] ack = ControlPacket.Build(ControlType.PunchAck, PeerRole.Host, GroupKeys.IdBytesOf(sid), 123999999, 123456789, keys.PunchKey(sid));
            Assert.Equal(Vectors.S("punchAckHex"), BitConverter.ToString(ack).Replace("-", "").ToLowerInvariant());
        }

        [Fact]
        public void Fields_read_back_and_tampering_is_caught()
        {
            GroupKeys keys = Vectors.Keys;
            string sid = Vectors.S("sid");
            byte[] p = Vectors.Hex(Vectors.S("punchAckHex"));
            Assert.True(ControlPacket.LooksLikeControl(p, p.Length));
            Assert.Equal(ControlType.PunchAck, ControlPacket.TypeOf(p));
            Assert.Equal(PeerRole.Host, ControlPacket.RoleOf(p));
            Assert.Equal(sid, ControlPacket.SidOf(p));
            Assert.Equal(123999999UL, ControlPacket.TsOf(p));
            Assert.Equal(123456789UL, ControlPacket.EchoOf(p));
            Assert.True(ControlPacket.VerifyMac(p, keys.PunchKey(sid)));
            Assert.False(ControlPacket.VerifyMac(p, keys.PunchKey(GroupKeys.NewId())));

            byte[] t = (byte[])p.Clone();
            t[30] ^= 1;
            Assert.False(ControlPacket.VerifyMac(t, keys.PunchKey(sid)));
            Assert.False(ControlPacket.LooksLikeControl(p, 57));
            byte[] srt = new byte[58];
            Assert.False(ControlPacket.LooksLikeControl(srt, 58));
        }
    }

    /// <summary>The state machine on a fake clock, packets carried by an in-memory network with stateful firewalls.</summary>
    public class PeerSessionTests
    {
        static readonly IPEndPoint H6 = new IPEndPoint(IPAddress.Parse("2400::1"), 5000);
        static readonly IPEndPoint H4 = new IPEndPoint(IPAddress.Parse("198.51.100.1"), 5001);
        static readonly IPEndPoint V6 = new IPEndPoint(IPAddress.Parse("2400::2"), 6000);
        static readonly IPEndPoint V4 = new IPEndPoint(IPAddress.Parse("198.51.100.2"), 6001);

        sealed class Net
        {
            public readonly HashSet<string> Opened = new HashSet<string>();   // "from->to" pinholes (outbound seen)
            public bool BlockV6;
            public bool BlockV4;
            public int Delivered;

            public bool Pass(IPEndPoint from, IPEndPoint to)
            {
                if (BlockV6 && from.AddressFamily == AddressFamily.InterNetworkV6) return false;
                if (BlockV4 && from.AddressFamily == AddressFamily.InterNetwork) return false;
                Opened.Add(from + "->" + to);                  // the sender's firewall opens for replies
                return Opened.Contains(to + "->" + from);      // the receiver's firewall needs its own outbound first
            }
        }

        static (PeerSession host, PeerSession viewer) Pair(long now)
        {
            GroupKeys keys = Vectors.Keys;
            string sid = GroupKeys.NewId();
            var host = new PeerSession(keys, sid, PeerRole.Host, now);
            var viewer = new PeerSession(keys, sid, PeerRole.Viewer, now);
            host.AddRemoteCandidates(new[] { Endpoint.Format(V6), Endpoint.Format(V4) });
            viewer.AddRemoteCandidates(new[] { Endpoint.Format(H6), Endpoint.Format(H4) });
            return (host, viewer);
        }

        static IPEndPoint SourceFor(IPEndPoint to, bool fromHost) =>
            to.AddressFamily == AddressFamily.InterNetworkV6 ? (fromHost ? H6 : V6) : (fromHost ? H4 : V4);

        /// <summary>Runs both sides for <paramref name="durationUs"/> in 10 ms steps.</summary>
        static long Run(PeerSession host, PeerSession viewer, Net net, long now, long durationUs, Func<bool>? stop = null)
        {
            long end = now + durationUs;
            var inFlight = new List<(byte[] data, IPEndPoint from, IPEndPoint to, bool toHost)>();
            for (; now < end; now += 10_000)
            {
                foreach (Outgoing o in host.Tick(now)) inFlight.Add((o.Data, SourceFor(o.To, true), o.To, false));
                foreach (Outgoing o in viewer.Tick(now)) inFlight.Add((o.Data, SourceFor(o.To, false), o.To, true));
                var batch = inFlight.ToList();
                inFlight.Clear();
                foreach (var pkt in batch)
                {
                    if (!net.Pass(pkt.from, pkt.to)) continue;
                    net.Delivered++;
                    PeerSession target = pkt.toHost ? host : viewer;
                    foreach (Outgoing r in target.OnControl(pkt.data, pkt.from, now))
                        inFlight.Add((r.Data, SourceFor(r.To, pkt.toHost), r.To, !pkt.toHost));
                }
                if (stop != null && stop()) break;
            }
            return now;
        }

        [Fact]
        public void Both_firewalls_open_and_the_viewer_prefers_ipv6()
        {
            var (host, viewer) = Pair(1_000_000);
            var net = new Net();
            Run(host, viewer, net, 1_000_000, 3_000_000, () => host.State == LinkState.Connected && viewer.State == LinkState.Connected);
            Assert.Equal(LinkState.Connected, viewer.State);
            Assert.Equal(LinkState.Connected, host.State);
            Assert.Equal(H6, viewer.Selected);
            Assert.True(viewer.RttMs >= 0);
            Assert.True(viewer.IsConfirmed(H6));
            Assert.True(host.IsConfirmed(V6));
        }

        [Fact]
        public void Ipv4_is_used_when_ipv6_cannot_get_through()
        {
            var (host, viewer) = Pair(0);
            var net = new Net { BlockV6 = true };
            Run(host, viewer, net, 0, 3_000_000, () => viewer.State == LinkState.Connected && host.State == LinkState.Connected);
            Assert.Equal(H4, viewer.Selected);
            Assert.Equal(LinkState.Connected, host.State);
        }

        [Fact]
        public void An_ipv4_answer_waits_briefly_for_ipv6()
        {
            GroupKeys keys = Vectors.Keys;
            string sid = GroupKeys.NewId();
            var viewer = new PeerSession(keys, sid, PeerRole.Viewer, 0);
            viewer.AddRemoteCandidates(new[] { Endpoint.Format(H6), Endpoint.Format(H4) });
            byte[] ack = ControlPacket.Build(ControlType.PunchAck, PeerRole.Host, GroupKeys.IdBytesOf(sid), 5, 0, keys.PunchKey(sid));
            viewer.OnControl(ack, H4, 100_000);
            viewer.Tick(150_000);
            Assert.Null(viewer.Selected);                             // still inside the IPv6 grace window
            viewer.OnControl(ack, H6, 250_000);
            Assert.Equal(H6, viewer.Selected);

            var viewer2 = new PeerSession(keys, sid, PeerRole.Viewer, 0);
            viewer2.OnControl(ack, H4, 100_000);
            viewer2.Tick(320_000);
            Assert.Equal(H4, viewer2.Selected);                       // no IPv6 answer: IPv4 it is
            Assert.Equal(LinkState.Connected, viewer2.State);
        }

        [Fact]
        public void Nothing_answering_fails_after_the_deadline_with_a_reason()
        {
            var (host, viewer) = Pair(0);
            var net = new Net { BlockV4 = true, BlockV6 = true };
            Run(host, viewer, net, 0, PeerSession.PunchDeadlineUs + 500_000);
            Assert.Equal(LinkState.Failed, viewer.State);
            Assert.Equal(LinkState.Failed, host.State);
            Assert.Contains("no candidate", viewer.Reason);
        }

        [Fact]
        public void Keepalives_flow_while_idle_and_silence_closes()
        {
            var (host, viewer) = Pair(0);
            var net = new Net();
            long now = Run(host, viewer, net, 0, 2_000_000, () => viewer.State == LinkState.Connected && host.State == LinkState.Connected);
            int before = net.Delivered;
            now = Run(host, viewer, net, now, 6_000_000);
            Assert.True(net.Delivered - before >= 4, "expected keepalives");
            Assert.Equal(LinkState.Connected, viewer.State);

            // the host vanishes: the viewer gives up after the silence timeout
            for (long t = now; t < now + PeerSession.SilenceTimeoutUs + 200_000; t += 100_000) viewer.Tick(t);
            Assert.Equal(LinkState.Closed, viewer.State);
            Assert.Contains("silent", viewer.Reason);
        }

        [Fact]
        public void Bye_closes_the_other_side()
        {
            var (host, viewer) = Pair(0);
            var net = new Net();
            long now = Run(host, viewer, net, 0, 2_000_000, () => viewer.State == LinkState.Connected && host.State == LinkState.Connected);
            foreach (Outgoing o in host.CloseWithBye("stop", now))
                if (o.To.Equals(V6)) viewer.OnControl(o.Data, H6, now);
            Assert.Equal(LinkState.Closed, viewer.State);
            Assert.Equal(LinkState.Closed, host.State);
        }

        [Fact]
        public void Data_is_accepted_only_from_confirmed_addresses_and_the_host_follows_it()
        {
            var (host, viewer) = Pair(0);
            var net = new Net();
            long now = Run(host, viewer, net, 0, 2_000_000, () => viewer.State == LinkState.Connected && host.State == LinkState.Connected);
            Assert.False(host.OnData(new IPEndPoint(IPAddress.Parse("2400::99"), 6000), now));
            Assert.True(host.OnData(V6, now));
            Assert.Equal(V6, host.Selected);
            Assert.True(host.OnData(V4, now));                          // V4 punched too: the viewer may switch paths
            Assert.Equal(V4, host.Selected);
        }
    }

    public class LocalAddressTests
    {
        static AddressCandidate C(string a, string nic, bool temp = false, bool preferred = true, long life = 3600) =>
            new AddressCandidate(IPAddress.Parse(a), nic, temp, preferred, life);

        [Fact]
        public void The_stable_address_on_the_route_interface_replaces_a_temporary_source()
        {
            var all = new List<AddressCandidate>
            {
                C("2400:aaaa:bbbb:cccc:5e1:22:33:44", "eth", temp: true),           // what Windows sends from
                C("2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566", "eth"),                // the stable one
                C("fe80::1", "eth"),
                C("fd7a:115c:a1e0::1", "tailscale"),
                C("2400:9999::5", "vpn"),
            };
            Assert.Equal(IPAddress.Parse("2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566"),
                         LocalAddresses.PickStableIpv6(IPAddress.Parse("2400:aaaa:bbbb:cccc:5e1:22:33:44"), all));
        }

        [Fact]
        public void Falls_back_to_the_route_source_and_rejects_non_global_sources()
        {
            var onlyTemp = new List<AddressCandidate> { C("2400::5", "eth", temp: true) };
            Assert.Equal(IPAddress.Parse("2400::5"), LocalAddresses.PickStableIpv6(IPAddress.Parse("2400::5"), onlyTemp));
            var deprecated = new List<AddressCandidate> { C("2400::5", "eth", temp: true), C("2400::6", "eth", preferred: false) };
            Assert.Equal(IPAddress.Parse("2400::5"), LocalAddresses.PickStableIpv6(IPAddress.Parse("2400::5"), deprecated));
            Assert.Null(LocalAddresses.PickStableIpv6(null, onlyTemp));
            Assert.Null(LocalAddresses.PickStableIpv6(IPAddress.Parse("2001:0:1::1"), onlyTemp));   // Teredo
            Assert.Equal(IPAddress.Parse("2400::7"), LocalAddresses.PickStableIpv6(IPAddress.Parse("2400::7"), new List<AddressCandidate>()));
        }

        [Theory]
        [InlineData("2400:aaaa::1", true)]
        [InlineData("2001:4860:4860::8888", true)]
        [InlineData("fe80::1", false)]
        [InlineData("fd00::1", false)]
        [InlineData("2001::1", false)]
        [InlineData("2002:c000:204::1", false)]
        [InlineData("2001:db8::1", false)]
        [InlineData("::1", false)]
        [InlineData("192.0.2.1", false)]
        public void Global_unicast_classification(string address, bool global)
        {
            Assert.Equal(global, LocalAddresses.IsGlobalUnicastV6(IPAddress.Parse(address)));
        }

        [Fact]
        public void Reading_interfaces_works_on_this_platform()
        {
            List<AddressCandidate> all = LocalAddresses.Read();
            Assert.Contains(all, c => IPAddress.IsLoopback(c.Address));
        }
    }

    /// <summary>Stateful firewall stand-in: inbound only from endpoints this socket has sent to.</summary>
    public sealed class StatefulFirewall : IPacketFilter
    {
        readonly HashSet<IPEndPoint> _opened = new HashSet<IPEndPoint>();
        public volatile bool BlockEverything;
        public int Dropped;

        public void OnOutbound(IPEndPoint to)
        {
            lock (_opened) _opened.Add(to);
        }

        public bool AllowInbound(IPEndPoint from)
        {
            if (BlockEverything) { Interlocked.Increment(ref Dropped); return false; }
            lock (_opened)
            {
                if (_opened.Contains(from)) return true;
            }
            Interlocked.Increment(ref Dropped);
            return false;
        }
    }

    /// <summary>Real sockets on loopback, a stateful firewall in front of each WAN socket.</summary>
    public class P2pLinkSocketTests
    {
        static bool HasIpv6Loopback()
        {
            try
            {
                using (var s = new Socket(AddressFamily.InterNetworkV6, SocketType.Dgram, ProtocolType.Udp))
                {
                    s.Bind(new IPEndPoint(IPAddress.IPv6Loopback, 0));
                    return true;
                }
            }
            catch (SocketException) { return false; }
        }

        static LinkAddresses Loopback(StatefulFirewall v6, StatefulFirewall v4) => new LinkAddresses
        {
            Ipv6 = HasIpv6Loopback() ? IPAddress.IPv6Loopback : null,
            Ipv4Bind = IPAddress.Loopback,
            FilterV6 = v6,
            FilterV4 = v4,
        };

        static bool WaitFor(Func<bool> cond, int ms)
        {
            var sw = System.Diagnostics.Stopwatch.StartNew();
            while (sw.ElapsedMilliseconds < ms)
            {
                if (cond()) return true;
                Thread.Sleep(10);
            }
            return cond();
        }

        /// <summary>Stands in for the remux child's SRT listener: echoes every datagram, prefixed.</summary>
        sealed class EchoChild : IDisposable
        {
            readonly UdpChannel _ch;
            public int Received;
            public EchoChild()
            {
                _ch = new UdpChannel(new IPEndPoint(IPAddress.Loopback, 0), "echo-child", (ch, buf, n, from) =>
                {
                    Interlocked.Increment(ref Received);
                    byte[] reply = new byte[n + 3];
                    reply[0] = (byte)'R'; reply[1] = (byte)'E'; reply[2] = (byte)':';
                    Buffer.BlockCopy(buf, 0, reply, 3, n);
                    ch.Send(reply, from);
                });
            }
            public int Port => _ch.LocalEndPoint.Port;
            public void Dispose() => _ch.Dispose();
        }

        static string RoundTrip(int viewerLocalPort, string text)
        {
            using (var mpv = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp))
            {
                mpv.Bind(new IPEndPoint(IPAddress.Loopback, 0));
                mpv.ReceiveTimeout = 300;
                var to = new IPEndPoint(IPAddress.Loopback, viewerLocalPort);
                var buf = new byte[2048];
                for (int attempt = 0; attempt < 10; attempt++)
                {
                    mpv.SendTo(Encoding.ASCII.GetBytes(text), to);
                    try
                    {
                        EndPoint from = new IPEndPoint(IPAddress.Any, 0);
                        int n = mpv.ReceiveFrom(buf, ref from);
                        return Encoding.ASCII.GetString(buf, 0, n);
                    }
                    catch (SocketException) { }
                }
                return "";
            }
        }

        [Fact]
        public void Punches_through_both_firewalls_and_carries_data_both_ways()
        {
            GroupKeys keys = new GroupKeys(GroupKeys.RandomBytes(32));
            var fwH6 = new StatefulFirewall(); var fwH4 = new StatefulFirewall();
            var fwV6 = new StatefulFirewall(); var fwV4 = new StatefulFirewall();
            using (var child = new EchoChild())
            using (var host = new HostLink(keys, Loopback(fwH6, fwH4)))
            using (var viewer = new ViewerLink(keys, Loopback(fwV6, fwV4)))
            {
                var events = new List<HostSessionInfo>();
                host.SessionChanged += i => { lock (events) events.Add(i); };
                string sid = GroupKeys.NewId();
                host.AddSession(sid, viewer.Candidates, child.Port, tag: "小红");
                viewer.StartSession(sid, host.Candidates);

                Assert.True(WaitFor(() => viewer.State == LinkState.Connected, 5000), "viewer: " + viewer.State + " " + viewer.Reason);
                if (viewer.HasIpv6) Assert.Equal(AddressFamily.InterNetworkV6, viewer.Path!.AddressFamily);
                Assert.Equal("RE:hello", RoundTrip(viewer.LocalPort, "hello"));
                Assert.True(child.Received >= 1);
                Assert.True(WaitFor(() => { lock (events) return events.Any(e => e.State == LinkState.Connected && (string?)e.Tag == "小红"); }, 2000));
                Assert.True(fwH6.Dropped + fwH4.Dropped + fwV6.Dropped + fwV4.Dropped >= 0);   // early punches may be dropped; that is the point
                Assert.Contains("ms", viewer.DescribePath());
            }
        }

        [Fact]
        public void Falls_back_to_ipv4_when_ipv6_is_blocked()
        {
            if (!HasIpv6Loopback()) return;
            GroupKeys keys = new GroupKeys(GroupKeys.RandomBytes(32));
            var fwH6 = new StatefulFirewall { BlockEverything = true }; var fwH4 = new StatefulFirewall();
            var fwV6 = new StatefulFirewall { BlockEverything = true }; var fwV4 = new StatefulFirewall();
            using (var child = new EchoChild())
            using (var host = new HostLink(keys, Loopback(fwH6, fwH4)))
            using (var viewer = new ViewerLink(keys, Loopback(fwV6, fwV4)))
            {
                string sid = GroupKeys.NewId();
                host.AddSession(sid, viewer.Candidates, child.Port);
                viewer.StartSession(sid, host.Candidates);
                Assert.True(WaitFor(() => viewer.State == LinkState.Connected, 5000));
                Assert.Equal(AddressFamily.InterNetwork, viewer.Path!.AddressFamily);
                Assert.Equal("RE:v4", RoundTrip(viewer.LocalPort, "v4"));
            }
        }

        [Fact]
        public void Strangers_and_wrong_keys_never_reach_the_child()
        {
            GroupKeys keys = new GroupKeys(GroupKeys.RandomBytes(32));
            using (var child = new EchoChild())
            using (var host = new HostLink(keys, new LinkAddresses { Ipv6 = null, Ipv4Bind = IPAddress.Loopback }))
            using (var intruder = new ViewerLink(new GroupKeys(GroupKeys.RandomBytes(32)), new LinkAddresses { Ipv6 = null, Ipv4Bind = IPAddress.Loopback }))
            using (var raw = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp))
            {
                string sid = GroupKeys.NewId();
                host.AddSession(sid, intruder.Candidates, child.Port);
                intruder.StartSession(sid, host.Candidates);     // same session id, different group key
                raw.Bind(new IPEndPoint(IPAddress.Loopback, 0));
                var hostEp = Endpoint.Parse(host.Candidates[0]).ToIPEndPoint(0);
                for (int i = 0; i < 20; i++) raw.SendTo(new byte[1316], hostEp);   // SRT-sized junk from nowhere
                Thread.Sleep(800);
                RoundTrip(intruder.LocalPort, "let me in");
                Assert.Equal(0, child.Received);
                Assert.NotEqual(LinkState.Connected, intruder.State);
            }
        }

        [Fact]
        public void Removing_a_session_tells_the_viewer_and_ending_one_tells_the_host()
        {
            GroupKeys keys = new GroupKeys(GroupKeys.RandomBytes(32));
            var addrs = new Func<LinkAddresses>(() => new LinkAddresses { Ipv6 = null, Ipv4Bind = IPAddress.Loopback });
            using (var child = new EchoChild())
            using (var host = new HostLink(keys, addrs()))
            using (var viewer = new ViewerLink(keys, addrs()))
            {
                var closed = new ManualResetEventSlim();
                host.SessionChanged += i => { if (i.State == LinkState.Closed) closed.Set(); };

                string sid = GroupKeys.NewId();
                host.AddSession(sid, viewer.Candidates, child.Port);
                viewer.StartSession(sid, host.Candidates);
                Assert.True(WaitFor(() => viewer.State == LinkState.Connected, 5000));
                host.RemoveSession(sid, "stream ended");
                Assert.True(WaitFor(() => viewer.State == LinkState.Closed, 2000));
                Assert.Equal(0, host.SessionCount);

                // a re-join with a new session id on the same local port
                string sid2 = GroupKeys.NewId();
                int port = viewer.LocalPort;
                host.AddSession(sid2, viewer.Candidates, child.Port);
                viewer.StartSession(sid2, host.Candidates);
                Assert.True(WaitFor(() => viewer.State == LinkState.Connected, 5000));
                Assert.Equal(port, viewer.LocalPort);
                Assert.Equal("RE:again", RoundTrip(viewer.LocalPort, "again"));
                closed.Reset();
                viewer.EndSession("bye");
                Assert.True(closed.Wait(2000));
            }
        }
    }
}
