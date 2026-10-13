using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Net;
using System.Threading;
using HdrCast.Common;
using HdrCast.Group;

namespace HdrCast.P2p
{
    public sealed class P2pHostOptions
    {
        public GroupConfig Group { get; set; } = new GroupConfig();
        public string FfmpegPath { get; set; } = "ffmpeg";
        public long BitrateBps { get; set; } = 40_000_000;
        public string Codec { get; set; } = "hevc";
        public int MaxViewers { get; set; } = 4;
        public ProcessJob? Job { get; set; }
        /// <summary>Null = detect this PC's stable IPv6 and LAN IPv4.</summary>
        public LinkAddresses? Addresses { get; set; }
        /// <summary>Ask the signaling server's UDP echo for the public IPv4 mapping (the extra IPv4 attempt).</summary>
        public bool DiscoverPublicIpv4 { get; set; } = true;
        public int EchoPort { get; set; } = EchoProtocol.DefaultPort;
        public int HeartbeatMs { get; set; } = 10_000;
        /// <summary>Forward the IPv4 port on the router (UPnP) while live, so viewers whose routers change ports get in.</summary>
        public bool MapRouterPort { get; set; } = true;
        /// <summary>Null = the real router (tests point it at a stand-in).</summary>
        public PortMappingOptions? RouterMapping { get; set; }
        /// <summary>
        /// Whether the firewall lets unsolicited UDP reach this program: true (a rule, or no firewall), false, or null
        /// (unknown). Supplied by the GUI, which owns the Windows Firewall check.
        /// </summary>
        public Func<bool?>? InboundAllowed { get; set; }
    }

    public enum ViewerState
    {
        Connecting = 0,
        Watching = 1,
    }

    public sealed class ViewerInfo
    {
        public string Sid { get; set; } = "";
        public string Name { get; set; } = "";
        public ViewerState State { get; set; }
        public string Path { get; set; } = "";
        public bool Ipv6 { get; set; }
        public double RttMs { get; set; } = -1;
        public DateTime JoinedUtc { get; set; }
        public int LatencyMs { get; set; }
        public int OverheadPercent { get; set; }
    }

    /// <summary>
    /// Streaming to the group (sender side): advertises this host every 10 s, answers join requests, starts one
    /// remux child per viewer fed from the shared encoder output, and lets <see cref="HostLink"/> punch through to
    /// each viewer. Viewers come and go without touching the encoder.
    /// </summary>
    public sealed class P2pHost : IDisposable
    {
        public const int MaxChildRestartsPerMinute = 6;

        sealed class Viewer
        {
            public Viewer(ViewerInfo info, IList<string> candidates) { Info = info; Candidates = candidates; }
            public ViewerInfo Info;
            public readonly IList<string> Candidates;
            public RemuxChild? Child;
            public int ChildPort;
            public bool Ended;
            public readonly Queue<DateTime> Restarts = new Queue<DateTime>();
        }

        readonly P2pHostOptions _o;
        readonly Stream _ts;
        readonly Action<string> _log;
        readonly object _lock = new object();
        readonly Dictionary<string, Viewer> _viewers = new Dictionary<string, Viewer>();
        readonly Dictionary<string, DateTime> _seenSids = new Dictionary<string, DateTime>();
        readonly CancellationTokenSource _cts = new CancellationTokenSource();
        GroupKeys? _keys;
        SignalingClient? _signal;
        HostLink? _link;
        RouterPortMapping? _router;
        TsFanOut? _fan;
        Thread? _heartbeat;
        Thread? _joins;
        long _sinceMs;
        string _status = "Starting";
        volatile bool _stopped;
        volatile bool _announced;

        public P2pHost(P2pHostOptions options, Stream tsInput, Action<string> log)
        {
            _o = options ?? throw new ArgumentNullException(nameof(options));
            _ts = tsInput ?? throw new ArgumentNullException(nameof(tsInput));
            _log = log ?? (_ => { });
        }

