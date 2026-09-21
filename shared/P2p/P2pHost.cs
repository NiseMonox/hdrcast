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
            public Viewer(ViewerInfo info) { Info = info; }
            public ViewerInfo Info;
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
        TsFanOut? _fan;
        Thread? _heartbeat;
        Thread? _joins;
        long _sinceMs;
        string _status = "Starting";
        volatile bool _stopped;

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
            bool registered = false, echoed = !_o.DiscoverPublicIpv4;
            int failures = 0;
            while (!ct.IsCancellationRequested)
            {
                try
                {
                    if (!echoed)
                    {
                        echoed = true;
                        DiscoverPublicIpv4();
                    }
                    if (!registered)
                    {
                        _signal!.RegisterGroup(ct);
                        registered = true;
                    }
                    _signal!.PutLive(_o.Group.PeerId, SealLive(), ct);
                    if (failures > 0) _log("[p2p] signaling server reachable again");
                    failures = 0;
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

        void DiscoverPublicIpv4()
        {
            try
            {
                IPAddress? server = ResolveIpv4(_signal!.Host);
                if (server == null) return;
                IPEndPoint? mapped = _link!.DiscoverPublicIpv4(new IPEndPoint(server, _o.EchoPort), 1500);
                if (mapped == null) return;
                string candidate = Endpoint.Format(mapped);
                _link.AddCandidate(candidate);
                _log("[p2p] public IPv4 mapping: " + candidate);
            }
            catch (Exception ex)
            {
                _log("[p2p] public IPv4 check skipped: " + ex.Message);
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
            });
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
                _log("[p2p] " + v.Info.Name + " connected via " + (info.PathIsIpv6 ? "IPv6 " : "IPv4 ") + info.Path);
            }
            else if (info.State == LinkState.Closed || info.State == LinkState.Failed)
            {
                EndViewer(v, info.State == LinkState.Failed ? "could not connect: " + info.Reason : info.Reason);
            }
            Changed?.Invoke();
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
                MaxViewers = _o.MaxViewers, Viewers = Viewers.Count,
            };
            return SignedEnvelope.Seal(_keys!, SignedEnvelope.KindLive, g.GroupId, g.PeerId, rec.ToPayload());
        }

        string DescribeLive()
        {
            int n = Viewers.Count;
            return "Live in \"" + _o.Group.GroupName + "\" - " + n + (n == 1 ? " viewer" : " viewers") +
                   (_link!.HasIpv6 ? "" : " (no IPv6 on this PC: only IPv4 viewers on the same network can join)");
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
