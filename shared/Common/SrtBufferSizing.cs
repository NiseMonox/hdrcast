using System;

namespace HdrCast.Common
{
    /// <summary>Result of <see cref="SrtBufferSizing.Compute"/>.</summary>
    public sealed class SrtBufferPlan
    {
        /// <summary>Packets in flight for latency x bitrate (ceil(latency_s * bitrate_bps / (1316*8))).</summary>
        public int Packets { get; }
        /// <summary>SRTO_FC (ffs=) value: max(25600, next power of two >= packets * 1.2).</summary>
        public int FlowWindow { get; }
        /// <summary>sndbuf= / rcvbuf= value in bytes: FlowWindow * 1472 (SRTO_RCVBUF must be &lt;= FC * (MSS-28)).</summary>
        public long BufferBytes { get; }

        public SrtBufferPlan(int packets, int flowWindow, long bufferBytes)
        {
            Packets = packets;
            FlowWindow = flowWindow;
            BufferBytes = bufferBytes;
        }
    }

    /// <summary>
    /// Sizes libsrt's flow-control window and socket buffers for a given latency x bitrate product.
    /// At >~40 Mbps with >=2 s latency the stream in flight overflows libsrt's default 8192-packet
    /// receive buffer ("No room to store incoming packet" then "SEQUENCE DISCREPANCY"), so both
    /// apps put these values in the srt:// URL (docs/research/srt-mediamtx.research.json).
    /// </summary>
    public static class SrtBufferSizing
    {
        /// <summary>SRT payload per packet (7 x 188-byte TS packets).</summary>
        public const int PayloadBytes = 1316;
        /// <summary>Bytes libsrt reserves per buffered packet (MSS 1500 - 28).</summary>
        public const int PacketSlotBytes = 1472;
        /// <summary>libsrt default SRTO_FC.</summary>
        public const int DefaultFlowWindow = 25600;
        /// <summary>UDP socket receive buffer (recv_buffer_size=) recommended for readers.</summary>
        public const int RecvSocketBufferBytes = 8000000;

        public static SrtBufferPlan Compute(int latencyMs, long bitrateBps)
        {
            if (latencyMs <= 0) throw new ArgumentOutOfRangeException(nameof(latencyMs), "latency must be positive");
            if (bitrateBps <= 0) throw new ArgumentOutOfRangeException(nameof(bitrateBps), "bitrate must be positive");

            double latencySeconds = latencyMs / 1000.0;
            double packetsExact = latencySeconds * bitrateBps / (PayloadBytes * 8.0);
            int packets = (int)Math.Ceiling(packetsExact);
            if (packets < 1) packets = 1;

            long target = (long)Math.Ceiling(packets * 1.2);
            int flowWindow = target <= DefaultFlowWindow ? DefaultFlowWindow : NextPowerOfTwo(target);
            long bufferBytes = (long)flowWindow * PacketSlotBytes;
            return new SrtBufferPlan(packets, flowWindow, bufferBytes);
        }

        /// <summary>Smallest power of two that is >= value (value must be >= 1).</summary>
        public static int NextPowerOfTwo(long value)
        {
            if (value <= 1) return 1;
            long p = 1;
            while (p < value) p <<= 1;
            if (p > int.MaxValue) throw new ArgumentOutOfRangeException(nameof(value));
            return (int)p;
        }
    }
}
