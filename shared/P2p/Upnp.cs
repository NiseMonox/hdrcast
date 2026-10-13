#pragma warning disable SYSLIB0014 // HttpWebRequest: the net48 apps have no HttpClient without extra packages
using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Net;
using System.Net.Sockets;
using System.Security;
using System.Text;
using System.Threading;
using System.Xml;

namespace HdrCast.P2p
{
    /// <summary>A failed UPnP call: the router's UPnP error code (718 = port taken, 606 = not allowed, …) or 0.</summary>
    public sealed class UpnpException : Exception
    {
        public UpnpException(int code, string message, Exception? inner = null) : base(message, inner) { Code = code; }

        public int Code { get; }
    }

    /// <summary>One WANIPConnection / WANPPPConnection service of an Internet Gateway Device.</summary>
    public sealed class UpnpGateway
    {
        public UpnpGateway(Uri controlUrl, string serviceType, string name)
        {
            ControlUrl = controlUrl;
            ServiceType = serviceType;
            Name = name;
        }

        public Uri ControlUrl { get; }
        public string ServiceType { get; }
        /// <summary>The router's model or friendly name, for logs.</summary>
        public string Name { get; }
    }

    /// <summary>
    /// The small part of UPnP IGD (v1 and v2) needed to forward one UDP port (docs/design.md §8.7): an SSDP search,
    /// the device description, and the SOAP actions GetExternalIPAddress / AddPortMapping / DeletePortMapping.
    /// </summary>
    public static class UpnpClient
    {
        public static readonly IPEndPoint SsdpMulticast = new IPEndPoint(IPAddress.Parse("239.255.255.250"), 1900);

        public const int ErrorOnlyPermanentLeases = 725;
        public const int ErrorConflict = 718;

        const int MaxDescriptionChars = 256 * 1024;

        static readonly string[] SearchTargets =
        {
            "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
            "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
            "urn:schemas-upnp-org:service:WANIPConnection:1",
            "urn:schemas-upnp-org:service:WANIPConnection:2",
            "urn:schemas-upnp-org:service:WANPPPConnection:1",
        };

        /// <summary>
        /// LOCATION URLs of the gateways that answer an SSDP search sent from <paramref name="bind"/> (this PC's LAN
        /// address, so the multicast leaves on the LAN and not on a VPN) to <paramref name="target"/>.
        /// </summary>
        public static List<Uri> Search(IPAddress bind, IPEndPoint target, int timeoutMs, CancellationToken ct)
        {
            var found = new List<Uri>();
            using (var s = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp))
            {
                s.Bind(new IPEndPoint(bind, 0));
                if (IsMulticast(target.Address))
                {
                    try
                    {
                        s.SetSocketOption(SocketOptionLevel.IP, SocketOptionName.MulticastInterface, bind.GetAddressBytes());
                        s.SetSocketOption(SocketOptionLevel.IP, SocketOptionName.MulticastTimeToLive, 2);
                    }
                    catch (SocketException) { }
                }
                for (int round = 0; round < 2; round++)
                {
                    foreach (string st in SearchTargets)
                    {
                        byte[] msg = Encoding.ASCII.GetBytes(
                            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: " + st + "\r\n\r\n");
                        try { s.SendTo(msg, target); } catch (SocketException) { }
                    }
                }

                var buf = new byte[4096];
                DateTime deadline = DateTime.UtcNow.AddMilliseconds(timeoutMs);
                while (!ct.IsCancellationRequested)
                {
                    int left = (int)(deadline - DateTime.UtcNow).TotalMilliseconds;
                    if (left <= 0) break;
                    s.ReceiveTimeout = Math.Min(left, 250);
                    EndPoint from = new IPEndPoint(IPAddress.Any, 0);
                    int n;
                    try
                    {
                        n = s.ReceiveFrom(buf, ref from);
                    }
                    catch (SocketException)
                    {
                        continue;        // timeout slice, or an ICMP error for the search
                    }
                    Uri? location = ParseSearchResponse(Encoding.ASCII.GetString(buf, 0, n));
                    if (location == null || found.Exists(u => u.Equals(location))) continue;
                    found.Add(location);
                    // the router answers within a few ms; give a second device a moment, then stop waiting
                    DateTime soon = DateTime.UtcNow.AddMilliseconds(400);
                    if (soon < deadline) deadline = soon;
                }
            }
            return found;
        }

