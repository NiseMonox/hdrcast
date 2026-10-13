using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Linq;
using System.Net;
using System.Net.Sockets;
using System.Text;
using System.Text.RegularExpressions;
using System.Threading;
using HdrCast.Common;
using HdrCast.Group;
using HdrCast.P2p;
using Xunit;

namespace HdrCast.Tests
{
    /// <summary>
    /// A UPnP Internet Gateway Device stand-in on loopback: answers SSDP searches (sent to <see cref="SsdpEndPoint"/>
    /// instead of the multicast group) and the WANPPPConnection SOAP actions, and records what it was asked.
    /// </summary>
    public sealed class FakeRouter : IDisposable
    {
        public const string ServiceType = "urn:schemas-upnp-org:service:WANPPPConnection:1";

        readonly Socket _ssdp;
        readonly TcpListener _http;
        readonly Thread _ssdpThread;
        readonly Thread _httpThread;
        volatile bool _closed;

        public FakeRouter()
        {
            _ssdp = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp);
            _ssdp.Bind(new IPEndPoint(IPAddress.Loopback, 0));
            _http = new TcpListener(IPAddress.Loopback, 0);
            _http.Start();
            _ssdpThread = new Thread(SsdpLoop) { IsBackground = true, Name = "fake-ssdp" };
            _httpThread = new Thread(HttpLoop) { IsBackground = true, Name = "fake-igd" };
            _ssdpThread.Start();
            _httpThread.Start();
        }

        public volatile string ExternalIp = "203.0.113.7";
        /// <summary>Returns (code, description) to fault an AddPortMapping, or null to accept it.</summary>
        public Func<IDictionary<string, string>, Tuple<int, string>?>? RefuseAdd;
        public readonly List<string> Calls = new List<string>();
        /// <summary>External port → "client:internalPort lease".</summary>
        public readonly Dictionary<int, string> Mappings = new Dictionary<int, string>();
        public int Searches;

        public IPEndPoint SsdpEndPoint => (IPEndPoint)_ssdp.LocalEndPoint!;
        int HttpPort => ((IPEndPoint)_http.LocalEndpoint).Port;

        public PortMappingOptions Options(int renewSeconds = 600) => new PortMappingOptions
        {
            SsdpTarget = SsdpEndPoint, Bind = IPAddress.Loopback, SearchTimeoutMs = 1500, CallTimeoutMs = 2000,
            RetrySeconds = 1, RenewSeconds = renewSeconds,
        };

        public int Count(string action)
        {
            lock (Calls) return Calls.Count(c => c.StartsWith(action + " ", StringComparison.Ordinal));
        }

        void SsdpLoop()
        {
            var buf = new byte[2048];
            while (!_closed)
            {
                EndPoint from = new IPEndPoint(IPAddress.Any, 0);
                int n;
                try { n = _ssdp.ReceiveFrom(buf, ref from); }
                catch (Exception) { if (_closed) return; continue; }
                string text = Encoding.ASCII.GetString(buf, 0, n);
                if (!text.StartsWith("M-SEARCH", StringComparison.Ordinal)) continue;
                Interlocked.Increment(ref Searches);
                Match st = Regex.Match(text, @"ST: *(\S+)");
                string reply = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nEXT:\r\nLOCATION: http://127.0.0.1:" + HttpPort +
                               "/desc.xml\r\nSERVER: Fake/1.0 UPnP/1.0\r\nST: " + st.Groups[1].Value + "\r\nUSN: uuid:fake\r\n\r\n";
                try { _ssdp.SendTo(Encoding.ASCII.GetBytes(reply), from); } catch (Exception) { }
            }
        }

        void HttpLoop()
        {
            while (!_closed)
            {
                TcpClient c;
                try { c = _http.AcceptTcpClient(); }
                catch (Exception) { return; }
                try
                {
                    using (c)
                    using (NetworkStream s = c.GetStream())
                        Serve(s);
                }
                catch (Exception) { }
            }
        }

