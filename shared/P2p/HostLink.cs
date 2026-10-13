using System;
using System.Collections.Generic;
using System.Net;
using HdrCast.Common;
using HdrCast.Group;

namespace HdrCast.P2p
{
    /// <summary>A snapshot of one viewer session for the UI and the host logic.</summary>
    public sealed class HostSessionInfo
    {
        public string Sid { get; set; } = "";
        public object? Tag { get; set; }
        public LinkState State { get; set; }
        public string Reason { get; set; } = "";
        public string Path { get; set; } = "";
        public bool PathIsIpv6 { get; set; }
        /// <summary>The path is an address the viewer never advertised: its router picked a new port for this PC.</summary>
        public bool PathLearned { get; set; }
        public double RttMs { get; set; }
        public long BytesToViewer { get; set; }
    }

    /// <summary>
    /// The host side: many viewers on the same two WAN sockets. A datagram from a confirmed viewer address goes
    /// to that viewer's own 127.0.0.1 socket, which is connected to the viewer's remux child (SRT listener);
    /// what the child answers goes back to wherever that viewer's data last came from.
    /// </summary>
    public sealed class HostLink : LinkBase
    {
        sealed class Entry
        {
            public Entry(PeerSession session, UdpChannel local, IPEndPoint target, object? tag)
            {
                Session = session; Local = local; Target = target; Tag = tag;
            }
            public PeerSession Session;
            public UdpChannel Local;
            public IPEndPoint Target;
            public object? Tag;
            public LinkState Reported = LinkState.Punching;
            public long BytesToViewer;
        }

        readonly Dictionary<string, Entry> _bySid = new Dictionary<string, Entry>();
        readonly Dictionary<IPEndPoint, Entry> _byAddress = new Dictionary<IPEndPoint, Entry>();

        public HostLink(GroupKeys keys, LinkAddresses addresses) : base(keys, addresses, "p2p-host")
        {
            StartTicking();
        }

        /// <summary>Raised (on a network thread) when a session connects, closes or fails. Keep handlers short.</summary>
        public event Action<HostSessionInfo>? SessionChanged;

        /// <summary>Starts punching towards a viewer; its data will be forwarded to 127.0.0.1:<paramref name="childPort"/>.</summary>
        public void AddSession(string sid, IEnumerable<string> viewerCandidates, int childPort, object? tag = null)
        {
            var session = new PeerSession(Keys, sid, PeerRole.Host, MonoClock.NowUs);
            session.AddRemoteCandidates(viewerCandidates);
            Entry? entry = null;
            UdpChannel local = new UdpChannel(new IPEndPoint(IPAddress.Loopback, 0), "p2p-child-" + sid.Substring(0, 6),
                (ch, buf, n, from) =>
                {
                    Entry? ready = entry;          // the receive thread can start before the assignment below
                    if (ready != null) OnChildData(ready, buf, n, from);
                });
            entry = new Entry(session, local, new IPEndPoint(IPAddress.Loopback, childPort), tag);
            Entry? old;
            lock (Sync)
            {
                _bySid.TryGetValue(sid, out old);
                if (old != null) Forget(old);
                _bySid[sid] = entry;
            }
            old?.Local.Dispose();
        }

        /// <summary>The viewer's remux child moved to another port (it was restarted).</summary>
        public void RetargetSession(string sid, int childPort)
        {
            lock (Sync)
                if (_bySid.TryGetValue(sid, out Entry? e)) e.Target = new IPEndPoint(IPAddress.Loopback, childPort);
        }

        /// <summary>Ends a session (BYE to the viewer) and frees its local socket.</summary>
        public void RemoveSession(string sid, string reason)
        {
            Entry? e;
            lock (Sync)
            {
                if (!_bySid.TryGetValue(sid, out e)) return;
                Forget(e);
            }
            SendAll(e.Session.CloseWithBye(reason, MonoClock.NowUs));
            e.Local.Dispose();
            Report(e, force: true);
        }

        public List<HostSessionInfo> Snapshot()
        {
            lock (Sync)
            {
                var list = new List<HostSessionInfo>();
                foreach (Entry e in _bySid.Values) list.Add(Info(e));
                return list;
            }
        }