        /// <summary>The LOCATION of an SSDP "HTTP/1.1 200 OK" answer; null for anything else.</summary>
        public static Uri? ParseSearchResponse(string text)
        {
            if (!text.StartsWith("HTTP/1.1 200", StringComparison.OrdinalIgnoreCase) &&
                !text.StartsWith("HTTP/1.0 200", StringComparison.OrdinalIgnoreCase)) return null;
            foreach (string raw in text.Split('\n'))
            {
                string line = raw.Trim();
                int colon = line.IndexOf(':');
                if (colon <= 0 || !line.Substring(0, colon).Trim().Equals("LOCATION", StringComparison.OrdinalIgnoreCase)) continue;
                string value = line.Substring(colon + 1).Trim();
                if (Uri.TryCreate(value, UriKind.Absolute, out Uri? u) && u.Scheme == Uri.UriSchemeHttp) return u;
            }
            return null;
        }

        /// <summary>The WAN connection services the device description at <paramref name="location"/> lists.</summary>
        public static List<UpnpGateway> ReadDescription(Uri location, int timeoutMs)
        {
            var req = (HttpWebRequest)WebRequest.Create(location);
            Prepare(req, "GET", timeoutMs);
            try
            {
                using (var resp = (HttpWebResponse)req.GetResponse())
                    return ParseDescription(ReadText(resp), location);
            }
            catch (WebException ex)
            {
                throw new UpnpException(0, "the router's description could not be read (" + ex.Status + ")", ex);
            }
            catch (IOException ex)
            {
                throw new UpnpException(0, "the router's description could not be read", ex);
            }
        }

        /// <summary>Pure: WANIPConnection / WANPPPConnection services with absolute control URLs, in document order.</summary>
        public static List<UpnpGateway> ParseDescription(string xml, Uri location)
        {
            var list = new List<UpnpGateway>();
            XmlDocument doc;
            try { doc = Load(xml); }
            catch (XmlException ex) { throw new UpnpException(0, "the router's description is not valid XML", ex); }
            Uri baseUri = location;
            string? urlBase = FirstText(doc, "URLBase");
            if (!string.IsNullOrEmpty(urlBase) && Uri.TryCreate(urlBase, UriKind.Absolute, out Uri? b)) baseUri = b;
            string name = FirstText(doc, "modelName") ?? FirstText(doc, "friendlyName") ?? location.Host;

            foreach (XmlNode service in doc.SelectNodes("//*[local-name()='service']")!)
            {
                string type = (ChildText(service, "serviceType") ?? "").Trim();
                if (!type.StartsWith("urn:schemas-upnp-org:service:WANIPConnection:", StringComparison.Ordinal) &&
                    !type.StartsWith("urn:schemas-upnp-org:service:WANPPPConnection:", StringComparison.Ordinal)) continue;
                string control = (ChildText(service, "controlURL") ?? "").Trim();
                if (control.Length == 0 || !Uri.TryCreate(baseUri, control, out Uri? url)) continue;
                list.Add(new UpnpGateway(url, type, name.Trim()));
            }
            return list;
        }

        public static IPAddress? GetExternalAddress(UpnpGateway gw, int timeoutMs)
        {
            Dictionary<string, string> r = Call(gw, "GetExternalIPAddress", new List<KeyValuePair<string, string>>(), timeoutMs);
            return r.TryGetValue("NewExternalIPAddress", out string? text) && IPAddress.TryParse(text.Trim(), out IPAddress? a) &&
                   a!.AddressFamily == AddressFamily.InterNetwork ? a : null;
        }

