using System;
using System.Collections.Generic;

namespace HdrCast.Receiver
{
    public enum MpvExitReason
    {
        Unknown = 0,
        /// <summary>The server had nothing to play yet ("no stream is available"; kept for old relay-style servers).</summary>
        NoPublisher = 1,
        /// <summary>Connection refused / timed out / reset (the path to the host is not up, or the host stopped).</summary>
        ConnectionFailed = 2,
        /// <summary>Wrong passphrase or credentials.</summary>
        AuthFailed = 3,
        /// <summary>The mpv build cannot open srt:// (no libsrt).</summary>
        ProtocolUnsupported = 4,
        /// <summary>The stream ended (the host stopped).</summary>
        EndOfStream = 5,
        /// <summary>The user closed the mpv window / quit was sent.</summary>
        Quit = 6,
    }

    /// <summary>Guesses why mpv exited from its last log lines so the GUI can say something useful.</summary>
    public static class MpvLogClassifier
    {
        public static MpvExitReason ClassifyLine(string? line)
        {
            if (string.IsNullOrEmpty(line)) return MpvExitReason.Unknown;
            // mpv echoes the whole URL when a connection fails, and our URL always contains the word
            // "passphrase". Classifying on the raw line would report every failure as an auth failure, so the
            // query string is stripped before anything is matched.
            string l = StripUrls(line!);
            if (Has(l, "no stream is available") || Has(l, "no one is publishing")) return MpvExitReason.NoPublisher;
            if (Has(l, "Protocol not found") || Has(l, "not on whitelist") || Has(l, "Unsupported protocol") || Has(l, "Unrecognized URL")) return MpvExitReason.ProtocolUnsupported;
            if (Has(l, "bad password") || Has(l, "badsecret") || Has(l, "wrong password") ||
                Has(l, "invalid passphrase") || Has(l, "passphrase not provided") || Has(l, "wrong passphrase") ||
                Has(l, "authentication failed") || Has(l, "unauthorized") || Has(l, "KM_BADSECRET"))
                return MpvExitReason.AuthFailed;
            if (Has(l, "Exiting... (Quit)")) return MpvExitReason.Quit;
            if (Has(l, "Exiting... (End of file)") || Has(l, "Exiting... (Errors when loading file)")) return MpvExitReason.EndOfStream;
            if (Has(l, "Connection refused") || Has(l, "Connection timed out") || Has(l, "timed out") ||
                Has(l, "Connection setup failure") || Has(l, "connection rejected") || Has(l, "Connection reset") ||
                Has(l, "Failed to open") || Has(l, "Failed to recognize file format") || Has(l, "Input/output error") ||
                Has(l, "No route to host") || Has(l, "Name or service not known") || Has(l, "getaddrinfo"))
                return MpvExitReason.ConnectionFailed;
            return MpvExitReason.Unknown;
        }

        /// <summary>
        /// Removes srt:// / rist:// / http:// URLs from a line. mpv prints the URL it failed to open, and that
        /// URL carries our own option names ("passphrase", "streamid", "latency"), which would otherwise be
        /// matched as if they were error text.
        /// </summary>
        internal static string StripUrls(string line)
        {
            int at = line.IndexOf("://", StringComparison.Ordinal);
            if (at < 0) return line;
            var sb = new System.Text.StringBuilder(line.Length);
            int i = 0;
            while (i < line.Length)
            {
                at = line.IndexOf("://", i, StringComparison.Ordinal);
                if (at < 0) { sb.Append(line, i, line.Length - i); break; }
                // back up over the scheme
                int start = at;
                while (start > i && IsSchemeChar(line[start - 1])) start--;
                sb.Append(line, i, start - i);
                sb.Append("<url>");
                int end = at + 3;
                while (end < line.Length && !char.IsWhiteSpace(line[end])) end++;
                i = end;
            }
            return sb.ToString();
        }

        static bool IsSchemeChar(char c) => char.IsLetterOrDigit(c) || c == '+' || c == '-' || c == '.';

        /// <summary>Most specific reason found in the lines (later lines do not override a more specific earlier one).</summary>
        public static MpvExitReason Classify(IEnumerable<string> recentLines)
        {
            var best = MpvExitReason.Unknown;
            foreach (string line in recentLines)
            {
                var r = ClassifyLine(line);
                if (r == MpvExitReason.Unknown) continue;
                if (best == MpvExitReason.Unknown || Rank(r) > Rank(best)) best = r;
            }
            return best;
        }

        /// <summary>True for lines that mean "nothing to play yet, retry" (nothing published yet, or unreachable).</summary>
        public static bool IsStreamUnavailable(string? line)
        {
            var r = ClassifyLine(line);
            return r == MpvExitReason.NoPublisher || r == MpvExitReason.ConnectionFailed;
        }

        public static string Describe(MpvExitReason reason)
        {
            switch (reason)
            {
                case MpvExitReason.NoPublisher: return "nothing to play yet";
                case MpvExitReason.ConnectionFailed: return "could not connect (the path to the host is not up yet, or the host stopped)";
                case MpvExitReason.AuthFailed: return "rejected: check passphrase / user / password";
                case MpvExitReason.ProtocolUnsupported: return "this mpv build cannot open srt:// (use the ffmpeg pipe frontend or a shinchiro/zhongfly build)";
                case MpvExitReason.EndOfStream: return "stream ended";
                case MpvExitReason.Quit: return "mpv was closed";
                default: return "mpv exited";
            }
        }

        private static int Rank(MpvExitReason r)
        {
            switch (r)
            {
                case MpvExitReason.ProtocolUnsupported: return 6;
                case MpvExitReason.AuthFailed: return 5;
                case MpvExitReason.NoPublisher: return 4;
                case MpvExitReason.ConnectionFailed: return 3;
                case MpvExitReason.EndOfStream: return 2;
                case MpvExitReason.Quit: return 1;
                default: return 0;
            }
        }

        private static bool Has(string line, string needle) =>
            line.IndexOf(needle, StringComparison.OrdinalIgnoreCase) >= 0;
    }
}