        /// <summary>The viewer list or the status line changed (raised on a background thread).</summary>
        public event Action? Changed;
        /// <summary>The encoder's stream ended.</summary>
        public event Action? InputEnded;

        public string Status { get { lock (_lock) return _status; } }
        public IList<string> Candidates => _link?.Candidates ?? new string[0];
        public bool HasIpv6 => _link?.HasIpv6 ?? false;
        /// <summary>The router forward of the IPv4 port (null when not asked for).</summary>
        public RouterPortMapping? RouterMapping => _router;

        /// <summary>
        /// Viewers can reach the IPv4 port from any address and port: the router forwards it (or there is no router)
        /// and the firewall lets it in. Null while that is still being found out. Advertised as "open4".
        /// </summary>
        public bool? Ipv4Open
        {
            get
            {
                RouterPortMapping? r = _router;
                if (r == null) return null;
                PortMappingState s = r.State;
                if (s == PortMappingState.Off || s == PortMappingState.Working) return null;
                bool? firewall = _o.InboundAllowed?.Invoke();
                if (!r.Open || firewall == false) return false;
                return firewall;
            }
        }

        public List<ViewerInfo> Viewers
        {
            get
            {
                lock (_lock) return _viewers.Values.Where(v => !v.Ended).Select(v => Copy(v.Info)).OrderBy(v => v.JoinedUtc).ToList();
            }
        }

        /// <summary>Upload this stream needs right now: bitrate × viewers × (1 + each viewer's SRT overhead).</summary>
        public double UploadMbps
        {
            get
            {
                lock (_lock)
                    return _viewers.Values.Where(v => !v.Ended)
                                   .Sum(v => _o.BitrateBps / 1e6 * (1 + v.Info.OverheadPercent / 100.0));
            }
        }

        public static double UploadMbpsFor(long bitrateBps, int viewers, int overheadPercent) =>
            bitrateBps / 1e6 * viewers * (1 + overheadPercent / 100.0);

        public void Start()
        {
            GroupConfig g = _o.Group;
            _keys = g.Keys();
            _signal = new SignalingClient(g);
            _link = new HostLink(_keys, _o.Addresses ?? LinkAddresses.Detect());
            _link.SessionChanged += OnSessionChanged;
            _sinceMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
            _log("[p2p] candidates: " + string.Join(", ", _link.Candidates) + (_link.HasIpv6 ? "" : "  (no IPv6 on this PC)"));
            if (_o.MapRouterPort && _link.Ipv4Port.HasValue)
            {
                HostLink link = _link;
                var router = new RouterPortMapping(link.Ipv4Port.Value, _o.RouterMapping, _log);
                router.Changed += () =>
                {
                    IPEndPoint? ext = router.External;
                    link.SetCandidate(LinkBase.SourceRouter, router.Open && ext != null ? Endpoint.Format(ext) : null);
                    if (_announced && !_stopped) SetStatus(DescribeLive());
                };
                _router = router;
                router.Start();
            }

            _fan = new TsFanOut(_ts);
            _fan.SinkFailed += OnSinkFailed;
            _fan.InputEnded += () => { if (!_stopped) { _log("[p2p] the encoder output ended"); InputEnded?.Invoke(); } };
            _fan.Start();

            _heartbeat = new Thread(HeartbeatLoop) { IsBackground = true, Name = "p2p-heartbeat" };
            _joins = new Thread(JoinLoop) { IsBackground = true, Name = "p2p-joins" };
            _heartbeat.Start();
            _joins.Start();
        }

