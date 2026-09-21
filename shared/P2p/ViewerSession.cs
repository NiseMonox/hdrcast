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
        Thread? _thread;
        ViewerSessionState _state = ViewerSessionState.Starting;
        string _status = "Starting";
        string _hostName = "";
        bool _everConnected;

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

        public void Start()
        {
            _keys = _o.Group.Keys();
            _signal = new SignalingClient(_o.Group);
            _link = new ViewerLink(_keys, _o.Addresses ?? LinkAddresses.Detect(), _o.LocalPort);
            _link.StateChanged += _ => Changed?.Invoke();
            _log("[p2p] this PC offers: " + string.Join(", ", _link.Candidates) + (_link.HasIpv6 ? "" : "  (no IPv6)"));
            _thread = new Thread(Run) { IsBackground = true, Name = "p2p-viewer" };
            _thread.Start();
        }

        void Run()
        {
            CancellationToken ct = _cts.Token;
            if (_o.DiscoverPublicIpv4) DiscoverPublicIpv4();
            int failures = 0;
            while (!ct.IsCancellationRequested)
            {
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
                    lock (_lock) _everConnected = true;
                    SetState(ViewerSessionState.Connected, "Connected to " + entry.Record.Name + " (" + _link.DescribePath() + ")");
                    _log("[p2p] connected to " + entry.Record.Name + ": " + _link.DescribePath());
                    return "";
                }
                if (s == LinkState.Failed || s == LinkState.Closed) break;
                ct.WaitHandle.WaitOne(100);
            }
            ct.ThrowIfCancellationRequested();
            _link.EndSession("no path");
            string why = P2pDiagnostics.Explain(_link.HasIpv6, entry.Record.Candidates);
            _log("[p2p] could not reach " + entry.Record.Name + ": " + why + " (host offers " + string.Join(", ", entry.Record.Candidates) + ")");
            return why;
        }

        void DiscoverPublicIpv4()
        {
            try
            {
                IPAddress? server = P2pHost.ResolveIpv4(_signal!.Host);
                if (server == null) return;
                IPEndPoint? mapped = _link!.DiscoverPublicIpv4(new IPEndPoint(server, _o.EchoPort), 1500);
                if (mapped != null) _link.AddCandidate(Endpoint.Format(mapped));
            }
            catch (Exception)
            {
                // optional extra: IPv6 does not need it
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
            SetState(ViewerSessionState.Stopped, "Stopped");
        }

        public void Dispose() => Stop();
    }

    /// <summary>Plain-language reasons for a failed connection, based on who offers which address family.</summary>
    public static class P2pDiagnostics
    {
        public const string SignalingDown = "The signaling server cannot be reached - check this PC's internet connection";

        public static string Explain(bool thisPcHasIpv6, IList<string> hostCandidates)
        {
            bool hostV6 = false, hostPublicV4 = false;
            foreach (string c in hostCandidates)
            {
                if (!Endpoint.TryParse(c, out HostPort? hp, out _) || !hp!.IsLiteral) continue;
                IPAddress a = IPAddress.Parse(hp.Host);
                if (a.AddressFamily == AddressFamily.InterNetworkV6) hostV6 |= LocalAddresses.IsGlobalUnicastV6(a) || IPAddress.IsLoopback(a);
                else hostPublicV4 |= !IsPrivateV4(a);
            }
            if (!thisPcHasIpv6 && hostV6)
                return "This PC has no IPv6 internet connection, and the host is reachable over IPv6 only. " +
                       "Turn on IPv6 on your router or ask your ISP (check at test-ipv6.com)";
            if (thisPcHasIpv6 && !hostV6)
                return "The host's PC has no IPv6 internet connection; they need to turn IPv6 on (check at test-ipv6.com)";
            if (!thisPcHasIpv6 && !hostV6)
                return hostPublicV4
                    ? "Neither side has IPv6 and the IPv4 attempt did not get through; at least one side needs IPv6"
                    : "Neither side has IPv6; at least one side needs IPv6 (check at test-ipv6.com)";
            return "Both sides have IPv6 but the path is blocked - usually a firewall or VPN that drops all incoming UDP";
        }

        static bool IsPrivateV4(IPAddress a)
        {
            byte[] b = a.GetAddressBytes();
            return b[0] == 10 || b[0] == 127 || (b[0] == 172 && b[1] >= 16 && b[1] <= 31) || (b[0] == 192 && b[1] == 168) ||
                   (b[0] == 100 && b[1] >= 64 && b[1] <= 127) || (b[0] == 169 && b[1] == 254);
        }
    }
}
