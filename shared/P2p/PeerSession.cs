using System;
using System.Collections.Generic;
using System.Net;
using System.Net.Sockets;
using HdrCast.Common;
using HdrCast.Group;

namespace HdrCast.P2p
{
    public enum LinkState
    {
        /// <summary>Sending PUNCH to every candidate, waiting for a PUNCH_ACK.</summary>
        Punching = 0,
        /// <summary>A path works both ways; data flows; keepalives while idle.</summary>
        Connected = 1,
        /// <summary>Ended normally (BYE, local close) or went silent after connecting.</summary>
        Closed = 2,
        /// <summary>No candidate answered within the punch deadline.</summary>
        Failed = 3,
    }

    /// <summary>One datagram the session wants sent.</summary>
    public struct Outgoing
    {
        public Outgoing(byte[] data, IPEndPoint to) { Data = data; To = to; }
        public byte[] Data { get; }
        public IPEndPoint To { get; }
    }

    /// <summary>
    /// The hole-punching state machine for one session (docs/design.md §8.6), independent of sockets so it can be
    /// driven by a fake clock in tests. Both sides PUNCH every candidate every 100 ms until they get a PUNCH_ACK;
    /// each PUNCH is answered with a PUNCH_ACK to its source, so both firewalls see outbound traffic for the
    /// 5-tuple and open it. An authentic PUNCH from an address the other side never advertised (its router gave the
    /// packets a new port) makes that address a candidate too. Thread-safe.
    /// </summary>
    public sealed class PeerSession
    {
        public const long PunchIntervalUs = 100_000;
        public const long PunchDeadlineUs = 15_000_000;
        public const long KeepaliveIntervalUs = 2_000_000;
        public const long SilenceTimeoutUs = 15_000_000;
        /// <summary>After an IPv4 path answers, wait this long for IPv6 to answer too (IPv6 wins).</summary>
        public const long Ipv6GraceUs = 200_000;
        /// <summary>Advertised plus learned addresses punched at most (learned ones need an authentic packet).</summary>
        public const int MaxRemoteCandidates = 16;

        readonly object _lock = new object();
        readonly byte[] _punchKey;
        readonly List<IPEndPoint> _remote = new List<IPEndPoint>();
        readonly HashSet<IPEndPoint> _advertised = new HashSet<IPEndPoint>();
        readonly HashSet<IPEndPoint> _confirmed = new HashSet<IPEndPoint>();
        IPEndPoint? _ackedV4;
        long _ackedV4Us;
        long _lastPunchUs = long.MinValue;
        long _lastSendUs;
        long _lastRecvUs;

        public PeerSession(GroupKeys keys, string sid, PeerRole role, long nowUs)
        {
            if (!GroupKeys.IsValidId(sid)) throw new ArgumentException("bad session id", nameof(sid));
            Sid = sid;
            SidBytes = GroupKeys.IdBytesOf(sid);
            Role = role;
            _punchKey = keys.PunchKey(sid);
            StartUs = nowUs;
            _lastRecvUs = nowUs;
            _lastSendUs = nowUs;
        }

        public string Sid { get; }
        public byte[] SidBytes { get; }
        public PeerRole Role { get; }
        public long StartUs { get; }
        public LinkState State { get; private set; }
        public string Reason { get; private set; } = "";
        /// <summary>Where data goes: the viewer's choice, or (host) where the viewer's data last came from.</summary>
        public IPEndPoint? Selected { get; private set; }
        public double RttMs { get; private set; } = -1;
        public byte[] PunchKey => _punchKey;

        public bool IsOver => State == LinkState.Closed || State == LinkState.Failed;

        /// <summary>Adds candidates (host: the viewer's from the join; viewer: the host's from the live record / answer).</summary>
        public void AddRemoteCandidates(IEnumerable<string> candidates)
        {
            lock (_lock)
            {
                foreach (string c in candidates)
                {
                    if (!Endpoint.TryParse(c, out HostPort? hp, out _) || !hp!.IsLiteral || !hp.Port.HasValue) continue;
                    IPEndPoint ep = hp.ToIPEndPoint(0);
                    _advertised.Add(ep);
                    if (!_remote.Contains(ep) && _remote.Count < MaxRemoteCandidates) _remote.Add(ep);
                }
            }
        }

        public IList<IPEndPoint> RemoteCandidates
        {
            get { lock (_lock) return _remote.ToArray(); }
        }

        /// <summary>False for an address learned from the other side's packets rather than its candidate list.</summary>
        public bool IsAdvertised(IPEndPoint ep)
        {
            lock (_lock) return _advertised.Contains(ep);
        }

        public bool IsConfirmed(IPEndPoint from)
        {
            lock (_lock) return _confirmed.Contains(from);
        }

        public IPEndPoint[] ConfirmedAddresses
        {
            get
            {
                lock (_lock)
                {
                    var a = new IPEndPoint[_confirmed.Count];
                    _confirmed.CopyTo(a);
                    return a;
                }
            }
        }

