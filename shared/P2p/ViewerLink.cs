using System;
using System.Collections.Generic;
using System.Net;
using System.Net.Sockets;
using HdrCast.Common;
using HdrCast.Group;

namespace HdrCast.P2p
{
    /// <summary>
    /// The viewer side: mpv connects to srt://127.0.0.1:<see cref="LocalPort"/>; its datagrams go to the host over
    /// the punched path and the host's come back to mpv. The local port and the WAN sockets survive re-joins
    /// (<see cref="StartSession"/> again), so mpv can keep retrying the same URL while the path is rebuilt.
    /// </summary>
    public sealed class ViewerLink : LinkBase
    {
        readonly UdpChannel _local;
        PeerSession? _session;
        IPEndPoint? _mpv;
        LinkState _reported = LinkState.Punching;
        long _bytesFromHost;

        public ViewerLink(GroupKeys keys, LinkAddresses addresses, int localPort = 0) : base(keys, addresses, "p2p-viewer")
        {
            _local = new UdpChannel(new IPEndPoint(IPAddress.Loopback, localPort), "p2p-mpv", OnLocalData);
            StartTicking();
        }

        /// <summary>mpv's URL port on 127.0.0.1.</summary>
        public int LocalPort => _local.LocalEndPoint.Port;

        /// <summary>Raised (on a network thread) when the current session connects, closes or fails.</summary>
        public event Action<ViewerLink>? StateChanged;

        public string? SessionId { get { lock (Sync) return _session?.Sid; } }
        public LinkState State { get { lock (Sync) return _session?.State ?? LinkState.Closed; } }
        public string Reason { get { lock (Sync) return _session?.Reason ?? ""; } }
        public IPEndPoint? Path { get { lock (Sync) return _session?.Selected; } }
        public double RttMs { get { lock (Sync) return _session?.RttMs ?? -1; } }
        public long BytesFromHost => System.Threading.Interlocked.Read(ref _bytesFromHost);

        /// <summary>Begins (or restarts with a new session id) punching towards the host's candidates.</summary>
        public void StartSession(string sid, IEnumerable<string> hostCandidates)
        {
            var next = new PeerSession(Keys, sid, PeerRole.Viewer, MonoClock.NowUs);
            next.AddRemoteCandidates(hostCandidates);
            PeerSession? previous;
            lock (Sync)
            {
                previous = _session;
                _session = next;
                _reported = LinkState.Punching;
            }
            if (previous != null && previous.Sid != sid) SendAll(previous.CloseWithBye("the viewer re-joined", MonoClock.NowUs));
        }

        /// <summary>More host candidates (the answer can carry a fresher list than the live record).</summary>
        public void AddHostCandidates(IEnumerable<string> hostCandidates)
        {
            PeerSession? s;
            lock (Sync) s = _session;
            s?.AddRemoteCandidates(hostCandidates);
        }

        /// <summary>Ends the current session with a BYE (the sockets stay open for a later re-join).</summary>
        public void EndSession(string reason)
        {
            PeerSession? s;
            lock (Sync) s = _session;
            if (s != null) SendAll(s.CloseWithBye(reason, MonoClock.NowUs));
            Report();
        }

        protected override void OnControl(byte[] packet, IPEndPoint from)
        {
            if (ControlPacket.RoleOf(packet) != PeerRole.Host) return;
            PeerSession? s;
            lock (Sync) s = _session;
            if (s == null || ControlPacket.SidOf(packet) != s.Sid || !ControlPacket.VerifyMac(packet, s.PunchKey)) return;
            SendAll(s.OnControl(packet, from, MonoClock.NowUs));
            Report();
        }

        protected override void OnWanData(byte[] buf, int length, IPEndPoint from)
        {
            PeerSession? s;
            IPEndPoint? mpv;
            lock (Sync)
            {
                s = _session;
                mpv = _mpv;
            }
            if (s == null || !s.OnData(from, MonoClock.NowUs) || mpv == null) return;
            if (_local.Send(buf, length, mpv)) System.Threading.Interlocked.Add(ref _bytesFromHost, length);
        }

        void OnLocalData(UdpChannel ch, byte[] buf, int length, IPEndPoint from)
        {
            if (!from.Address.Equals(IPAddress.Loopback)) return;
            PeerSession? s;
            lock (Sync)
            {
                _mpv = from;            // a restarted mpv comes from a new port: follow it
                s = _session;
            }
            IPEndPoint? to = s?.Selected;
            if (s == null || to == null || s.IsOver) return;
            if (SendWan(buf, length, to)) s.NoteSent(MonoClock.NowUs);
        }

        protected override void OnTick(long nowUs)
        {
            PeerSession? s;
            lock (Sync) s = _session;
            if (s == null) return;
            SendAll(s.Tick(nowUs));
            Report();
        }

        protected override void OnDispose()
        {
            PeerSession? s;
            lock (Sync) s = _session;
            if (s != null) SendAll(s.CloseWithBye("the viewer closed", MonoClock.NowUs));
            _local.Dispose();
        }

        void Report()
        {
            bool changed;
            lock (Sync)
            {
                LinkState now = _session?.State ?? LinkState.Closed;
                changed = now != _reported;
                _reported = now;
            }
            if (changed) StateChanged?.Invoke(this);
        }

        /// <summary>"IPv6 [2400:…]:51234, 38 ms" for the status line.</summary>
        public string DescribePath()
        {
            IPEndPoint? p = Path;
            if (p == null) return "";
            string family = p.AddressFamily == AddressFamily.InterNetworkV6 ? "IPv6" : "IPv4";
            double rtt = RttMs;
            return family + " " + Endpoint.Format(p) + (rtt >= 0 ? ", " + Math.Round(rtt) + " ms" : "");
        }
    }
}
