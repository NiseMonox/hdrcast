using System;
using System.Collections.Generic;
using System.Net;
using System.Net.Sockets;
using System.Threading;
using HdrCast.Common;
using HdrCast.Group;

namespace HdrCast.P2p
{
    /// <summary>Where a peer's WAN sockets bind and what it advertises.</summary>
    public sealed class LinkAddresses
    {
        /// <summary>This PC's stable global IPv6 address (bound directly so replies and PUNCHes leave from it); null = no IPv6.</summary>
        public IPAddress? Ipv6 { get; set; }
        /// <summary>IPv4 bind address (normally 0.0.0.0); null = no IPv4 socket.</summary>
        public IPAddress? Ipv4Bind { get; set; } = IPAddress.Any;
        /// <summary>IPv4 address to advertise when bound to 0.0.0.0 (the LAN address); null = do not advertise.</summary>
        public IPAddress? Ipv4Advertise { get; set; }
        /// <summary>Test hooks.</summary>
        public IPacketFilter? FilterV6 { get; set; }
        public IPacketFilter? FilterV4 { get; set; }

        /// <summary>What this PC offers by default: its stable IPv6 and its LAN IPv4.</summary>
        public static LinkAddresses Detect() => new LinkAddresses
        {
            Ipv6 = LocalAddresses.BestIpv6(),
            Ipv4Bind = IPAddress.Any,
            Ipv4Advertise = LocalAddresses.LanIpv4(),
        };
    }

    /// <summary>The two WAN sockets, the candidate list and the 20 ms timer shared by both roles.</summary>
    public abstract class LinkBase : IDisposable
    {
        protected readonly GroupKeys Keys;
        protected readonly object Sync = new object();
        readonly UdpChannel? _v6;
        readonly UdpChannel? _v4;
        readonly List<string> _candidates = new List<string>();
        readonly Thread _ticker;
        volatile bool _disposed;

        protected LinkBase(GroupKeys keys, LinkAddresses addresses, string name)
        {
            Keys = keys ?? throw new ArgumentNullException(nameof(keys));
            if (addresses == null) throw new ArgumentNullException(nameof(addresses));
            try
            {
                if (addresses.Ipv6 != null)
                {
                    _v6 = new UdpChannel(new IPEndPoint(addresses.Ipv6, 0), name + "-v6", OnWanReceive) { Filter = addresses.FilterV6 };
                    _candidates.Add(Endpoint.Format(_v6.LocalEndPoint));
                }
                if (addresses.Ipv4Bind != null)
                {
                    _v4 = new UdpChannel(new IPEndPoint(addresses.Ipv4Bind, 0), name + "-v4", OnWanReceive) { Filter = addresses.FilterV4 };
                    IPAddress? advertise = addresses.Ipv4Bind.Equals(IPAddress.Any) ? addresses.Ipv4Advertise : addresses.Ipv4Bind;
                    if (advertise != null) _candidates.Add(Endpoint.Format(new IPEndPoint(advertise, _v4.LocalEndPoint.Port)));
                }
            }
            catch
            {
                _v6?.Dispose();
                _v4?.Dispose();
                throw;
            }
            _ticker = new Thread(TickLoop) { IsBackground = true, Name = name + "-tick" };
        }

        /// <summary>Starts the timer; call at the end of the derived constructor.</summary>
        protected void StartTicking() => _ticker.Start();

        public bool HasIpv6 => _v6 != null;
        public bool HasIpv4 => _v4 != null;
        public int? Ipv4Port => _v4?.LocalEndPoint.Port;
        public IPEndPoint? Ipv6EndPoint => _v6?.LocalEndPoint;

        /// <summary>"[v6]:port" and "lan4:port" plus anything added with <see cref="AddCandidate"/>.</summary>
        public IList<string> Candidates
        {
            get { lock (_candidates) return _candidates.ToArray(); }
        }

        /// <summary>Adds e.g. the public IPv4 mapping learned from the UDP echo.</summary>
        public void AddCandidate(string candidate)
        {
            lock (_candidates)
                if (!_candidates.Contains(candidate)) _candidates.Add(candidate);
        }