        public int SessionCount
        {
            get { lock (Sync) return _bySid.Count; }
        }

        protected override void OnControl(byte[] packet, IPEndPoint from)
        {
            if (ControlPacket.RoleOf(packet) != PeerRole.Viewer) return;   // our own packets reflected back
            string sid = ControlPacket.SidOf(packet);
            Entry? e;
            lock (Sync) _bySid.TryGetValue(sid, out e);
            if (e == null || !ControlPacket.VerifyMac(packet, e.Session.PunchKey)) return;

            List<Outgoing> replies = e.Session.OnControl(packet, from, MonoClock.NowUs);
            if (!e.Session.IsOver)
            {
                lock (Sync)
                    if (_bySid.TryGetValue(sid, out Entry? current) && current == e) _byAddress[from] = e;
            }
            SendAll(replies);
            Report(e, force: false);
        }

        protected override void OnWanData(byte[] buf, int length, IPEndPoint from)
        {
            Entry? e;
            lock (Sync) _byAddress.TryGetValue(from, out e);
            if (e == null) return;                                           // not a confirmed viewer address
            if (!e.Session.OnData(from, MonoClock.NowUs)) return;
            e.Local.Send(buf, length, e.Target);
        }

        void OnChildData(Entry e, byte[] buf, int length, IPEndPoint from)
        {
            if (!from.Address.Equals(IPAddress.Loopback)) return;
            IPEndPoint? to = e.Session.Selected;
            if (to == null || e.Session.IsOver) return;
            if (SendWan(buf, length, to))
            {
                e.Session.NoteSent(MonoClock.NowUs);
                System.Threading.Interlocked.Add(ref e.BytesToViewer, length);
            }
        }

        protected override void OnTick(long nowUs)
        {
            Entry[] entries;
            lock (Sync)
            {
                entries = new Entry[_bySid.Count];
                _bySid.Values.CopyTo(entries, 0);
            }
            foreach (Entry e in entries)
            {
                SendAll(e.Session.Tick(nowUs));
                if (e.Session.IsOver)
                {
                    lock (Sync)
                        if (_bySid.TryGetValue(e.Session.Sid, out Entry? current) && current == e) Forget(e);
                    e.Local.Dispose();
                }
                Report(e, force: false);
            }
        }

        protected override void OnDispose()
        {
            Entry[] entries;
            lock (Sync)
            {
                entries = new Entry[_bySid.Count];
                _bySid.Values.CopyTo(entries, 0);
                _bySid.Clear();
                _byAddress.Clear();
            }
            foreach (Entry e in entries)
            {
                SendAll(e.Session.CloseWithBye("the host stopped", MonoClock.NowUs));
                e.Local.Dispose();
            }
        }

        void Forget(Entry e)
        {
            _bySid.Remove(e.Session.Sid);
            var stale = new List<IPEndPoint>();
            foreach (var kv in _byAddress) if (kv.Value == e) stale.Add(kv.Key);
            foreach (IPEndPoint ep in stale) _byAddress.Remove(ep);
        }

        void Report(Entry e, bool force)
        {
            LinkState now = e.Session.State;
            bool changed;
            lock (Sync)
            {
                changed = force || now != e.Reported;
                e.Reported = now;
            }
            if (changed) SessionChanged?.Invoke(Info(e));
        }

        static HostSessionInfo Info(Entry e)
        {
            IPEndPoint? path = e.Session.Selected;
            return new HostSessionInfo
            {
                Sid = e.Session.Sid,
                Tag = e.Tag,
                State = e.Session.State,
                Reason = e.Session.Reason,
                Path = path == null ? "" : Endpoint.Format(path),
                PathIsIpv6 = path != null && path.AddressFamily == System.Net.Sockets.AddressFamily.InterNetworkV6,
                PathLearned = path != null && !e.Session.IsAdvertised(path),
                RttMs = e.Session.RttMs,
                BytesToViewer = System.Threading.Interlocked.Read(ref e.BytesToViewer),
            };
        }
    }
}