        public static void AddPortMapping(UpnpGateway gw, int externalPort, IPAddress internalClient, int internalPort,
                                          string description, int leaseSeconds, int timeoutMs)
        {
            // Argument order as in the IGD spec: some routers insist on it.
            Call(gw, "AddPortMapping", new List<KeyValuePair<string, string>>
            {
                Arg("NewRemoteHost", ""),
                Arg("NewExternalPort", externalPort.ToString(CultureInfo.InvariantCulture)),
                Arg("NewProtocol", "UDP"),
                Arg("NewInternalPort", internalPort.ToString(CultureInfo.InvariantCulture)),
                Arg("NewInternalClient", internalClient.ToString()),
                Arg("NewEnabled", "1"),
                Arg("NewPortMappingDescription", description),
                Arg("NewLeaseDuration", leaseSeconds.ToString(CultureInfo.InvariantCulture)),
            }, timeoutMs);
        }

        public static void DeletePortMapping(UpnpGateway gw, int externalPort, int timeoutMs)
        {
            Call(gw, "DeletePortMapping", new List<KeyValuePair<string, string>>
            {
                Arg("NewRemoteHost", ""),
                Arg("NewExternalPort", externalPort.ToString(CultureInfo.InvariantCulture)),
                Arg("NewProtocol", "UDP"),
            }, timeoutMs);
        }

        /// <summary>One SOAP action; returns the response's out-arguments, throws <see cref="UpnpException"/> on a fault.</summary>
        public static Dictionary<string, string> Call(UpnpGateway gw, string action, IList<KeyValuePair<string, string>> args, int timeoutMs)
        {
            byte[] body = Encoding.UTF8.GetBytes(BuildEnvelope(gw.ServiceType, action, args));
            var req = (HttpWebRequest)WebRequest.Create(gw.ControlUrl);
            Prepare(req, "POST", timeoutMs);
            req.ContentType = "text/xml; charset=\"utf-8\"";
            req.Headers["SOAPAction"] = "\"" + gw.ServiceType + "#" + action + "\"";
            req.ContentLength = body.Length;
            try
            {
                using (Stream s = req.GetRequestStream()) s.Write(body, 0, body.Length);
                using (var resp = (HttpWebResponse)req.GetResponse())
                    return ParseResponse(ReadText(resp), action);
            }
            catch (WebException ex) when (ex.Response is HttpWebResponse er)
            {
                string text;
                int status;
                using (er)
                {
                    status = (int)er.StatusCode;             // before the dispose: it throws afterwards
                    try { text = ReadText(er); } catch (Exception) { text = ""; }
                }
                throw ParseFault(text, status, action);
            }
            catch (WebException ex)
            {
                throw new UpnpException(0, action + ": the router did not answer (" + ex.Status + ")", ex);
            }
            catch (IOException ex)
            {
                throw new UpnpException(0, action + ": the connection to the router failed", ex);
            }
        }