        void HeartbeatLoop()
        {
            CancellationToken ct = _cts.Token;
            bool registered = false, needEcho = _o.DiscoverPublicIpv4, echoTried = false;
            DateTime nextEchoUtc = DateTime.MinValue;
            int failures = 0;
            while (!ct.IsCancellationRequested)
            {
                try
                {
                    if (needEcho && DateTime.UtcNow >= nextEchoUtc)
                    {
                        // until the echo answers, ask again every minute (the port may be opened while live)
                        needEcho = !DiscoverPublicIpv4(logFailure: !echoTried);
                        echoTried = true;
                        nextEchoUtc = DateTime.UtcNow.AddMinutes(1);
                    }
                    if (!registered)
                    {
                        _signal!.RegisterGroup(ct);
                        registered = true;
                    }
                    _signal!.PutLive(_o.Group.PeerId, SealLive(), ct);
                    if (failures > 0) _log("[p2p] signaling server reachable again");
                    failures = 0;
                    _announced = true;
                    SetStatus(DescribeLive());
                    ct.WaitHandle.WaitOne(_o.HeartbeatMs);
                }
                catch (OperationCanceledException)
                {
                    return;
                }
                catch (SignalingException ex)
                {
                    if (ct.IsCancellationRequested) return;
                    if (ex.Status == 404) registered = false;
                    if (ex.Status == 409)
                    {
                        SetStatus("This group's id is registered with a different key on the signaling server. Create a new group.");
                        _log("[p2p] " + ex.Message);
                        return;
                    }
                    failures++;
                    int delay = Backoff(failures);
                    if (failures == 1 || failures % 10 == 0) _log("[p2p] " + ex.Message + " - retrying");
                    SetStatus("Signaling server unreachable - retrying in " + delay / 1000 + " s (viewers already watching are not affected)");
                    ct.WaitHandle.WaitOne(delay);
                }
            }
        }

        /// <summary>
        /// This PC's public IPv4 mapping, the only IPv4 candidate a viewer outside this network can use. False = no
        /// answer yet (worth retrying).
        /// </summary>
        bool DiscoverPublicIpv4(bool logFailure)
        {
            try
            {
                IPAddress? server = ResolveIpv4(_signal!.Host);
                if (server == null) return true;                   // IPv6-only signaling server: nothing to ask
                var echo = new IPEndPoint(server, _o.EchoPort);
                IPEndPoint? mapped = _link!.DiscoverPublicIpv4(echo, 1500);
                if (mapped == null)
                {
                    if (logFailure)
                        _log("[p2p] public IPv4 check: no answer from the signaling server's UDP echo at " + Endpoint.Format(echo) +
                             ", so this PC advertises no public IPv4 and viewers without IPv6 cannot reach it from outside this network. " +
                             "Is UDP " + _o.EchoPort + " open in the server's firewall / cloud security group? Retrying every minute.");
                    return false;
                }
                string candidate = Endpoint.Format(mapped);
                _link.SetCandidate(LinkBase.SourceEcho, candidate);
                _log("[p2p] public IPv4 mapping: " + candidate);
                return true;
            }
            catch (Exception ex)
            {
                if (logFailure) _log("[p2p] public IPv4 check skipped: " + ex.Message);
                return false;
            }
        }

        internal static IPAddress? ResolveIpv4(string host)
        {
            if (IPAddress.TryParse(host, out IPAddress? literal))
                return literal!.AddressFamily == System.Net.Sockets.AddressFamily.InterNetwork ? literal : null;
            foreach (IPAddress a in Dns.GetHostAddresses(host))
                if (a.AddressFamily == System.Net.Sockets.AddressFamily.InterNetwork) return a;
            return null;
        }

        void JoinLoop()
        {
            CancellationToken ct = _cts.Token;
            long after = 0;
            int failures = 0;
            while (!ct.IsCancellationRequested)
            {
                try
                {
                    var joins = _signal!.PollJoins(_o.Group.PeerId, after, 20, out long next, ct);
                    after = next;
                    failures = 0;
                    foreach (var kv in joins) HandleJoin(kv.Value);
                }
                catch (OperationCanceledException)
                {
                    return;
                }
                catch (SignalingException ex)
                {
                    if (ct.IsCancellationRequested) return;
                    // A restarted server numbers its mailboxes from 1 again; duplicates are dropped by session id.
                    if (ex.Unreachable) after = 0;
                    failures++;
                    ct.WaitHandle.WaitOne(ex.Status == 404 ? 1000 : Backoff(failures));
                }
            }
        }