        /// <summary>Called for every authentic control packet of this session. Returns replies to send.</summary>
        public List<Outgoing> OnControl(byte[] packet, IPEndPoint from, long nowUs)
        {
            var output = new List<Outgoing>();
            lock (_lock)
            {
                if (IsOver) return output;
                ControlType type = ControlPacket.TypeOf(packet);
                if (type == ControlType.Bye)
                {
                    Close("the other side ended the session");
                    return output;
                }

                _lastRecvUs = nowUs;
                _confirmed.Add(from);
                // A symmetric NAT gives the other side's packets a new port for every destination, so they arrive from
                // an address it never advertised, and our PUNCHes to the advertised one are dropped by that NAT. Punch the
                // address the packets really come from: its NAT made that mapping for us and lets our packets in.
                if ((type == ControlType.Punch || type == ControlType.PunchAck) && !_remote.Contains(from) &&
                    _remote.Count < MaxRemoteCandidates)
                    _remote.Add(from);

                switch (type)
                {
                    case ControlType.Punch:
                        output.Add(new Outgoing(Build(ControlType.PunchAck, nowUs, ControlPacket.TsOf(packet)), from));
                        _lastSendUs = nowUs;
                        if (Role == PeerRole.Host && Selected == null) Selected = from;
                        break;

                    case ControlType.PunchAck:
                        ulong echo = ControlPacket.EchoOf(packet);
                        if (echo > 0 && (long)echo <= nowUs) RttMs = (nowUs - (long)echo) / 1000.0;
                        if (Role == PeerRole.Viewer)
                        {
                            if (from.AddressFamily == AddressFamily.InterNetworkV6)
                            {
                                if (Selected == null || Selected.AddressFamily != AddressFamily.InterNetworkV6) Selected = from;
                            }
                            else if (_ackedV4 == null)
                            {
                                _ackedV4 = from;
                                _ackedV4Us = nowUs;
                            }
                        }
                        else if (Selected == null)
                        {
                            Selected = from;
                        }
                        if (Selected != null && State == LinkState.Punching) State = LinkState.Connected;
                        break;

                    case ControlType.Keepalive:
                        break;
                }
            }
            return output;
        }

        /// <summary>Data (SRT) arrived from <paramref name="from"/>. False = not ours (drop it).</summary>
        public bool OnData(IPEndPoint from, long nowUs)
        {
            lock (_lock)
            {
                if (IsOver || !_confirmed.Contains(from)) return false;
                _lastRecvUs = nowUs;
                // The host follows the viewer: answer wherever its data comes from (the viewer picked that path).
                if (Role == PeerRole.Host && (Selected == null || !Selected.Equals(from))) Selected = from;
                if (State == LinkState.Punching && Role == PeerRole.Host) State = LinkState.Connected;
                return true;
            }
        }

        /// <summary>Data is being sent on <see cref="Selected"/> (resets the keepalive timer).</summary>
        public void NoteSent(long nowUs)
        {
            lock (_lock) _lastSendUs = nowUs;
        }

        /// <summary>Timers: punching, the IPv6 grace window, keepalives, deadlines. Returns datagrams to send.</summary>
        public List<Outgoing> Tick(long nowUs)
        {
            var output = new List<Outgoing>();
            lock (_lock)
            {
                if (IsOver) return output;

                if (Role == PeerRole.Viewer && Selected == null && _ackedV4 != null && nowUs - _ackedV4Us >= Ipv6GraceUs)
                {
                    Selected = _ackedV4;
                    State = LinkState.Connected;
                }

                if (State == LinkState.Punching)
                {
                    if (nowUs - StartUs > PunchDeadlineUs)
                    {
                        State = LinkState.Failed;
                        Reason = _confirmed.Count > 0 ? "packets arrived but the path never worked both ways" : "no candidate answered";
                        return output;
                    }
                    if (_lastPunchUs == long.MinValue || nowUs - _lastPunchUs >= PunchIntervalUs)
                    {
                        _lastPunchUs = nowUs;
                        foreach (IPEndPoint ep in _remote) output.Add(new Outgoing(Build(ControlType.Punch, nowUs, 0), ep));
                        if (_remote.Count > 0) _lastSendUs = nowUs;
                    }
                    return output;
                }

                // Connected
                if (nowUs - _lastRecvUs > SilenceTimeoutUs)
                {
                    Close("the other side went silent");
                    return output;
                }
                if (Selected != null && nowUs - _lastSendUs >= KeepaliveIntervalUs)
                {
                    output.Add(new Outgoing(Build(ControlType.Keepalive, nowUs, 0), Selected));
                    _lastSendUs = nowUs;
                }
            }
            return output;
        }

        /// <summary>Ends the session; returns the BYE datagrams (sent three times, fire and forget).</summary>
        public List<Outgoing> CloseWithBye(string reason, long nowUs)
        {
            var output = new List<Outgoing>();
            lock (_lock)
            {
                if (IsOver) return output;
                var targets = new List<IPEndPoint>();
                if (Selected != null) targets.Add(Selected);
                foreach (IPEndPoint ep in _confirmed) if (!targets.Contains(ep)) targets.Add(ep);
                byte[] bye = Build(ControlType.Bye, nowUs, 0);
                for (int i = 0; i < 3; i++)
                    foreach (IPEndPoint ep in targets) output.Add(new Outgoing(bye, ep));
                Close(reason);
            }
            return output;
        }

        void Close(string reason)
        {
            State = LinkState.Closed;
            Reason = reason;
        }

        byte[] Build(ControlType type, long nowUs, ulong echo) =>
            ControlPacket.Build(type, Role, SidBytes, (ulong)Math.Max(1, nowUs), echo, _punchKey);
    }
}