        /// <summary>Pure: the SOAP request body.</summary>
        public static string BuildEnvelope(string serviceType, string action, IList<KeyValuePair<string, string>> args)
        {
            var sb = new StringBuilder();
            sb.Append("<?xml version=\"1.0\"?>");
            sb.Append("<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">");
            sb.Append("<s:Body><u:").Append(action).Append(" xmlns:u=\"").Append(SecurityElement.Escape(serviceType)).Append("\">");
            foreach (KeyValuePair<string, string> a in args)
                sb.Append('<').Append(a.Key).Append('>').Append(SecurityElement.Escape(a.Value)).Append("</").Append(a.Key).Append('>');
            sb.Append("</u:").Append(action).Append("></s:Body></s:Envelope>");
            return sb.ToString();
        }

        /// <summary>Pure: the child elements of &lt;u:ActionResponse&gt; by local name.</summary>
        public static Dictionary<string, string> ParseResponse(string xml, string action)
        {
            XmlDocument doc;
            try { doc = Load(xml); }
            catch (XmlException ex) { throw new UpnpException(0, action + ": the router sent something that is not SOAP", ex); }
            XmlNode? response = doc.SelectSingleNode("//*[local-name()='" + action + "Response']");
            if (response == null) throw new UpnpException(0, action + ": the router's answer has no " + action + "Response");
            var result = new Dictionary<string, string>(StringComparer.Ordinal);
            foreach (XmlNode child in response.ChildNodes)
                if (child.NodeType == XmlNodeType.Element) result[child.LocalName] = child.InnerText;
            return result;
        }

        /// <summary>Pure: a SOAP fault (UPnPError errorCode / errorDescription) as an exception.</summary>
        public static UpnpException ParseFault(string xml, int httpStatus, string action)
        {
            int code = 0;
            string description = "";
            try
            {
                XmlDocument doc = Load(xml);
                int.TryParse((FirstText(doc, "errorCode") ?? "").Trim(), NumberStyles.Integer, CultureInfo.InvariantCulture, out code);
                description = (FirstText(doc, "errorDescription") ?? "").Trim();
            }
            catch (XmlException) { }
            string what = code != 0 ? "error " + code + (description.Length > 0 ? " " + description : "") : "HTTP " + httpStatus;
            return new UpnpException(code, action + " refused by the router (" + what + ")");
        }

        /// <summary>This PC's address on the way to <paramref name="gateway"/>: the mapping's internal client.</summary>
        public static IPAddress? LocalAddressTowards(IPAddress gateway)
        {
            try
            {
                using (var s = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp))
                {
                    s.Connect(new IPEndPoint(gateway, 1900));            // UDP connect: picks a route, sends nothing
                    return ((IPEndPoint)s.LocalEndPoint!).Address;
                }
            }
            catch (SocketException)
            {
                return null;
            }
        }

        static bool IsMulticast(IPAddress a)
        {
            byte[] b = a.GetAddressBytes();
            return b.Length == 4 && b[0] >= 224 && b[0] <= 239;
        }

        static KeyValuePair<string, string> Arg(string name, string value) => new KeyValuePair<string, string>(name, value);

        static void Prepare(HttpWebRequest req, string method, int timeoutMs)
        {
            req.Method = method;
            req.Proxy = null;                      // the router is on the LAN; a system proxy would only get in the way
            req.Timeout = timeoutMs;
            req.ReadWriteTimeout = timeoutMs;
            req.KeepAlive = false;                 // routers' tiny HTTP servers handle one request per connection best
            req.UserAgent = "HdrCast/1 UPnP/1.1";
            req.ServicePoint.Expect100Continue = false;
        }

        static string ReadText(HttpWebResponse resp)
        {
            using (Stream s = resp.GetResponseStream())
            using (var r = new StreamReader(s, Encoding.UTF8))
            {
                var sb = new StringBuilder();
                var buf = new char[8192];
                int n;
                while ((n = r.Read(buf, 0, buf.Length)) > 0)
                {
                    if (sb.Length + n > MaxDescriptionChars) throw new IOException("the router's answer is too large");
                    sb.Append(buf, 0, n);
                }
                return sb.ToString();
            }
        }

        /// <summary>Parses without DTDs or external entities: the text comes from any device on the LAN.</summary>
        static XmlDocument Load(string xml)
        {
            var settings = new XmlReaderSettings { DtdProcessing = DtdProcessing.Ignore, XmlResolver = null };
            var doc = new XmlDocument { XmlResolver = null };
            using (var sr = new StringReader(xml))
            using (XmlReader r = XmlReader.Create(sr, settings))
                doc.Load(r);
            return doc;
        }

        static string? FirstText(XmlNode doc, string localName) =>
            doc.SelectSingleNode("//*[local-name()='" + localName + "']")?.InnerText;

        static string? ChildText(XmlNode node, string localName)
        {
            foreach (XmlNode child in node.ChildNodes)
                if (child.NodeType == XmlNodeType.Element && child.LocalName == localName) return child.InnerText;
            return null;
        }
    }

