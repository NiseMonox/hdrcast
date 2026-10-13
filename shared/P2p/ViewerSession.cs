using System;
using System.Collections.Generic;
using System.Linq;
using System.Net;
using System.Net.Sockets;
using System.Threading;
using HdrCast.Common;
using HdrCast.Group;

namespace HdrCast.P2p
{
    public sealed class ViewerSessionOptions
    {
        public GroupConfig Group { get; set; } = new GroupConfig();
        public string HostId { get; set; } = "";
        /// <summary>SRT latency this viewer wants (its region preset).</summary>
        public int LatencyMs { get; set; } = 1500;
        public int OverheadPercent { get; set; } = 25;
        /// <summary>Null = detect this PC's stable IPv6 and LAN IPv4.</summary>
        public LinkAddresses? Addresses { get; set; }
        public bool DiscoverPublicIpv4 { get; set; } = true;
        public int EchoPort { get; set; } = EchoProtocol.DefaultPort;
        /// <summary>Fixed local port for mpv (0 = any free port).</summary>
        public int LocalPort { get; set; }
        /// <summary>Forward this PC's IPv4 port on the router (UPnP), so hosts whose routers change ports can reach it.</summary>
        public bool MapRouterPort { get; set; } = true;
        /// <summary>Null = the real router (tests point it at a stand-in).</summary>
        public PortMappingOptions? RouterMapping { get; set; }
    }

    public enum ViewerSessionState
    {
        Starting = 0,
        /// <summary>Asking the host through the signaling server.</summary>
        Joining = 1,
        /// <summary>Opening the path.</summary>
        Punching = 2,
        /// <summary>The path works; mpv can play.</summary>
        Connected = 3,
        /// <summary>Not connected; trying again after a pause (the status says why).</summary>
        Waiting = 4,
        Stopped = 5,
    }

    /// <summary>
    /// Watching one host (receiver side). Keeps a punched path to the host alive underneath mpv: joins, punches,
    /// and when the path drops, joins again with a new session on the same local port, so mpv's retry loop simply
    /// reconnects to srt://127.0.0.1:<see cref="LocalPort"/>. When something fails the status explains whose side
    /// lacks what (see <see cref="P2pDiagnostics"/>).
    /// </summary>
    public sealed class ViewerSession : IDisposable
    {
        readonly ViewerSessionOptions _o;
        readonly Action<string> _log;
        readonly object _lock = new object();
        readonly CancellationTokenSource _cts = new CancellationTokenSource();
        GroupKeys? _keys;
        SignalingClient? _signal;
        ViewerLink? _link;
        RouterPortMapping? _router;
        Thread? _thread;
        ViewerSessionState _state = ViewerSessionState.Starting;
        string _status = "Starting";
        string _hostName = "";
        string _echoCandidate = "";
        bool _everConnected;
        bool _ipv4OnlyFailure;

        public ViewerSession(ViewerSessionOptions options, Action<string> log)
        {
            _o = options ?? throw new ArgumentNullException(nameof(options));
            _log = log ?? (_ => { });
        }

        public event Action? Changed;

        public ViewerSessionState State { get { lock (_lock) return _state; } }
        public string Status { get { lock (_lock) return _status; } }
        public string HostName { get { lock (_lock) return _hostName; } }
        public int LocalPort => _link?.LocalPort ?? 0;
        public string PathDescription => _link?.DescribePath() ?? "";
        public bool EverConnected { get { lock (_lock) return _everConnected; } }
        public string SrtPassphrase => _keys?.SrtPassphrase ?? "";
        /// <summary>The router forward of this PC's IPv4 port (null when not asked for).</summary>
        public RouterPortMapping? RouterMapping => _router;
        /// <summary>
        /// The last attempt failed on IPv4 alone and the host's side is not known to be open: if the host's router
        /// changes ports, only this PC taking unsolicited IPv4 (router forward + firewall rule) can help.
        /// </summary>
        public bool ThisSideCouldHelp { get { lock (_lock) return _ipv4OnlyFailure; } }