        public long WanBytesIn => (_v6?.BytesIn ?? 0) + (_v4?.BytesIn ?? 0);
        public long WanBytesOut => (_v6?.BytesOut ?? 0) + (_v4?.BytesOut ?? 0);

        /// <summary>The IPv4 socket, for the UDP echo that discovers the public mapping.</summary>
        protected UdpChannel? V4Channel => _v4;

        protected bool SendWan(byte[] data, int length, IPEndPoint to)
        {
            UdpChannel? ch = to.AddressFamily == AddressFamily.InterNetworkV6 ? _v6 : _v4;
            return ch != null && ch.Send(data, length, to);
        }

        protected void SendAll(List<Outgoing> packets)
        {
            foreach (Outgoing o in packets) SendWan(o.Data, o.Data.Length, o.To);
        }

        protected bool IsDisposed => _disposed;

        void OnWanReceive(UdpChannel ch, byte[] buf, int n, IPEndPoint from)
        {
            if (_disposed) return;
            if (ControlPacket.LooksLikeControl(buf, n)) OnControl(buf, from);
            else if (EchoProtocol.LooksLikeReply(buf, n)) OnEchoReply(buf, n, from);
            else OnWanData(buf, n, from);
        }

        // ---- UDP echo (docs/design.md §8.7): what does the internet see as this socket's IPv4 address? ----

        readonly object _echoLock = new object();
        byte[]? _echoNonce;
        IPEndPoint? _echoServer;
        IPEndPoint? _echoResult;
        ManualResetEventSlim? _echoDone;

        /// <summary>
        /// Asks the signaling server's UDP echo what address and port it saw for the IPv4 socket (the NAT mapping
        /// peers must punch towards). Null without an IPv4 socket, or when no answer comes in time.
        /// </summary>
        public IPEndPoint? DiscoverPublicIpv4(IPEndPoint echoServer, int timeoutMs)
        {
            if (_v4 == null || echoServer.AddressFamily != AddressFamily.InterNetwork) return null;
            byte[] nonce = GroupKeys.RandomBytes(EchoProtocol.NonceLength);
            var done = new ManualResetEventSlim(false);
            lock (_echoLock)
            {
                _echoNonce = nonce;
                _echoServer = echoServer;
                _echoResult = null;
                _echoDone = done;
            }
            byte[] request = EchoProtocol.BuildRequest(nonce);
            for (int i = 0; i < 3 && !done.IsSet; i++)
            {
                _v4.Send(request, echoServer);
                done.Wait(Math.Max(100, timeoutMs / 3));
            }
            lock (_echoLock)
            {
                IPEndPoint? result = _echoResult;
                _echoNonce = null;
                _echoDone = null;
                return result;
            }
        }

        void OnEchoReply(byte[] buf, int n, IPEndPoint from)
        {
            lock (_echoLock)
            {
                if (_echoNonce == null || _echoServer == null || !from.Equals(_echoServer)) return;
                IPEndPoint? mapped = EchoProtocol.ParseReply(buf, n, _echoNonce);
                if (mapped == null) return;
                _echoResult = mapped;
                _echoDone?.Set();
            }
        }

        /// <summary>A datagram that looks like a control packet (authenticity not yet checked).</summary>
        protected abstract void OnControl(byte[] packet, IPEndPoint from);
        /// <summary>Any other datagram (SRT, or garbage).</summary>
        protected abstract void OnWanData(byte[] buf, int length, IPEndPoint from);
        protected abstract void OnTick(long nowUs);
        protected virtual void OnDispose() { }

        void TickLoop()
        {
            while (!_disposed)
            {
                try
                {
                    OnTick(MonoClock.NowUs);
                }
                catch (Exception)
                {
                    // keep ticking; a failed send is retried on the next tick
                }
                Thread.Sleep(20);
            }
        }

        public void Dispose()
        {
            if (_disposed) return;
            try { OnDispose(); } catch (Exception) { }
            _disposed = true;
            _v6?.Dispose();
            _v4?.Dispose();
            if (Thread.CurrentThread != _ticker && _ticker.IsAlive) _ticker.Join(1000);
        }
    }
}
