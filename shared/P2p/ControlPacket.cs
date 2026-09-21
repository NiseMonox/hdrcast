using System;
using System.Text;
using HdrCast.Group;

namespace HdrCast.P2p
{
    public enum ControlType : byte
    {
        Punch = 1,
        PunchAck = 2,
        Keepalive = 3,
        Bye = 4,
    }

    public enum PeerRole : byte
    {
        Host = 1,
        Viewer = 2,
    }

    /// <summary>
    /// The 58-byte UDP control packet (docs/design.md §8.6):
    /// magic "HDRCP2P1" | type | role | sid (16) | ts µs (u64 BE) | echo (u64 BE) | HMAC-SHA256(punchKey, 0..41)[0..16].
    /// Everything else on the same socket is SRT and is forwarded untouched.
    /// </summary>
    public static class ControlPacket
    {
        public const int Length = 58;
        public const int MacOffset = 42;
        public const int MacLength = 16;
        static readonly byte[] Magic = Encoding.ASCII.GetBytes("HDRCP2P1");

        public static byte[] Build(ControlType type, PeerRole role, byte[] sid, ulong ts, ulong echo, byte[] punchKey)
        {
            if (sid == null || sid.Length != GroupKeys.IdBytes) throw new ArgumentException("sid must be 16 bytes", nameof(sid));
            var b = new byte[Length];
            Buffer.BlockCopy(Magic, 0, b, 0, 8);
            b[8] = (byte)type;
            b[9] = (byte)role;
            Buffer.BlockCopy(sid, 0, b, 10, 16);
            WriteU64(b, 26, ts);
            WriteU64(b, 34, echo);
            byte[] mac = GroupKeys.HmacSha256(punchKey, Slice(b, 0, MacOffset));
            Buffer.BlockCopy(mac, 0, b, MacOffset, MacLength);
            return b;
        }

        /// <summary>Right length, magic, known type and role. Says nothing about authenticity.</summary>
        public static bool LooksLikeControl(byte[] buf, int length)
        {
            if (length != Length || buf.Length < Length) return false;
            for (int i = 0; i < Magic.Length; i++)
                if (buf[i] != Magic[i]) return false;
            byte type = buf[8], role = buf[9];
            return type >= 1 && type <= 4 && (role == 1 || role == 2);
        }

        public static ControlType TypeOf(byte[] buf) => (ControlType)buf[8];
        public static PeerRole RoleOf(byte[] buf) => (PeerRole)buf[9];
        public static ulong TsOf(byte[] buf) => ReadU64(buf, 26);
        public static ulong EchoOf(byte[] buf) => ReadU64(buf, 34);

        /// <summary>The session id as the 22-character id used everywhere else.</summary>
        public static string SidOf(byte[] buf) => HdrCast.Common.Base64Url.Encode(Slice(buf, 10, 16));

        public static bool VerifyMac(byte[] buf, byte[] punchKey)
        {
            byte[] mac = GroupKeys.HmacSha256(punchKey, Slice(buf, 0, MacOffset));
            return GroupKeys.FixedTimeEquals(mac, 0, buf, MacOffset, MacLength);
        }

        static byte[] Slice(byte[] b, int offset, int count)
        {
            var s = new byte[count];
            Buffer.BlockCopy(b, offset, s, 0, count);
            return s;
        }

        static void WriteU64(byte[] b, int offset, ulong v)
        {
            for (int i = 7; i >= 0; i--)
            {
                b[offset + i] = (byte)(v & 0xFF);
                v >>= 8;
            }
        }

        static ulong ReadU64(byte[] b, int offset)
        {
            ulong v = 0;
            for (int i = 0; i < 8; i++) v = (v << 8) | b[offset + i];
            return v;
        }
    }
}