        void HandleJoin(SignedEnvelope env)
        {
            GroupConfig g = _o.Group;
            if (!env.TryOpen(_keys!, SignedEnvelope.KindJoin, g.GroupId, g.PeerId, out var payload)) return;
            JoinRequest? join = JoinRequest.FromPayload(payload!);
            if (join == null) return;
            long now = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
            if (Math.Abs(now - join.TimeMs) > 10 * 60 * 1000) return;

            string refusal = "";
            lock (_lock)
            {
                foreach (string old in _seenSids.Where(kv => (DateTime.UtcNow - kv.Value).TotalMinutes > 10).Select(kv => kv.Key).ToList())
                    _seenSids.Remove(old);
                if (_seenSids.ContainsKey(join.SessionId)) return;
                _seenSids[join.SessionId] = DateTime.UtcNow;
                if (_stopped) refusal = JoinAnswer.ReasonStopped;
                else if (_viewers.Values.Count(v => !v.Ended) >= _o.MaxViewers) refusal = JoinAnswer.ReasonFull;
            }
            if (refusal.Length > 0)
            {
                _log("[p2p] " + join.Name + " wanted to join: " + refusal);
                Answer(join.SessionId, false, refusal);
                return;
            }

            var viewer = new Viewer(new ViewerInfo
            {
                Sid = join.SessionId, Name = join.Name, State = ViewerState.Connecting, JoinedUtc = DateTime.UtcNow,
                LatencyMs = join.LatencyMs, OverheadPercent = join.OverheadPercent,
            }, join.Candidates);
            try
            {
                StartChild(viewer);
            }
            catch (Exception ex)
            {
                _log("[p2p] cannot start ffmpeg for " + join.Name + ": " + ex.Message);
                Answer(join.SessionId, false, JoinAnswer.ReasonBusy);
                return;
            }
            lock (_lock) _viewers[join.SessionId] = viewer;
            _link!.AddSession(join.SessionId, join.Candidates, viewer.ChildPort, join.SessionId);
            _log("[p2p] " + join.Name + " is joining (latency " + join.LatencyMs + " ms, candidates " + string.Join(", ", join.Candidates) + ")");
            Answer(join.SessionId, true, "");
            Changed?.Invoke();
        }

        void StartChild(Viewer v)
        {
            int port = RemuxChild.FreeLoopbackUdpPort();
            string url = RemuxChild.BuildListenerUrl(port, _keys!.SrtPassphrase, v.Info.LatencyMs, v.Info.OverheadPercent, _o.BitrateBps);
            var child = new RemuxChild(_o.FfmpegPath, url, _o.Job);
            string name = v.Info.Name;
            child.Log += line => _log("[remux " + name + "] " + line);
            child.Exited += code => OnChildExited(v, child, code);
            child.Start();
            v.Child = child;
            v.ChildPort = port;
            _fan!.AddSink(v.Info.Sid, child.Input, TsFanOut.QueueBytesFor(_o.BitrateBps));
        }

        /// <summary>A child ends when its SRT connection ends (the viewer's mpv restarted): give the viewer a fresh one.</summary>
        void OnChildExited(Viewer v, RemuxChild child, int code)
        {
            bool restart;
            lock (_lock)
            {
                restart = !_stopped && !v.Ended && v.Child == child;
                if (restart)
                {
                    while (v.Restarts.Count > 0 && (DateTime.UtcNow - v.Restarts.Peek()).TotalSeconds > 60) v.Restarts.Dequeue();
                    v.Restarts.Enqueue(DateTime.UtcNow);
                    if (v.Restarts.Count > MaxChildRestartsPerMinute) restart = false;
                }
            }
            child.Dispose();
            if (!restart)
            {
                if (!v.Ended && !_stopped)
                {
                    _log("[p2p] ffmpeg for " + v.Info.Name + " keeps exiting (code " + code + "); ending that viewer");
                    _link?.RemoveSession(v.Info.Sid, "the host could not serve this viewer");
                }
                return;
            }
            try
            {
                StartChild(v);
                _link?.RetargetSession(v.Info.Sid, v.ChildPort);
            }
            catch (Exception ex)
            {
                _log("[p2p] cannot restart ffmpeg for " + v.Info.Name + ": " + ex.Message);
                _link?.RemoveSession(v.Info.Sid, "the host could not serve this viewer");
            }
        }