    public enum PortMappingState
    {
        Off = 0,
        /// <summary>Looking for the router / asking it.</summary>
        Working = 1,
        /// <summary>The router forwards the port to this PC.</summary>
        Mapped = 2,
        /// <summary>This PC has a public IPv4 address itself: there is no router to ask.</summary>
        Direct = 3,
        /// <summary>No UPnP router answered.</summary>
        NoRouter = 4,
        /// <summary>A router answered but would not forward the port.</summary>
        Refused = 5,
    }

    public sealed class PortMappingOptions
    {
        /// <summary>Where the SSDP search goes (tests point it at a stand-in router).</summary>
        public IPEndPoint SsdpTarget { get; set; } = UpnpClient.SsdpMulticast;
        /// <summary>The address to search from; null = this PC's LAN IPv4.</summary>
        public IPAddress? Bind { get; set; }
        public int SearchTimeoutMs { get; set; } = 2500;
        public int CallTimeoutMs { get; set; } = 3000;
        /// <summary>Lease asked for; renewed well before it runs out, and gone by itself an hour after a crash.</summary>
        public int LeaseSeconds { get; set; } = 3600;
        public int RenewSeconds { get; set; } = 1200;
        /// <summary>First retry after "no router" / a refusal; doubles up to 15 minutes.</summary>
        public int RetrySeconds { get; set; } = 60;
        public string Description { get; set; } = "HdrCast";
    }

    /// <summary>
    /// Keeps this PC's IPv4 P2P port forwarded on the home router while it lives (docs/design.md §8.7), so peers whose
    /// routers pick a new port for every destination (symmetric NAT) can still reach it: their packets arrive from a
    /// port this PC never sent to, which a router without the forward drops. Works on a background thread, renews
    /// the lease, and deletes the forward on <see cref="Dispose"/>.
    /// </summary>
    public sealed class RouterPortMapping : IDisposable
    {
        readonly int _port;
        readonly PortMappingOptions _o;
        readonly Action<string> _log;
        readonly object _lock = new object();
        readonly CancellationTokenSource _stop = new CancellationTokenSource();
        readonly Random _random = new Random();
        Thread? _thread;
        UpnpGateway? _gateway;
        IPAddress? _client;
        int _externalPort;
        int _lease;
        string _lastLogged = "";
        PortMappingState _state = PortMappingState.Off;
        IPEndPoint? _external;
        bool _behindAnotherNat;
        string _routerName = "";
        string _problem = "";

        public RouterPortMapping(int port, PortMappingOptions? options, Action<string> log)
        {
            if (port <= 0 || port > 65535) throw new ArgumentOutOfRangeException(nameof(port));
            _port = port;
            _o = options ?? new PortMappingOptions();
            _log = log ?? (_ => { });
        }

        /// <summary>Raised (on the mapping thread) whenever <see cref="State"/> or <see cref="External"/> changes.</summary>
        public event Action? Changed;

        public int Port => _port;
        public PortMappingState State { get { lock (_lock) return _state; } }
        /// <summary>The router's internet address and the forwarded port (or this PC's own public address).</summary>
        public IPEndPoint? External { get { lock (_lock) return _external; } }
        /// <summary>The router's own internet address is private or CGNAT: the forward exists but cannot help.</summary>
        public bool BehindAnotherNat { get { lock (_lock) return _behindAnotherNat; } }
        public string RouterName { get { lock (_lock) return _routerName; } }
        /// <summary>Why there is no forward (NoRouter / Refused), in words.</summary>
        public string Problem { get { lock (_lock) return _problem; } }

        /// <summary>Peers can reach the port from anywhere, as far as the router is concerned.</summary>
        public bool Open
        {
            get
            {
                lock (_lock) return (_state == PortMappingState.Mapped && !_behindAnotherNat) || _state == PortMappingState.Direct;
            }
        }

        public void Start()
        {
            if (_thread != null) return;
            _thread = new Thread(Loop) { IsBackground = true, Name = "p2p-upnp" };
            _thread.Start();
        }