        void Serve(NetworkStream s)
        {
            s.ReadTimeout = 3000;
            var head = new StringBuilder();
            while (!head.ToString().EndsWith("\r\n\r\n", StringComparison.Ordinal))
            {
                int b = s.ReadByte();
                if (b < 0) return;
                head.Append((char)b);
            }
            string[] lines = head.ToString().Split(new[] { "\r\n" }, StringSplitOptions.None);
            string[] request = lines[0].Split(' ');
            int length = 0;
            string action = "";
            foreach (string l in lines)
            {
                if (l.StartsWith("Content-Length:", StringComparison.OrdinalIgnoreCase)) length = int.Parse(l.Substring(15).Trim(), CultureInfo.InvariantCulture);
                if (l.StartsWith("SOAPAction:", StringComparison.OrdinalIgnoreCase)) action = l.Substring(l.IndexOf('#') + 1).Trim().Trim('"');
            }
            var body = new byte[length];
            for (int got = 0; got < length;)
            {
                int n = s.Read(body, got, length - got);
                if (n <= 0) break;
                got += n;
            }

            if (request[0] == "GET" && request[1] == "/desc.xml") { Reply(s, 200, Description()); return; }
            if (request[0] != "POST" || request[1] != "/ctl/wan") { Reply(s, 404, ""); return; }

            var args = new Dictionary<string, string>(StringComparer.Ordinal);
            foreach (Match m in Regex.Matches(Encoding.UTF8.GetString(body), @"<(New\w+)>([^<]*)</\1>")) args[m.Groups[1].Value] = m.Groups[2].Value;
            switch (action)
            {
                case "GetExternalIPAddress":
                    lock (Calls) Calls.Add("GetExternalIPAddress ");
                    Reply(s, 200, Soap(action, "<NewExternalIPAddress>" + ExternalIp + "</NewExternalIPAddress>"));
                    return;
                case "AddPortMapping":
                    lock (Calls) Calls.Add("AddPortMapping " + args["NewExternalPort"] + "->" + args["NewInternalClient"] + ":" + args["NewInternalPort"] + " lease " + args["NewLeaseDuration"]);
                    Tuple<int, string>? refuse = RefuseAdd?.Invoke(args);
                    if (refuse != null) { Reply(s, 500, Fault(refuse.Item1, refuse.Item2)); return; }
                    lock (Mappings) Mappings[int.Parse(args["NewExternalPort"], CultureInfo.InvariantCulture)] = args["NewInternalClient"] + ":" + args["NewInternalPort"] + " " + args["NewLeaseDuration"];
                    Reply(s, 200, Soap(action, ""));
                    return;
                case "DeletePortMapping":
                    lock (Calls) Calls.Add("DeletePortMapping " + args["NewExternalPort"]);
                    bool had;
                    lock (Mappings) had = Mappings.Remove(int.Parse(args["NewExternalPort"], CultureInfo.InvariantCulture));
                    Reply(s, had ? 200 : 500, had ? Soap(action, "") : Fault(714, "NoSuchEntryInArray"));
                    return;
                default:
                    Reply(s, 500, Fault(401, "Invalid Action"));
                    return;
            }
        }

        static string Description() =>
            "<?xml version=\"1.0\"?><root xmlns=\"urn:schemas-upnp-org:device-1-0\"><specVersion><major>1</major><minor>0</minor></specVersion>" +
            "<device><deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType><friendlyName>Fake Router</friendlyName>" +
            "<modelName>FR-100</modelName><deviceList><device><deviceType>urn:schemas-upnp-org:device:WANDevice:1</deviceType>" +
            "<serviceList><service><serviceType>urn:schemas-upnp-org:service:WANCommonInterfaceConfig:1</serviceType>" +
            "<controlURL>/ctl/common</controlURL></service></serviceList><deviceList><device>" +
            "<deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</deviceType><serviceList><service>" +
            "<serviceType>" + ServiceType + "</serviceType><serviceId>urn:upnp-org:serviceId:WANPPPConn1</serviceId>" +
            "<controlURL>/ctl/wan</controlURL><eventSubURL>/evt/wan</eventSubURL><SCPDURL>/wan.xml</SCPDURL>" +
            "</service></serviceList></device></deviceList></device></deviceList></device></root>";

        static string Soap(string action, string inner) =>
            "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" " +
            "s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:" + action + "Response xmlns:u=\"" + ServiceType + "\">" +
            inner + "</u:" + action + "Response></s:Body></s:Envelope>";

        static string Fault(int code, string description) =>
            "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" " +
            "s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><s:Fault><faultcode>s:Client</faultcode>" +
            "<faultstring>UPnPError</faultstring><detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>" + code +
            "</errorCode><errorDescription>" + description + "</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>";

        static void Reply(NetworkStream s, int status, string body)
        {
            byte[] b = Encoding.UTF8.GetBytes(body);
            string head = "HTTP/1.1 " + status + (status == 200 ? " OK" : status == 404 ? " Not Found" : " Internal Server Error") +
                          "\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nContent-Length: " + b.Length + "\r\nConnection: close\r\n\r\n";
            byte[] h = Encoding.ASCII.GetBytes(head);
            s.Write(h, 0, h.Length);
            s.Write(b, 0, b.Length);
        }

