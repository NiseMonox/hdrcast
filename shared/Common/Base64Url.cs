using System;

namespace HdrCast.Common
{
    /// <summary>RFC 4648 §5 base64url without padding (decoding also accepts '=' padding).</summary>
    public static class Base64Url
    {
        public static string Encode(byte[] data)
        {
            if (data == null) throw new ArgumentNullException(nameof(data));
            return Convert.ToBase64String(data).TrimEnd('=').Replace('+', '-').Replace('/', '_');
        }

        public static byte[] Decode(string text)
        {
            if (!TryDecode(text, out byte[] data)) throw new FormatException("not valid base64url");
            return data;
        }

        public static bool TryDecode(string? text, out byte[] data)
        {
            data = new byte[0];
            if (text == null) return false;
            string s = text.TrimEnd('=');
            if (s.Length % 4 == 1) return false;
            foreach (char c in s)
                if (!IsAlphabet(c)) return false;
            string std = s.Replace('-', '+').Replace('_', '/');
            switch (std.Length % 4)
            {
                case 2: std += "=="; break;
                case 3: std += "="; break;
            }
            try
            {
                data = Convert.FromBase64String(std);
                return true;
            }
            catch (FormatException)
            {
                return false;
            }
        }

        public static bool IsAlphabet(char c) =>
            (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '-' || c == '_';
    }
}