        public void Start()
        {
            _keys = _o.Group.Keys();
            _signal = new SignalingClient(_o.Group);
            _link = new ViewerLink(_keys, _o.Addresses ?? LinkAddresses.Detect(), _o.LocalPort);
            _link.StateChanged += _ => Changed?.Invoke();
            _log("[p2p] this PC offers: " + string.Join(", ", _link.Candidates) + (_link.HasIpv6 ? "" : "  (no IPv6)"));
            if (_o.MapRouterPort && _link.Ipv4Port.HasValue)
            {
                ViewerLink link = _link;
                var router = new RouterPortMapping(link.Ipv4Port.Value, _o.RouterMapping, _log);
                router.Changed += () =>
                {
                    IPEndPoint? ext = router.External;
                    link.SetCandidate(LinkBase.SourceRouter, router.Open && ext != null ? Endpoint.Format(ext) : null);
                    Changed?.Invoke();
                };
                _router = router;
                router.Start();
            }
            _thread = new Thread(Run) { IsBackground = true, Name = "p2p-viewer" };
            _thread.Start();
        }

        void Run()
        {
            CancellationToken ct = _cts.Token;
            bool echoTried = false;
            int failures = 0;
            while (!ct.IsCancellationRequested)
            {
                if (_o.DiscoverPublicIpv4)
                {
                    // before every join: the echo may have been opened meanwhile, and the router may have dropped the
                    // mapping while we waited, so the address the host would punch towards could be stale
                    DiscoverPublicIpv4(logFailure: !echoTried);
                    echoTried = true;
                    if (ct.IsCancellationRequested) return;
                }
                string waitReason;
                try
                {
                    waitReason = JoinOnce(ct);
                }
                catch (OperationCanceledException)
                {
                    return;
                }
                catch (SignalingException ex)
                {
                    if (ct.IsCancellationRequested) return;
                    waitReason = ex.Unreachable ? P2pDiagnostics.SignalingDown : "Signaling error: " + ex.Message;
                }
                if (ct.IsCancellationRequested) return;
                if (waitReason.Length == 0)
                {
                    failures = 0;
                    // Connected: stay until the path ends.
                    while (!ct.IsCancellationRequested && _link!.State == LinkState.Connected) ct.WaitHandle.WaitOne(200);
                    if (ct.IsCancellationRequested) return;
                    string reason = _link!.Reason;
                    bool ended = reason.IndexOf("ended", StringComparison.OrdinalIgnoreCase) >= 0 ||
                                 reason.IndexOf("stopped", StringComparison.OrdinalIgnoreCase) >= 0;
                    _log("[p2p] path to " + HostName + " closed: " + reason);
                    SetState(ViewerSessionState.Waiting, ended ? "The stream ended - waiting for " + HostName + " to go live again" : "Connection lost (" + reason + ") - reconnecting");
                    ct.WaitHandle.WaitOne(ended ? 5000 : 1000);
                    continue;
                }
                failures++;
                int delay = failures <= 1 ? 3000 : failures == 2 ? 6000 : failures == 3 ? 12000 : 30000;
                SetState(ViewerSessionState.Waiting, waitReason + " (retrying in " + delay / 1000 + " s)");
                ct.WaitHandle.WaitOne(delay);
            }
        }

