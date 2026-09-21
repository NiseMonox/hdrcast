using System;
using System.Security.Cryptography;
using System.Text;
using HdrCast.Common;

namespace HdrCast.Group
{
    /// <summary>
    /// Everything derived from the 32-byte group key K (docs/design.md §8.3):
    /// sub(label) = HMAC-SHA256(K, "hdrcast/v1/" + label). The signaling server only ever learns
    /// <see cref="ServerKey"/>, so it can authenticate requests but cannot forge what peers sign with
    /// <see cref="MsgKey"/>, and it never sees the SRT passphrase.
    /// </summary>
    public sealed class GroupKeys
    {
        public const int KeyLength = 32;
        public const int IdBytes = 16;
        public const int IdLength = 22;

        public GroupKeys(byte[] groupKey)
        {
            if (groupKey == null || groupKey.Length != KeyLength)
                throw new ArgumentException("the group key must be 32 bytes", nameof(groupKey));
            GroupKey = (byte[])groupKey.Clone();
            ServerKey = Sub("server");
            MsgKey = Sub("msg");
            SrtPassphrase = Base64Url.Encode(Sub("srt")).Substring(0, 32);
        }

        public byte[] GroupKey { get; }
        /// <summary>Authenticates requests to the signaling server (the server stores it).</summary>
        public byte[] ServerKey { get; }
        /// <summary>Signs live records, join requests and answers (the server does not have it).</summary>
        public byte[] MsgKey { get; }
        /// <summary>SRT AES-256 passphrase (32 characters, pbkeylen=32), identical for every member.</summary>
        public string SrtPassphrase { get; }

        /// <summary>Per-session key for the UDP punch packets.</summary>
        public byte[] PunchKey(string sid) => Sub("punch/" + sid);

        public byte[] Sub(string label) => HmacSha256(GroupKey, Encoding.UTF8.GetBytes("hdrcast/v1/" + label));

        // ---- primitives shared by the P2P code ----------------------------------------------------

        public static byte[] HmacSha256(byte[] key, byte[] data)
        {
            using (var h = new HMACSHA256(key)) return h.ComputeHash(data);
        }

        public static byte[] HmacSha256(byte[] key, string text) => HmacSha256(key, Encoding.UTF8.GetBytes(text));

        public static byte[] Sha256(byte[] data)
        {
            using (var h = SHA256.Create()) return h.ComputeHash(data);
        }

        /// <summary>Constant-time comparison (net48 has no CryptographicOperations.FixedTimeEquals).</summary>
        public static bool FixedTimeEquals(byte[]? a, byte[]? b)
        {
            if (a == null || b == null || a.Length != b.Length) return false;
            int diff = 0;
            for (int i = 0; i < a.Length; i++) diff |= a[i] ^ b[i];
            return diff == 0;
        }

        /// <summary>Constant-time comparison of the first <paramref name="count"/> bytes of two buffers.</summary>
        public static bool FixedTimeEquals(byte[] a, int aOffset, byte[] b, int bOffset, int count)
        {
            if (aOffset < 0 || bOffset < 0 || aOffset + count > a.Length || bOffset + count > b.Length) return false;
            int diff = 0;
            for (int i = 0; i < count; i++) diff |= a[aOffset + i] ^ b[bOffset + i];
            return diff == 0;
        }

        public static byte[] RandomBytes(int count)
        {
            var buf = new byte[count];
            using (var rng = RandomNumberGenerator.Create()) rng.GetBytes(buf);
            return buf;
        }

        /// <summary>A new 22-character id (16 random bytes, base64url): group, peer and session ids.</summary>
        public static string NewId() => Base64Url.Encode(RandomBytes(IdBytes));

        public static bool IsValidId(string? id)
        {
            if (id == null || id.Length != IdLength) return false;
            foreach (char c in id)
                if (!Base64Url.IsAlphabet(c)) return false;
            return Base64Url.TryDecode(id, out byte[] raw) && raw.Length == IdBytes;
        }

        public static byte[] IdBytesOf(string id)
        {
            if (!IsValidId(id)) throw new ArgumentException("not a 22-character id", nameof(id));
            return Base64Url.Decode(id);
        }
    }
}