        void Loop()
        {
            int failures = 0;
            CancellationToken ct = _stop.Token;
            while (!ct.IsCancellationRequested)
            {
                bool ok, mapped;
                lock (_lock) mapped = _gateway != null;
                try
                {
                    ok = mapped ? Renew() : Map(ct);
                }
                catch (Exception ex)
                {
                    ok = false;
                    Fail(PortMappingState.Refused, "unexpected error: " + ex.Message);
                }
                if (ct.IsCancellationRequested) break;
                if (State == PortMappingState.Direct) break;          // nothing to keep alive
                failures = ok ? 0 : failures + 1;
                int wait = ok ? _o.RenewSeconds : Math.Min(_o.RetrySeconds << Math.Min(failures - 1, 4), 900);
                if (ct.WaitHandle.WaitOne(TimeSpan.FromSeconds(Math.Max(1, wait)))) break;
            }
        }

        bool Map(CancellationToken ct)
        {
            Set(PortMappingState.Working, null, false, "", "");
            IPAddress? bind = _o.Bind ?? LocalAddresses.LanIpv4();
            if (bind == null) return Fail(PortMappingState.NoRouter, "this PC has no IPv4 network");
            if (LocalAddresses.IsPublicIpv4(bind))
            {
                Set(PortMappingState.Direct, new IPEndPoint(bind, _port), false, "", "");
                Report("[p2p] router: this PC has the public IPv4 address " + bind + " itself, no router to open a port on");
                return true;
            }

            List<Uri> locations;
            try
            {
                locations = UpnpClient.Search(bind, _o.SsdpTarget, _o.SearchTimeoutMs, ct);
            }
            catch (SocketException ex)
            {
                return Fail(PortMappingState.NoRouter, "the UPnP search failed (" + ex.SocketErrorCode + ")");
            }
            if (ct.IsCancellationRequested) return false;
            if (locations.Count == 0) return Fail(PortMappingState.NoRouter, "no UPnP router answered (UPnP may be off in the router's settings)");

            string refusal = "";
            foreach (Uri location in locations)
            {
                List<UpnpGateway> gateways;
                try
                {
                    gateways = UpnpClient.ReadDescription(location, _o.CallTimeoutMs);
                }
                catch (UpnpException ex)
                {
                    refusal = ex.Message;
                    continue;
                }
                foreach (UpnpGateway gw in gateways)
                {
                    if (ct.IsCancellationRequested) return false;
                    IPAddress? external = null;
                    try { external = UpnpClient.GetExternalAddress(gw, _o.CallTimeoutMs); }
                    catch (UpnpException) { }        // some routers do not say; the forward can still work
                    IPAddress client = IPAddress.TryParse(gw.ControlUrl.Host, out IPAddress? gwAddress)
                        ? UpnpClient.LocalAddressTowards(gwAddress!) ?? bind : bind;
                    string? why = TryAdd(gw, client, ct);
                    if (why == null)
                    {
                        if (ct.IsCancellationRequested) { Remove(); return false; }
                        bool behind = external != null && !LocalAddresses.IsPublicIpv4(external);
                        Set(PortMappingState.Mapped, external == null ? null : new IPEndPoint(external, _externalPort), behind, gw.Name, "");
                        string where = gw.Name + (external != null ? ", internet address " + external + ":" + _externalPort : "");
                        Report(behind
                            ? "[p2p] router: UDP " + _port + " is forwarded on " + gw.Name + ", but that router's own internet address " +
                              external + " is not public (the provider or another router adds a second NAT), so it cannot make this PC reachable"
                            : "[p2p] router: UDP " + _port + " forwarded to this PC (" + where + "); renewed while live, removed when HdrCast stops");
                        return true;
                    }
                    refusal = why;
                }
            }
            return Fail(PortMappingState.Refused, refusal.Length > 0 ? refusal : "the router offers no WAN connection service");
        }