        /// <summary>One join attempt. Returns "" when connected, otherwise why not.</summary>
        string JoinOnce(CancellationToken ct)
        {
            GroupConfig g = _o.Group;
            SetState(ViewerSessionState.Joining, "Looking for the host");
            LiveEntry? entry = _signal!.GetLive(ct).FirstOrDefault(e => e.Record.HostId == _o.HostId);
            if (entry == null) return "Not live right now";
            lock (_lock) _hostName = entry.Record.Name;
            if (entry.Record.MaxViewers > 0 && entry.Record.Viewers >= entry.Record.MaxViewers)
                return entry.Record.Name + " is full (" + entry.Record.MaxViewers + " viewers)";

            string sid = GroupKeys.NewId();
            _link!.StartSession(sid, entry.Record.Candidates);          // punch right away with the advertised candidates
            var join = new JoinRequest
            {
                SessionId = sid, ViewerId = g.PeerId, Name = g.DisplayName, TimeMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(),
                Candidates = _link.Candidates.ToList(), LatencyMs = _o.LatencyMs, OverheadPercent = _o.OverheadPercent,
            };
            try
            {
                _signal.PostJoin(_o.HostId, SignedEnvelope.Seal(_keys!, SignedEnvelope.KindJoin, g.GroupId, _o.HostId, join.ToPayload()), ct);
            }
            catch (SignalingException ex) when (ex.Status == 404)
            {
                _link.EndSession("host offline");
                return "Not live right now";
            }
            catch (SignalingException ex) when (ex.Status == 429)
            {
                _link.EndSession("busy");
                return entry.Record.Name + " is busy, too many people joining at once";
            }

            SetState(ViewerSessionState.Punching, "Connecting to " + entry.Record.Name);
            SignedEnvelope? answerEnv = _signal.WaitAnswer(sid, 20, ct);
            if (answerEnv != null && answerEnv.TryOpen(_keys!, SignedEnvelope.KindAnswer, g.GroupId, sid, out var payload))
            {
                JoinAnswer? answer = JoinAnswer.FromPayload(payload!, sid);
                if (answer != null && !answer.Ok)
                {
                    _link.EndSession("refused");
                    return answer.Reason == JoinAnswer.ReasonFull ? entry.Record.Name + " is full"
                         : answer.Reason == JoinAnswer.ReasonStopped ? entry.Record.Name + " is stopping"
                         : entry.Record.Name + " could not take another viewer";
                }
                if (answer != null) _link.AddHostCandidates(answer.Candidates);
            }

            var deadline = DateTime.UtcNow.AddMilliseconds(PeerSession.PunchDeadlineUs / 1000 + 1000);
            while (!ct.IsCancellationRequested && DateTime.UtcNow < deadline)
            {
                LinkState s = _link.State;
                if (s == LinkState.Connected)
                {
                    lock (_lock)
                    {
                        _everConnected = true;
                        _ipv4OnlyFailure = false;
                    }
                    SetState(ViewerSessionState.Connected, "Connected to " + entry.Record.Name + " (" + _link.DescribePath() + ")");
                    _log("[p2p] connected to " + entry.Record.Name + ": " + _link.DescribePath() +
                         (_link.PathLearned ? " (a port the host's router picked for this PC, not the advertised one: it changes ports per destination)" : ""));
                    return "";
                }
                if (s == LinkState.Failed || s == LinkState.Closed) break;
                ct.WaitHandle.WaitOne(100);
            }
            ct.ThrowIfCancellationRequested();
            string reason = _link.Reason;                  // before EndSession replaces it
            _link.EndSession("no path");
            bool ipv4Tried = !(_link.HasIpv6 && P2pDiagnostics.HasIpv6(entry.Record.Candidates)) &&
                             P2pDiagnostics.HasPublicIpv4(entry.Record.Candidates) && P2pDiagnostics.HasPublicIpv4(_link.Candidates);
            lock (_lock) _ipv4OnlyFailure = ipv4Tried && entry.Record.OpenIpv4 != true;
            string why = P2pDiagnostics.Explain(_link.HasIpv6, _link.Candidates, entry.Record.Candidates, entry.Record.OpenIpv4);
            _log("[p2p] could not reach " + entry.Record.Name + ": " + why + " (" + (reason.Length > 0 ? reason + "; " : "") +
                 "host offers " + string.Join(", ", entry.Record.Candidates) +
                 (entry.Record.OpenIpv4.HasValue ? ", host IPv4 " + (entry.Record.OpenIpv4.Value ? "open" : "not open") + " to every router" : "") + ")");
            return why;
        }

        /// <summary>This PC's public IPv4 mapping for the IPv4 attempt (IPv6 does not need it). False = no answer yet.</summary>
        bool DiscoverPublicIpv4(bool logFailure)
        {
            try
            {
                IPAddress? server = P2pHost.ResolveIpv4(_signal!.Host);
                if (server == null) return true;                   // IPv6-only signaling server: nothing to ask
                var echo = new IPEndPoint(server, _o.EchoPort);
                IPEndPoint? mapped = _link!.DiscoverPublicIpv4(echo, 1500);
                if (mapped == null)
                {
                    if (logFailure)
                        _log("[p2p] public IPv4 check: no answer from the signaling server's UDP echo at " + Endpoint.Format(echo) +
                             " (is UDP " + _o.EchoPort + " open on the server?); without it IPv4 only works inside one network");
                    return false;
                }
                string candidate = Endpoint.Format(mapped);
                _link.SetCandidate(LinkBase.SourceEcho, candidate);
                if (candidate != _echoCandidate)
                    _log("[p2p] public IPv4 mapping: " + candidate + (_echoCandidate.Length > 0 ? " (was " + _echoCandidate + ")" : ""));
                _echoCandidate = candidate;
                return true;
            }
            catch (Exception)
            {
                return false;
            }
        }