        void OnSinkFailed(string sid)
        {
            // the child's stdin closed: OnChildExited handles the restart
        }

        void OnSessionChanged(HostSessionInfo info)
        {
            Viewer? v;
            lock (_lock) _viewers.TryGetValue(info.Sid, out v);
            if (v == null) return;
            if (info.State == LinkState.Connected)
            {
                lock (_lock)
                {
                    v.Info.State = ViewerState.Watching;
                    v.Info.Path = info.Path;
                    v.Info.Ipv6 = info.PathIsIpv6;
                    v.Info.RttMs = info.RttMs;
                }
                _log("[p2p] " + v.Info.Name + " connected via " + (info.PathIsIpv6 ? "IPv6 " : "IPv4 ") + info.Path +
                     (info.PathLearned ? " (a port their router picked for this PC, not the advertised one: it changes ports per destination)" : ""));
            }
            else if (info.State == LinkState.Closed || info.State == LinkState.Failed)
            {
                EndViewer(v, info.State == LinkState.Failed ? "could not connect: " + info.Reason + ExplainFailure(v, info.Reason) : info.Reason);
            }
            Changed?.Invoke();
        }

        /// <summary>For the log when a viewer never got through: whose side most likely kept it out.</summary>
        string ExplainFailure(Viewer v, string reason)
        {
            if (!reason.StartsWith("no candidate", StringComparison.Ordinal)) return "";      // one-way: a firewall on their side
            if (HasIpv6 && P2pDiagnostics.HasIpv6(v.Candidates)) return "; both have IPv6, so something drops UDP (a VPN, or a strict router firewall)";
            if (!P2pDiagnostics.HasPublicIpv4(v.Candidates)) return "; they have no IPv6 and offered no public IPv4 address";
            bool? open = Ipv4Open;
            if (open == true)
                return "; nothing of theirs arrived although this PC takes IPv4 from any port: their side drops UDP (VPN, proxy, game accelerator?) or they need IPv6";
            if (open == false)
                return "; if their router changes ports per destination, this PC must take IPv4 from any port first: " + ClosedIpv4Parts();
            return "";
        }

        /// <summary>What keeps the IPv4 port from taking packets from any address and port, in words.</summary>
        string ClosedIpv4Parts()
        {
            var parts = new List<string>();
            RouterPortMapping? r = _router;
            if (r != null && !r.Open) parts.Add(r.BehindAnotherNat ? "router behind another NAT" : "no router forward (UPnP)");
            if (_o.InboundAllowed?.Invoke() == false) parts.Add("no firewall rule (click Allow)");
            return parts.Count == 0 ? "unknown" : string.Join(", ", parts.ToArray());
        }

        void EndViewer(Viewer v, string reason)
        {
            RemuxChild? child;
            lock (_lock)
            {
                if (v.Ended) return;
                v.Ended = true;
                child = v.Child;
                v.Child = null;
                _viewers.Remove(v.Info.Sid);
            }
            _fan?.RemoveSink(v.Info.Sid);
            _log("[p2p] " + v.Info.Name + " left (" + reason + ")");
            if (child != null) ThreadPool.QueueUserWorkItem(_ => child.Dispose());
        }

        void Answer(string sid, bool ok, string reason)
        {
            var a = new JoinAnswer
            {
                SessionId = sid, Ok = ok, Reason = reason, TimeMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(),
                Candidates = ok ? _link!.Candidates.ToList() : new List<string>(),
            };
            SignedEnvelope env = SignedEnvelope.Seal(_keys!, SignedEnvelope.KindAnswer, _o.Group.GroupId, sid, a.ToPayload());
            for (int attempt = 0; attempt < 2; attempt++)
            {
                try
                {
                    _signal!.PutAnswer(sid, env, _cts.Token);
                    return;
                }
                catch (SignalingException ex)
                {
                    if (attempt == 1) _log("[p2p] could not answer a join: " + ex.Message);
                }
                catch (OperationCanceledException)
                {
                    return;
                }
            }
        }

