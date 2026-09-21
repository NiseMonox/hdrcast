using System;
using System.Net;
using System.Text;
using HdrCast.Group;

namespace HdrCast.P2p
{
    /// <summary>
    /// The signaling server's UDP echo (docs/design.md §8.7): request "HDRCECHO"+nonce(16); reply
    /// "HDRCECHO"+nonce+family(4|6)+port(u16 BE)+address(4|16). Unauthenticated and no larger than the request
    /// plus 19 bytes, so it cannot be used for amplification.
    /// </summary>
    public static class EchoProtocol
    {
        public const int NonceLength = 16;
        public const int DefaultPort = 18651;
        static readonly byte[] Magic = Encoding.ASCII.GetBytes("HDRCECHO");

        public static byte[] BuildRequest(byte[] nonce)
        {
            if (nonce == null || nonce.Length != NonceLength) throw new ArgumentException("nonce must be 16 bytes", nameof(nonce));
            var b = new byte[Magic.Length + NonceLength];
            Buffer.BlockCopy(Magic, 0, b, 0, Magic.Length);
            Buffer.BlockCopy(nonce, 0, b, Magic.Length, NonceLength);
            return b;
        }

        /// <summary>Magic and a plausible length (4- or 16-byte address).</summary>
        public static bool LooksLikeReply(byte[] buf, int n)
        {
            if (n != 8 + 16 + 1 + 2 + 4 && n != 8 + 16 + 1 + 2 + 16) return false;
            for (int i = 0; i < Magic.Length; i++)
                if (buf[i] != Magic[i]) return false;
            return true;
        }

        /// <summary>The address the server saw, or null when the nonce or format does not match.</summary>
        public static IPEndPoint? ParseReply(byte[] buf, int n, byte[] nonce)
        {
            if (!LooksLikeReply(buf, n)) return null;
            if (!GroupKeys.FixedTimeEquals(buf, 8, nonce, 0, NonceLength)) return null;
            byte family = buf[24];
            int port = (buf[25] << 8) | buf[26];
            int addrLen = family == 4 ? 4 : family == 6 ? 16 : 0;
            if (addrLen == 0 || n != 27 + addrLen || port == 0) return null;
            var addr = new byte[addrLen];
            Buffer.BlockCopy(buf, 27, addr, 0, addrLen);
            return new IPEndPoint(new IPAddress(addr), port);
        }

        /// <summary>For tests and the server stand-in: the reply for <paramref name="seen"/>.</summary>
        public static byte[] BuildReply(byte[] nonce, IPEndPoint seen)
        {
            IPAddress a = HdrCast.Common.Endpoint.Unmap(seen.Address);
            byte[] addr = a.GetAddressBytes();
            var b = new byte[27 + addr.Length];
            Buffer.BlockCopy(Magic, 0, b, 0, 8);
            Buffer.BlockCopy(nonce, 0, b, 8, NonceLength);
            b[24] = (byte)(addr.Length == 4 ? 4 : 6);
            b[25] = (byte)(seen.Port >> 8);
            b[26] = (byte)(seen.Port & 0xFF);
            Buffer.BlockCopy(addr, 0, b, 27, addr.Length);
            return b;
        }
    }
}