        void SetState(ViewerSessionState state, string status)
        {
            bool changed;
            lock (_lock)
            {
                changed = state != _state || status != _status;
                _state = state;
                _status = status;
            }
            if (changed) Changed?.Invoke();
        }

        public void Stop()
        {
            if (_cts.IsCancellationRequested) return;
            _cts.Cancel();
            _thread?.Join(3000);
            _link?.Dispose();
            _router?.Dispose();                        // deletes the router forward (waits for the router, a few seconds at most)
            SetState(ViewerSessionState.Stopped, "Stopped");
        }

        public void Dispose() => Stop();
    }

    /// <summary>Plain-language reasons for a failed connection, based on who offers which address family.</summary>
    public static class P2pDiagnostics
    {
        public const string SignalingDown = "The signaling server cannot be reached - check this PC's internet connection";

        /// <summary>
        /// Why the viewer did not get through. A public IPv4 candidate exists only when that side's UDP echo answered
        /// (docs/design.md §8.7); without one on the host there was no IPv4 path to try at all. <paramref name="hostIpv4Open"/>
        /// is the host's "open4" (router forward + firewall rule): it tells whose side kept the IPv4 attempt out.
        /// </summary>
        public static string Explain(bool thisPcHasIpv6, IList<string> thisPcCandidates, IList<string> hostCandidates,
                                     bool? hostIpv4Open = null)
        {
            bool hostV6 = HasIpv6(hostCandidates);
            if (thisPcHasIpv6 && hostV6)
                return "Both sides have IPv6 but the path is blocked - usually a firewall or VPN that drops all incoming UDP";
            string ipv4;
            if (!HasPublicIpv4(hostCandidates)) ipv4 = "the host offered no public IPv4 address";
            else if (!HasPublicIpv4(thisPcCandidates)) ipv4 = "the IPv4 attempt did not get through (this PC's public IPv4 address could not be found)";
            else if (hostIpv4Open == false)
                ipv4 = "the IPv4 attempt did not get through: the host's PC only takes IPv4 from routers that keep ports, " +
                       "so they should click Allow next to Firewall in HdrCast and turn UPnP on in their router";
            else if (hostIpv4Open == true)
                ipv4 = "the IPv4 attempt did not get through although the host takes IPv4 from any port: " +
                       "something on this side drops UDP (a VPN, proxy or game accelerator?)";
            else ipv4 = "the IPv4 attempt did not get through";
            if (hostV6)
                return "This PC has no IPv6 internet connection, and " + ipv4 + ". " +
                       "Turn on IPv6 on your router or ask your ISP (check at test-ipv6.com)";
            if (thisPcHasIpv6)
                return "The host's PC has no IPv6 internet connection, and " + ipv4 + "; they need to turn IPv6 on (check at test-ipv6.com)";
            return "Neither side has IPv6, and " + ipv4 + "; at least one side needs IPv6 (check at test-ipv6.com)";
        }

        /// <summary>A global IPv6 candidate (loopback counts too, for tests).</summary>
        public static bool HasIpv6(IEnumerable<string> candidates) =>
            AnyAddress(candidates, a => a.AddressFamily == AddressFamily.InterNetworkV6 && (LocalAddresses.IsGlobalUnicastV6(a) || IPAddress.IsLoopback(a)));

        /// <summary>An IPv4 candidate outside the private, shared (CGNAT), loopback and link-local ranges.</summary>
        public static bool HasPublicIpv4(IEnumerable<string> candidates) =>
            AnyAddress(candidates, LocalAddresses.IsPublicIpv4);

        static bool AnyAddress(IEnumerable<string> candidates, Func<IPAddress, bool> test)
        {
            foreach (string c in candidates)
                if (Endpoint.TryParse(c, out HostPort? hp, out _) && hp!.IsLiteral && test(IPAddress.Parse(hp.Host))) return true;
            return false;
        }
    }
}