        public void Dispose()
        {
            _closed = true;
            try { _ssdp.Close(); } catch (Exception) { }
            try { _http.Stop(); } catch (Exception) { }
            _ssdpThread.Join(1000);
            _httpThread.Join(1000);
        }
    }

    public class UpnpMessageTests
    {
        [Fact]
        public void An_ssdp_answer_gives_the_description_url_and_anything_else_is_ignored()
        {
            Uri? u = UpnpClient.ParseSearchResponse(
                "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nlocation: http://192.168.1.1:2869/upnp/790a830.xml\r\nST: upnp:rootdevice\r\n\r\n");
            Assert.Equal("http://192.168.1.1:2869/upnp/790a830.xml", u!.ToString());
            Assert.Null(UpnpClient.ParseSearchResponse("NOTIFY * HTTP/1.1\r\nLOCATION: http://192.168.1.1/x.xml\r\n\r\n"));
            Assert.Null(UpnpClient.ParseSearchResponse("HTTP/1.1 200 OK\r\nST: upnp:rootdevice\r\n\r\n"));
            Assert.Null(UpnpClient.ParseSearchResponse("HTTP/1.1 200 OK\r\nLOCATION: file:///etc/passwd\r\n\r\n"));
        }

        [Fact]
        public void The_wan_connection_services_come_out_of_the_description_with_absolute_control_urls()
        {
            string xml =
                "<?xml version=\"1.0\"?><root xmlns=\"urn:schemas-upnp-org:device-1-0\"><device><modelName>XG-100NE</modelName><serviceList>" +
                "<service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType><controlURL>/l3f</controlURL></service>" +
                "<service><serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType><controlURL>/upnp/control/0a1b2c3d</controlURL></service>" +
                "<service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:2</serviceType><controlURL>http://192.168.1.1:5000/ip</controlURL></service>" +
                "</serviceList></device></root>";
            List<UpnpGateway> list = UpnpClient.ParseDescription(xml, new Uri("http://192.168.1.1:2869/upnp/790a830.xml"));
            Assert.Equal(2, list.Count);
            Assert.Equal("http://192.168.1.1:2869/upnp/control/0a1b2c3d", list[0].ControlUrl.ToString());
            Assert.Equal("urn:schemas-upnp-org:service:WANPPPConnection:1", list[0].ServiceType);
            Assert.Equal("XG-100NE", list[0].Name);
            Assert.Equal("http://192.168.1.1:5000/ip", list[1].ControlUrl.ToString());

            // URLBase, when present, is what relative control URLs hang off
            string withBase = xml.Replace("<device>", "<URLBase>http://192.168.1.254:80/</URLBase><device>");
            Assert.Equal("http://192.168.1.254/upnp/control/0a1b2c3d",
                         UpnpClient.ParseDescription(withBase, new Uri("http://192.168.1.1:2869/d.xml"))[0].ControlUrl.ToString());
        }

        [Fact]
        public void A_description_with_a_dtd_is_read_without_resolving_it()
        {
            // any device on the LAN can answer the search: its DTD and external entities are never followed
            string body = "<root><device><modelName>M</modelName><serviceList><service><serviceType>" +
                          "urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>/c</controlURL></service></serviceList></device></root>";
            string dtd = "<?xml version=\"1.0\"?><!DOCTYPE root [<!ENTITY x SYSTEM \"file:///c:/windows/win.ini\">]>";
            Assert.Single(UpnpClient.ParseDescription(dtd + body, new Uri("http://10.0.0.1/d.xml")));
            Assert.Throws<UpnpException>(() => UpnpClient.ParseDescription(dtd + body.Replace(">M<", ">&x;<"), new Uri("http://10.0.0.1/d.xml")));
        }

        [Fact]
        public void Requests_are_escaped_and_answers_and_faults_are_read()
        {
            string env = UpnpClient.BuildEnvelope("urn:schemas-upnp-org:service:WANIPConnection:1", "AddPortMapping",
                new List<KeyValuePair<string, string>> { new KeyValuePair<string, string>("NewPortMappingDescription", "A & <B>") });
            Assert.Contains("<u:AddPortMapping xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\">", env);
            Assert.Contains("<NewPortMappingDescription>A &amp; &lt;B&gt;</NewPortMappingDescription>", env);

            var answer = UpnpClient.ParseResponse(
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:GetExternalIPAddressResponse xmlns:u=\"x\">" +
                "<NewExternalIPAddress>203.0.113.65</NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>", "GetExternalIPAddress");
            Assert.Equal("203.0.113.65", answer["NewExternalIPAddress"]);
            Assert.Throws<UpnpException>(() => UpnpClient.ParseResponse("<a/>", "GetExternalIPAddress"));

            UpnpException fault = UpnpClient.ParseFault(
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><s:Fault><detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">" +
                "<errorCode>718</errorCode><errorDescription>ConflictInMappingEntry</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>",
                500, "AddPortMapping");
            Assert.Equal(718, fault.Code);
            Assert.Contains("ConflictInMappingEntry", fault.Message);
            Assert.Equal(0, UpnpClient.ParseFault("not xml", 500, "AddPortMapping").Code);
        }
    }

    [Collection("upnp")]
    public class RouterPortMappingTests
    {
        static bool WaitFor(Func<bool> cond, int ms)
        {
            var sw = Stopwatch.StartNew();
            while (sw.ElapsedMilliseconds < ms) { if (cond()) return true; Thread.Sleep(20); }
            return cond();
        }

        [Fact]
        public void Forwards_the_port_reports_the_internet_address_and_removes_it_on_dispose()
        {
            using (var router = new FakeRouter())
            {
                var log = new List<string>();
                int changes = 0;
                var m = new RouterPortMapping(50331, router.Options(), l => { lock (log) log.Add(l); });
                m.Changed += () => Interlocked.Increment(ref changes);
                m.Start();
                Assert.True(WaitFor(() => m.State == PortMappingState.Mapped, 8000), m.State + " " + m.Problem);
                Assert.True(m.Open);
                Assert.Equal(new IPEndPoint(IPAddress.Parse("203.0.113.7"), 50331), m.External);
                Assert.Equal("FR-100", m.RouterName);
                Assert.True(changes >= 1);
                lock (router.Mappings) Assert.Equal("127.0.0.1:50331 3600", router.Mappings[50331]);
                lock (log) Assert.Contains(log, l => l.Contains("UDP 50331 forwarded to this PC") && l.Contains("203.0.113.7:50331"));

                m.Dispose();
                lock (router.Mappings) Assert.Empty(router.Mappings);
                Assert.Equal(1, router.Count("DeletePortMapping"));
                Assert.Equal(PortMappingState.Off, m.State);
            }
        }

        [Fact]
        public void The_lease_is_renewed_while_it_lives()
        {
            using (var router = new FakeRouter())
            using (var m = new RouterPortMapping(40000, router.Options(renewSeconds: 1), _ => { }))
            {
                m.Start();
                Assert.True(WaitFor(() => router.Count("AddPortMapping") >= 3, 8000), "renewals: " + router.Count("AddPortMapping"));
                Assert.Equal(PortMappingState.Mapped, m.State);
            }
        }

        [Fact]
        public void A_port_taken_by_another_device_is_forwarded_from_another_outside_port()
        {
            using (var router = new FakeRouter())
            {
                router.RefuseAdd = a => a["NewExternalPort"] == "50331" ? Tuple.Create(718, "ConflictInMappingEntry") : null;
                using (var m = new RouterPortMapping(50331, router.Options(), _ => { }))
                {
                    m.Start();
                    Assert.True(WaitFor(() => m.State == PortMappingState.Mapped, 8000), m.State + " " + m.Problem);
                    Assert.NotEqual(50331, m.External!.Port);
                    lock (router.Mappings) Assert.Equal("127.0.0.1:50331 3600", router.Mappings[m.External.Port]);
                }
            }
        }

        [Fact]
        public void An_old_router_that_only_takes_permanent_forwards_gets_one()
        {
            using (var router = new FakeRouter())
            {
                router.RefuseAdd = a => a["NewLeaseDuration"] != "0" ? Tuple.Create(725, "OnlyPermanentLeasesSupported") : null;
                using (var m = new RouterPortMapping(45000, router.Options(), _ => { }))
                {
                    m.Start();
                    Assert.True(WaitFor(() => m.State == PortMappingState.Mapped, 8000), m.State + " " + m.Problem);
                    lock (router.Mappings) Assert.Equal("127.0.0.1:45000 0", router.Mappings[45000]);
                }
                lock (router.Mappings) Assert.Empty(router.Mappings);     // a permanent forward must not outlive us
            }
        }

        [Fact]
        public void A_refusal_is_reported_with_the_routers_reason()
        {
            using (var router = new FakeRouter())
            {
                router.RefuseAdd = a => Tuple.Create(606, "Action not authorized");
                var log = new List<string>();
                using (var m = new RouterPortMapping(45001, router.Options(), l => { lock (log) log.Add(l); }))
                {
                    m.Start();
                    Assert.True(WaitFor(() => m.State == PortMappingState.Refused, 8000), m.State.ToString());
                    Assert.False(m.Open);
                    Assert.Contains("606", m.Problem);
                    lock (log) Assert.Contains(log, l => l.Contains("could not forward UDP 45001") && l.Contains("Action not authorized"));
                }
            }
        }

        [Fact]
        public void A_router_behind_another_nat_is_mapped_but_not_open()
        {
            using (var router = new FakeRouter { ExternalIp = "100.64.1.2" })
            using (var m = new RouterPortMapping(45002, router.Options(), _ => { }))
            {
                m.Start();
                Assert.True(WaitFor(() => m.State == PortMappingState.Mapped, 8000), m.State + " " + m.Problem);
                Assert.True(m.BehindAnotherNat);
                Assert.False(m.Open);
            }
        }

        [Fact]
        public void No_router_and_a_public_address_on_this_pc_are_told_apart()
        {
            int silent;
            using (var s = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp))
            {
                s.Bind(new IPEndPoint(IPAddress.Loopback, 0));
                silent = ((IPEndPoint)s.LocalEndPoint!).Port;
            }
            var none = new PortMappingOptions { SsdpTarget = new IPEndPoint(IPAddress.Loopback, silent), Bind = IPAddress.Loopback, SearchTimeoutMs = 400, RetrySeconds = 60 };
            using (var m = new RouterPortMapping(45003, none, _ => { }))
            {
                m.Start();
                Assert.True(WaitFor(() => m.State == PortMappingState.NoRouter, 5000), m.State.ToString());
                Assert.Contains("no UPnP router answered", m.Problem);
            }

            var direct = new PortMappingOptions { Bind = IPAddress.Parse("198.51.100.20") };
            using (var m = new RouterPortMapping(45004, direct, _ => { }))
            {
                m.Start();
                Assert.True(WaitFor(() => m.State == PortMappingState.Direct, 5000), m.State.ToString());
                Assert.True(m.Open);
                Assert.Equal(new IPEndPoint(IPAddress.Parse("198.51.100.20"), 45004), m.External);
            }
        }
    }

    /// <summary>
    /// Hole punching through simulated routers. The symmetric one gives every destination the next port, like the
    /// China Telecom router that advertised :20833 (what the echo saw) and reached the host from :20834.
    /// </summary>
    public class SymmetricNatTests
    {
        static readonly IPEndPoint EchoServer = new IPEndPoint(IPAddress.Parse("101.43.23.94"), 18651);

        sealed class Router
        {
            readonly IPAddress _public;
            readonly int _basePort;
            readonly bool _symmetric;
            readonly Dictionary<IPEndPoint, int> _ports = new Dictionary<IPEndPoint, int>();
            readonly HashSet<string> _pinholes = new HashSet<string>();
            int _next = 1;

            public Router(string publicIp, int basePort, bool symmetric, bool open)
            {
                _public = IPAddress.Parse(publicIp);
                _basePort = basePort;
                _symmetric = symmetric;
                Open = open;
                Send(EchoServer);                      // the echo saw the base port: that is what gets advertised
            }

            /// <summary>Router forward + firewall rule: anything addressed to the base port gets in.</summary>
            public bool Open { get; }
            public int Dropped;
            public string Advertised => _public + ":" + _basePort;
            public bool Owns(IPEndPoint to) => to.Address.Equals(_public);

            /// <summary>The public source of a packet this side sends to <paramref name="to"/>; opens its pinhole.</summary>
            public IPEndPoint Send(IPEndPoint to)
            {
                int port = _basePort;
                if (_symmetric && !_ports.TryGetValue(to, out port))
                {
                    port = _basePort + (_ports.Count == 0 ? 0 : _next++);
                    _ports[to] = port;
                }
                _pinholes.Add(port + ">" + to);
                return new IPEndPoint(_public, port);
            }

            public bool Admits(IPEndPoint from, IPEndPoint to)
            {
                bool ok = _pinholes.Contains(to.Port + ">" + from) || (Open && to.Port == _basePort);
                if (!ok) Dropped++;
                return ok;
            }
        }

        static (PeerSession host, PeerSession viewer) Pair(Router hostRouter, Router viewerRouter)
        {
            GroupKeys keys = Vectors.Keys;
            string sid = GroupKeys.NewId();
            var host = new PeerSession(keys, sid, PeerRole.Host, 0);
            var viewer = new PeerSession(keys, sid, PeerRole.Viewer, 0);
            host.AddRemoteCandidates(new[] { "192.168.51.108:62909", viewerRouter.Advertised });
            viewer.AddRemoteCandidates(new[] { "192.168.1.3:50331", hostRouter.Advertised });
            return (host, viewer);
        }

        static void Run(PeerSession host, Router hostRouter, PeerSession viewer, Router viewerRouter, long durationUs)
        {
            var inFlight = new List<(byte[] data, IPEndPoint from, IPEndPoint to)>();
            for (long now = 0; now < durationUs; now += 10_000)
            {
                // the other network's LAN candidate never leaves (and gets no NAT mapping): the real router gave the
                // first packet to the host's public address the port right after the advertised one
                foreach (Outgoing o in host.Tick(now))
                    if (viewerRouter.Owns(o.To)) inFlight.Add((o.Data, hostRouter.Send(o.To), o.To));
                foreach (Outgoing o in viewer.Tick(now))
                    if (hostRouter.Owns(o.To)) inFlight.Add((o.Data, viewerRouter.Send(o.To), o.To));
                var batch = inFlight.ToList();
                inFlight.Clear();
                foreach (var p in batch)
                {
                    bool toHost = hostRouter.Owns(p.to);
                    Router r = toHost ? hostRouter : viewerRouter;
                    if (!r.Admits(p.from, p.to)) continue;
                    foreach (Outgoing reply in (toHost ? host : viewer).OnControl(p.data, p.from, now))
                        inFlight.Add((reply.Data, r.Send(reply.To), reply.To));
                }
                if (host.IsOver && viewer.IsOver) break;
                if (host.State == LinkState.Connected && viewer.State == LinkState.Connected) break;
            }
        }

        [Fact]
        public void A_viewer_behind_a_symmetric_nat_reaches_a_host_that_takes_ipv4_from_any_port()
        {
            var hostRouter = new Router("203.0.113.65", 50331, symmetric: false, open: true);
            var viewerRouter = new Router("198.51.100.3", 20833, symmetric: true, open: false);
            var (host, viewer) = Pair(hostRouter, viewerRouter);
            Run(host, hostRouter, viewer, viewerRouter, 5_000_000);

            Assert.Equal(LinkState.Connected, viewer.State);
            Assert.Equal(LinkState.Connected, host.State);                  // on a PUNCH_ACK now, not only once SRT flows
            var learned = new IPEndPoint(IPAddress.Parse("198.51.100.3"), 20834);
            Assert.Equal(learned, host.Selected);
            Assert.False(host.IsAdvertised(learned));
            Assert.Contains(learned, host.RemoteCandidates);
        }

        [Fact]
        public void Nobody_gets_through_when_neither_side_takes_unsolicited_ipv4()
        {
            var hostRouter = new Router("203.0.113.65", 50331, symmetric: false, open: false);
            var viewerRouter = new Router("198.51.100.3", 20833, symmetric: true, open: false);
            var (host, viewer) = Pair(hostRouter, viewerRouter);
            Run(host, hostRouter, viewer, viewerRouter, PeerSession.PunchDeadlineUs + 500_000);

            Assert.Equal(LinkState.Failed, host.State);
            Assert.Equal(LinkState.Failed, viewer.State);
            Assert.Contains("no candidate answered", host.Reason);             // what the host's log said 7 times on 2026-09-25
            Assert.True(hostRouter.Dropped > 0 && viewerRouter.Dropped > 0);
        }

        [Fact]
        public void A_host_behind_a_symmetric_nat_reaches_a_viewer_that_takes_ipv4_from_any_port()
        {
            // the other direction: the friend in China hosts, and the viewer's side is the open one
            var hostRouter = new Router("198.51.100.3", 20884, symmetric: true, open: false);
            var viewerRouter = new Router("203.0.113.65", 50331, symmetric: false, open: true);
            var (host, viewer) = Pair(hostRouter, viewerRouter);
            Run(host, hostRouter, viewer, viewerRouter, 5_000_000);

            Assert.Equal(LinkState.Connected, host.State);
            Assert.Equal(LinkState.Connected, viewer.State);                // needs the learned address: 20884 never answers
            var learned = new IPEndPoint(IPAddress.Parse("198.51.100.3"), 20885);
            Assert.Equal(learned, viewer.Selected);
            Assert.False(viewer.IsAdvertised(learned));
        }

        [Fact]
        public void Two_cone_routers_still_punch_through_without_help()
        {
            var hostRouter = new Router("203.0.113.65", 50331, symmetric: false, open: false);
            var viewerRouter = new Router("198.51.100.13", 48626, symmetric: false, open: false);
            var (host, viewer) = Pair(hostRouter, viewerRouter);
            Run(host, hostRouter, viewer, viewerRouter, 5_000_000);
            Assert.Equal(LinkState.Connected, viewer.State);
            Assert.Equal(LinkState.Connected, host.State);
            Assert.True(host.IsAdvertised(host.Selected!));
        }

        [Fact]
        public void Learned_addresses_are_capped_and_need_an_authentic_packet()
        {
            GroupKeys keys = Vectors.Keys;
            string sid = GroupKeys.NewId();
            var host = new PeerSession(keys, sid, PeerRole.Host, 0);
            host.AddRemoteCandidates(new[] { "203.0.113.1:1000" });
            byte[] punch = ControlPacket.Build(ControlType.Punch, PeerRole.Viewer, GroupKeys.IdBytesOf(sid), 5, 0, keys.PunchKey(sid));
            for (int i = 0; i < 40; i++) host.OnControl(punch, new IPEndPoint(IPAddress.Parse("198.51.100.9"), 30000 + i), 1000 + i);
            Assert.Equal(PeerSession.MaxRemoteCandidates, host.RemoteCandidates.Count);

            // HostLink / ViewerLink only hand over packets whose MAC checks out; a forged one never gets this far
            byte[] forged = ControlPacket.Build(ControlType.Punch, PeerRole.Viewer, GroupKeys.IdBytesOf(sid), 5, 0, keys.PunchKey(GroupKeys.NewId()));
            Assert.False(ControlPacket.VerifyMac(forged, keys.PunchKey(sid)));
        }
    }

    public class WindowsFirewallTests
    {
        const string Exe = @"D:\HdrCast-0.2.4\HdrCastSender.exe";
        const int Private = 2, Public = 4;

        static FirewallRule Rule(bool allow = true, string app = Exe, int protocol = WindowsFirewall.ProtocolUdp,
                                 int profiles = WindowsFirewall.ProfileAll, bool enabled = true, bool inbound = true, string ports = "*", string remote = "*") =>
            new FirewallRule { Name = "r", Application = app, Allow = allow, Protocol = protocol, Profiles = profiles, Enabled = enabled,
                               Inbound = inbound, LocalPorts = ports, RemoteAddresses = remote };

        static FirewallVerdict Eval(params FirewallRule[] rules) => WindowsFirewall.Evaluate(Exe, rules, Private, 7, 0);

        [Fact]
        public void Only_an_enabled_inbound_udp_allow_rule_for_this_program_on_every_active_network_counts()
        {
            Assert.Equal(FirewallVerdict.NotAllowed, Eval());
            Assert.Equal(FirewallVerdict.Allowed, Eval(Rule()));
            Assert.Equal(FirewallVerdict.Allowed, Eval(Rule(protocol: WindowsFirewall.ProtocolAny)));
            Assert.Equal(FirewallVerdict.Allowed, Eval(Rule(app: @"d:\hdrcast-0.2.4\HDRCASTSENDER.EXE")));
            Assert.Equal(FirewallVerdict.NotAllowed, Eval(Rule(protocol: 6)));                                 // TCP only
            Assert.Equal(FirewallVerdict.NotAllowed, Eval(Rule(app: @"D:\HdrCast-0.2.2\HdrCastSender.exe"))); // an older copy
            Assert.Equal(FirewallVerdict.NotAllowed, Eval(Rule(enabled: false)));
            Assert.Equal(FirewallVerdict.NotAllowed, Eval(Rule(inbound: false)));
            Assert.Equal(FirewallVerdict.NotAllowed, Eval(Rule(ports: "5000")));                               // our port is random
            Assert.Equal(FirewallVerdict.NotAllowed, Eval(Rule(remote: "LocalSubnet")));                       // friends are not local
            Assert.Equal(FirewallVerdict.NotAllowed, Eval(Rule(app: "")));                                     // not about this program
            Assert.Equal(FirewallVerdict.NotAllowed, WindowsFirewall.Evaluate(Exe, new[] { Rule(profiles: Private) }, Private | Public, 7, 0));
            Assert.Equal(FirewallVerdict.Allowed, WindowsFirewall.Evaluate(Exe, new[] { Rule(profiles: Private), Rule(profiles: Public) }, Private | Public, 7, 0));
        }

        [Fact]
        public void A_block_rule_wins_and_the_firewall_being_off_or_blocking_everything_is_told_apart()
        {
            Assert.Equal(FirewallVerdict.Blocked, Eval(Rule(), Rule(allow: false)));
            Assert.Equal(FirewallVerdict.Allowed, Eval(Rule(), Rule(allow: false, profiles: Public)));          // a block rule for another network
            Assert.Equal(FirewallVerdict.Off, WindowsFirewall.Evaluate(Exe, new FirewallRule[0], Private, Public, 0));
            Assert.Equal(FirewallVerdict.BlockAll, WindowsFirewall.Evaluate(Exe, new[] { Rule() }, Private, 7, Private));
        }

        [Fact]
        public void Rule_paths_with_environment_variables_match()
        {
            string system = Environment.GetEnvironmentVariable("SystemRoot") ?? "";
            if (!OperatingSystem.IsWindows() || system.Length == 0) return;
            Assert.True(WindowsFirewall.SameProgram(@"%SystemRoot%\system32\svchost.exe", Path.Combine(system, "system32", "svchost.exe")));
            Assert.False(WindowsFirewall.SameProgram(null, Exe));
        }

        [Fact]
        public void The_live_settings_can_be_read_on_windows()
        {
            // read-only: never adds a rule (that needs the Allow button and Windows' UAC prompt)
            if (!OperatingSystem.IsWindows()) return;
            FirewallVerdict v = WindowsFirewall.Check(Path.Combine(Path.GetTempPath(), "hdrcast-no-such-program.exe"));
            Assert.NotEqual(FirewallVerdict.Unknown, v);
            Assert.NotEqual(FirewallVerdict.Allowed, v);

            // The rule list really is walked: every Windows has rules for svchost ("Core Networking" and others),
            // spelled %SystemRoot%\system32\svchost.exe. A first version read the COM enumerator wrongly and saw none,
            // which looked exactly like "no rule for HdrCast".
            string svchost = Path.Combine(Environment.GetEnvironmentVariable("SystemRoot") ?? @"C:\Windows", "system32", "svchost.exe");
            List<FirewallRule>? rules = WindowsFirewall.RulesFor(svchost);
            Assert.NotNull(rules);
            Assert.NotEmpty(rules!);
        }
    }

    public class OpenIpv4FlagTests
    {
        [Fact]
        public void The_live_record_carries_open4_only_when_known_and_old_records_read_as_unknown()
        {
            var rec = new LiveRecord { HostId = GroupKeys.NewId(), Name = "NiseMono", Candidates = new List<string> { "203.0.113.65:50331" } };
            Assert.False(rec.ToPayload().ContainsKey("open4"));                // byte-identical to a 0.2.3 host's record
            Assert.Null(LiveRecord.FromPayload(rec.ToPayload(), rec.HostId)!.OpenIpv4);
            rec.OpenIpv4 = true;
            Assert.Equal(true, LiveRecord.FromPayload(rec.ToPayload(), rec.HostId)!.OpenIpv4);
            rec.OpenIpv4 = false;
            Assert.Equal(false, LiveRecord.FromPayload(rec.ToPayload(), rec.HostId)!.OpenIpv4);
        }

        [Fact]
        public void The_viewer_is_told_whose_side_kept_the_ipv4_attempt_out()
        {
            var host = new[] { "[2400:aaaa:bbbb:cccc:1234:5678:9abc:def0]:57608", "192.168.1.3:57609", "203.0.113.65:57609" };
            var me = new[] { "192.168.51.108:62909", "198.51.100.3:19522" };
            string closed = P2pDiagnostics.Explain(false, me, host, hostIpv4Open: false);
            Assert.Contains("This PC has no IPv6", closed);
            Assert.Contains("click Allow next to Firewall", closed);
            string open = P2pDiagnostics.Explain(false, me, host, hostIpv4Open: true);
            Assert.Contains("something on this side drops UDP", open);
            Assert.Equal(P2pDiagnostics.Explain(false, me, host), P2pDiagnostics.Explain(false, me, host, null));
            Assert.DoesNotContain("Allow", P2pDiagnostics.Explain(false, me, host));
        }

        [Theory]
        [InlineData("203.0.113.65", true)]
        [InlineData("100.64.1.2", false)]
        [InlineData("192.168.1.1", false)]
        [InlineData("0.0.0.0", false)]
        [InlineData("239.255.255.250", false)]
        [InlineData("2400::1", false)]
        public void Public_ipv4_addresses_are_recognised(string address, bool isPublic)
        {
            Assert.Equal(isPublic, LocalAddresses.IsPublicIpv4(IPAddress.Parse(address)));
        }
    }
}
