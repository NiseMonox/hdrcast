using System;
using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using System.Threading;

namespace HdrCast.P2p
{
    /// <summary>Microseconds from a monotonic clock (Stopwatch), overflow-safe for long uptimes.</summary>
    public static class MonoClock
    {
        public static long NowUs
        {
            get
            {
                long t = Stopwatch.GetTimestamp();
                long f = Stopwatch.Frequency;
                return (t / f) * 1_000_000L + (t % f) * 1_000_000L / f;
            }
        }
    }

    /// <summary>
    /// Test hook standing in for the stateful firewalls on the real path: it sees every datagram a
    /// <see cref="UdpChannel"/> sends and decides which arriving datagrams get through.
    /// </summary>
    public interface IPacketFilter
    {
        void OnOutbound(IPEndPoint to);
        bool AllowInbound(IPEndPoint from);
    }

    /// <summary>
    /// A bound UDP socket with a dedicated receive thread (not the thread pool: net48 grows the pool slowly and a
    /// punch deadline is 15 s). The handler runs on that thread with a shared buffer and must not keep it.
    /// </summary>
    public sealed class UdpChannel : IDisposable
    {
        public delegate void ReceiveHandler(UdpChannel channel, byte[] buffer, int length, IPEndPoint from);

        public const int DefaultBufferBytes = 4 << 20;

        readonly Socket _socket;
        readonly ReceiveHandler _onReceive;
        readonly Thread _thread;
        volatile bool _closed;
        long _datagramsIn, _datagramsOut, _bytesIn, _bytesOut;

        public UdpChannel(IPEndPoint bind, string name, ReceiveHandler onReceive, int bufferBytes = DefaultBufferBytes)
        {
            _onReceive = onReceive ?? throw new ArgumentNullException(nameof(onReceive));
            Name = name;
            _socket = new Socket(bind.AddressFamily, SocketType.Dgram, ProtocolType.Udp);
            try
            {
                if (bind.AddressFamily == AddressFamily.InterNetworkV6) _socket.DualMode = false;
                try
                {
                    _socket.ReceiveBufferSize = bufferBytes;
                    _socket.SendBufferSize = bufferBytes;
                }
                catch (SocketException) { }
                DisableConnectionReset(_socket);
                _socket.Bind(bind);
            }
            catch
            {
                _socket.Close();
                throw;
            }
            LocalEndPoint = (IPEndPoint)_socket.LocalEndPoint!;
            _thread = new Thread(Loop) { IsBackground = true, Name = "udp-" + name };
            _thread.Start();
        }

        public string Name { get; }
        public IPEndPoint LocalEndPoint { get; }
        public AddressFamily Family => LocalEndPoint.AddressFamily;
        public IPacketFilter? Filter { get; set; }

        public long DatagramsIn => Interlocked.Read(ref _datagramsIn);
        public long DatagramsOut => Interlocked.Read(ref _datagramsOut);
        public long BytesIn => Interlocked.Read(ref _bytesIn);
        public long BytesOut => Interlocked.Read(ref _bytesOut);

        public bool Send(byte[] data, int length, IPEndPoint to)
        {
            if (_closed || to == null || to.AddressFamily != Family) return false;
            try
            {
                Filter?.OnOutbound(to);
                _socket.SendTo(data, 0, length, SocketFlags.None, to);
                Interlocked.Increment(ref _datagramsOut);
                Interlocked.Add(ref _bytesOut, length);
                return true;
            }
            catch (SocketException) { return false; }
            catch (ObjectDisposedException) { return false; }
        }

        public bool Send(byte[] data, IPEndPoint to) => Send(data, data.Length, to);

        void Loop()
        {
            var buf = new byte[65536];
            EndPoint any = new IPEndPoint(Family == AddressFamily.InterNetworkV6 ? IPAddress.IPv6Any : IPAddress.Any, 0);
            int errors = 0;
            while (!_closed)
            {
                EndPoint from = any;
                int n;
                try
                {
                    n = _socket.ReceiveFrom(buf, ref from);
                    errors = 0;
                }
                catch (SocketException ex)
                {
                    if (_closed) return;
                    // An ICMP error for an earlier send (unreachable candidate) or an oversize datagram: keep going.
                    if (ex.SocketErrorCode == SocketError.ConnectionReset || ex.SocketErrorCode == SocketError.MessageSize) continue;
                    if (++errors > 50) Thread.Sleep(20);
                    continue;
                }
                catch (ObjectDisposedException)
                {
                    return;
                }
                var ep = (IPEndPoint)from;
                if (Filter != null && !Filter.AllowInbound(ep)) continue;
                Interlocked.Increment(ref _datagramsIn);
                Interlocked.Add(ref _bytesIn, n);
                try
                {
                    _onReceive(this, buf, n, ep);
                }
                catch (Exception)
                {
                    // a handler bug must not stop the socket
                }
            }
        }

        public void Dispose()
        {
            if (_closed) return;
            _closed = true;
            try { _socket.Close(); } catch (Exception) { }
            if (Thread.CurrentThread != _thread) _thread.Join(1000);
        }

        /// <summary>Windows turns an ICMP "port unreachable" for an earlier SendTo into WSAECONNRESET on the next receive.</summary>
        static void DisableConnectionReset(Socket s)
        {
            const int SIO_UDP_CONNRESET = -1744830452;
            try
            {
                s.IOControl(SIO_UDP_CONNRESET, new byte[] { 0, 0, 0, 0 }, null);
            }
            catch (Exception)
            {
                // Linux and macOS: not a thing there
            }
        }
    }
}