        SignedEnvelope SealLive()
        {
            GroupConfig g = _o.Group;
            var rec = new LiveRecord
            {
                HostId = g.PeerId, Name = g.DisplayName, TimeMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(), SinceMs = _sinceMs,
                Candidates = _link!.Candidates.ToList(), Codec = _o.Codec, Mbps = (int)Math.Round(_o.BitrateBps / 1e6),
                MaxViewers = _o.MaxViewers, Viewers = Viewers.Count, OpenIpv4 = Ipv4Open,
            };
            return SignedEnvelope.Seal(_keys!, SignedEnvelope.KindLive, g.GroupId, g.PeerId, rec.ToPayload());
        }

        string DescribeLive()
        {
            int n = Viewers.Count;
            bool publicV4 = P2pDiagnostics.HasPublicIpv4(_link!.Candidates);
            string reach = _link.HasIpv6
                ? (publicV4 ? "" : " (no public IPv4 found: viewers without IPv6 can only join from this network)")
                : (publicV4 ? " (no IPv6 on this PC: IPv4 only, which some viewers' routers block)"
                            : " (no IPv6 on this PC: only IPv4 viewers on the same network can join)");
            if (publicV4 && Ipv4Open == false)
                reach += " (IPv4 closed to routers that change ports: " + ClosedIpv4Parts() + ")";
            return "Live in \"" + _o.Group.GroupName + "\" - " + n + (n == 1 ? " viewer" : " viewers") + reach;
        }

        void SetStatus(string s)
        {
            bool changed;
            lock (_lock)
            {
                changed = s != _status;
                _status = s;
            }
            if (changed) Changed?.Invoke();
        }

        static int Backoff(int failures) => failures <= 1 ? 3000 : failures == 2 ? 6000 : failures == 3 ? 12000 : 30000;

        static ViewerInfo Copy(ViewerInfo i) => new ViewerInfo
        {
            Sid = i.Sid, Name = i.Name, State = i.State, Path = i.Path, Ipv6 = i.Ipv6, RttMs = i.RttMs, JoinedUtc = i.JoinedUtc,
            LatencyMs = i.LatencyMs, OverheadPercent = i.OverheadPercent,
        };

        /// <summary>Goes offline: tells the server and every viewer, stops all remux children.</summary>
        public void Stop()
        {
            if (_stopped) return;
            _stopped = true;
            _cts.Cancel();
            _heartbeat?.Join(2000);
            _joins?.Join(2000);
            try
            {
                if (_signal != null)
                {
                    int saved = _signal.TimeoutMs;
                    _signal.TimeoutMs = 2000;
                    _signal.DeleteLive(_o.Group.PeerId, CancellationToken.None);
                    _signal.TimeoutMs = saved;
                }
            }
            catch (Exception) { }
            _link?.Dispose();
            _router?.Dispose();                        // deletes the router forward (waits for the router, a few seconds at most)
            List<RemuxChild> children;
            lock (_lock)
            {
                children = _viewers.Values.Where(v => v.Child != null).Select(v => v.Child!).ToList();
                foreach (Viewer v in _viewers.Values) { v.Ended = true; v.Child = null; }
                _viewers.Clear();
            }
            // children in parallel: each kill waits for the process to exit
            var done = new CountdownEvent(children.Count);
            foreach (RemuxChild c in children)
                ThreadPool.QueueUserWorkItem(_ => { try { c.Dispose(); } finally { done.Signal(); } });
            done.Wait(5000);
            _fan?.Dispose();
            SetStatus("Offline");
        }

        public void Dispose() => Stop();
    }
}