        /// <summary>Adds the forward on <paramref name="gw"/>; null on success, else why not.</summary>
        string? TryAdd(UpnpGateway gw, IPAddress client, CancellationToken ct)
        {
            int lease = _o.LeaseSeconds;
            var ports = new List<int> { _port };
            for (int i = 0; i < 3; i++) ports.Add(_random.Next(20000, 60000));
            string why = "";
            for (int i = 0; i < ports.Count && !ct.IsCancellationRequested; i++)
            {
                try
                {
                    UpnpClient.AddPortMapping(gw, ports[i], client, _port, _o.Description, lease, _o.CallTimeoutMs);
                    lock (_lock)
                    {
                        _gateway = gw;
                        _client = client;
                        _externalPort = ports[i];
                        _lease = lease;
                    }
                    return null;
                }
                catch (UpnpException ex) when (ex.Code == UpnpClient.ErrorOnlyPermanentLeases && lease != 0)
                {
                    lease = 0;           // an old IGD v1 router: ask for a permanent forward and delete it on stop
                    i--;
                }
                catch (UpnpException ex) when (ex.Code == UpnpClient.ErrorConflict)
                {
                    why = "UDP " + ports[i] + " is already forwarded to another device";   // try another outside port
                }
                catch (UpnpException ex)
                {
                    return ex.Message;     // includes 724 "same port values required" after a conflict
                }
            }
            return why;
        }

        bool Renew()
        {
            UpnpGateway? gw;
            IPAddress? client;
            int port, lease;
            lock (_lock)
            {
                gw = _gateway;
                client = _client;
                port = _externalPort;
                lease = _lease;
            }
            if (gw == null || client == null) return false;
            try
            {
                UpnpClient.AddPortMapping(gw, port, client, _port, _o.Description, lease, _o.CallTimeoutMs);
                return true;
            }
            catch (UpnpException ex)
            {
                lock (_lock) _gateway = null;          // start over: the router may have restarted or changed
                return Fail(PortMappingState.Refused, "renewing the forward failed: " + ex.Message);
            }
        }

        bool Fail(PortMappingState state, string problem)
        {
            Set(state, null, false, RouterName, problem);
            Report("[p2p] router: could not forward UDP " + _port + " to this PC - " + problem +
                   ". Friends whose routers change ports cannot reach this PC over IPv4 (docs/network.md §1.1)");
            return false;
        }

        void Set(PortMappingState state, IPEndPoint? external, bool behind, string name, string problem)
        {
            bool changed;
            lock (_lock)
            {
                changed = _state != state || !Equals(_external, external) || _behindAnotherNat != behind;
                _state = state;
                _external = external;
                _behindAnotherNat = behind;
                if (name.Length > 0) _routerName = name;
                _problem = problem;
            }
            if (changed) Changed?.Invoke();
        }

        /// <summary>Logs a line once: retries that end the same way stay quiet.</summary>
        void Report(string line)
        {
            lock (_lock)
            {
                if (line == _lastLogged) return;
                _lastLogged = line;
            }
            _log(line);
        }

        void Remove()
        {
            UpnpGateway? gw;
            int port;
            lock (_lock)
            {
                gw = _gateway;
                port = _externalPort;
                _gateway = null;
            }
            if (gw == null) return;
            try
            {
                UpnpClient.DeletePortMapping(gw, port, Math.Min(_o.CallTimeoutMs, 2000));
                _log("[p2p] router: forward of UDP " + port + " removed");
            }
            catch (UpnpException ex)
            {
                _log("[p2p] router: could not remove the forward of UDP " + port + " (" + ex.Message + "); it expires by itself");
            }
        }

        /// <summary>Stops renewing and deletes the forward (up to a few seconds: it waits for the router).</summary>
        public void Dispose()
        {
            if (_stop.IsCancellationRequested) return;
            _stop.Cancel();
            if (_thread != null && Thread.CurrentThread != _thread) _thread.Join(_o.SearchTimeoutMs + _o.CallTimeoutMs + 1000);
            Remove();
            lock (_lock) _state = PortMappingState.Off;
        }
    }
}
