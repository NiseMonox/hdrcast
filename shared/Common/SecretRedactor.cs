using System;
using System.Text;

namespace HdrCast.Common
{
    /// <summary>
    /// Removes the SRT passphrase, the MediaMTX credentials and HdrCast invite codes from anything that goes
    /// into a log file. The command line has to carry them, but the log does not: the app invites the user to
    /// open the log folder and send it when something breaks, and mpv echoes the whole URL on a failed connection.
    /// An invite code carries the group key, so it is as secret as a password.
    /// </summary>
    public static class SecretRedactor
    {
        public const string Mask = "***";

        /// <summary>Redacts every secret in a line of text (a command line, a log line, an mpv message).</summary>
        public static string Redact(string? text)
        {
            if (string.IsNullOrEmpty(text)) return text ?? "";
            string result = RedactQueryValue(text!, "passphrase");
            result = RedactStreamIdCredentials(result);
            result = RedactInviteCodes(result);
            return result;
        }

        /// <summary>hdrcast:eyJ2Ijox….AbCdEfGh → hdrcast:*** (the prefix stays so the log still says an invite was there).</summary>
        public static string RedactInviteCodes(string text)
        {
            const string needle = "hdrcast:";
            var sb = new StringBuilder(text.Length);
            int i = 0;
            while (i < text.Length)
            {
                int at = IndexOfIgnoreCase(text, needle, i);
                if (at < 0)
                {
                    sb.Append(text, i, text.Length - i);
                    break;
                }
                int valueStart = at + needle.Length;
                int valueEnd = valueStart;
                while (valueEnd < text.Length && IsInviteChar(text[valueEnd])) valueEnd++;
                sb.Append(text, i, valueStart - i);
                if (valueEnd > valueStart) sb.Append(Mask);
                i = valueEnd;
            }
            return sb.ToString();
        }

        static bool IsInviteChar(char c) =>
            (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '-' || c == '_' || c == '.';

        /// <summary>Replaces the value of <paramref name="key"/>=… up to the next &amp; or whitespace or quote.</summary>
        public static string RedactQueryValue(string text, string key)
        {
            var sb = new StringBuilder(text.Length);
            int i = 0;
            string needle = key + "=";
            while (i < text.Length)
            {
                int at = IndexOfIgnoreCase(text, needle, i);
                if (at < 0)
                {
                    sb.Append(text, i, text.Length - i);
                    break;
                }
                int valueStart = at + needle.Length;
                int valueEnd = valueStart;
                while (valueEnd < text.Length && !IsDelimiter(text[valueEnd])) valueEnd++;
                sb.Append(text, i, valueStart - i);
                if (valueEnd > valueStart) sb.Append(Mask);
                i = valueEnd;
            }
            return sb.ToString();
        }

        /// <summary>
        /// streamid=publish:screen:user:password → streamid=publish:screen:user:***
        /// (the path and the user name are kept: they are useful in a log and are not secrets).
        /// </summary>
        public static string RedactStreamIdCredentials(string text)
        {
            const string needle = "streamid=";
            var sb = new StringBuilder(text.Length);
            int i = 0;
            while (i < text.Length)
            {
                int at = IndexOfIgnoreCase(text, needle, i);
                if (at < 0)
                {
                    sb.Append(text, i, text.Length - i);
                    break;
                }
                int valueStart = at + needle.Length;
                int valueEnd = valueStart;
                while (valueEnd < text.Length && !IsDelimiter(text[valueEnd])) valueEnd++;
                sb.Append(text, i, valueStart - i);

                string value = text.Substring(valueStart, valueEnd - valueStart);
                sb.Append(MaskStreamId(value));
                i = valueEnd;
            }
            return sb.ToString();
        }

        static string MaskStreamId(string value)
        {
            // Custom form: publish:path[:user:pass[:query]] — mask everything from the 4th component on.
            string[] parts = value.Split(':');
            if (parts.Length >= 4)
            {
                var sb = new StringBuilder();
                for (int i = 0; i < parts.Length; i++)
                {
                    if (i > 0) sb.Append(':');
                    sb.Append(i >= 3 ? Mask : parts[i]);
                }
                return sb.ToString();
            }
            // Haivision standard form: #!::m=publish,r=path,u=user,s=secret
            if (value.IndexOf("s=", StringComparison.OrdinalIgnoreCase) >= 0)
                return RedactQueryValueCommaSeparated(value, "s");
            return value;
        }

        static string RedactQueryValueCommaSeparated(string value, string key)
        {
            string[] parts = value.Split(',');
            for (int i = 0; i < parts.Length; i++)
            {
                if (parts[i].StartsWith(key + "=", StringComparison.OrdinalIgnoreCase))
                    parts[i] = key + "=" + Mask;
            }
            return string.Join(",", parts);
        }

        static bool IsDelimiter(char c) => c == '&' || c == '"' || c == '\'' || char.IsWhiteSpace(c);

        static int IndexOfIgnoreCase(string haystack, string needle, int start)
            => haystack.IndexOf(needle, start, StringComparison.OrdinalIgnoreCase);
    }
}
