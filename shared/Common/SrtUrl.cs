using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;

namespace HdrCast.Common
{
    /// <summary>How the SRT socket reaches its peer.</summary>
    public enum SrtTopology
    {
        /// <summary>Caller to a MediaMTX relay; credentials travel in the streamid.</summary>
        Relay = 0,
        /// <summary>mode=listener on 0.0.0.0 (this side needs the UDP port forwarded).</summary>
        DirectListener = 1,
        /// <summary>mode=caller straight to the peer's public address.</summary>
        DirectCaller = 2,
    }

    /// <summary>Whether this side sends (sender app) or receives (receiver app) the stream.</summary>
    public enum SrtRole
    {
        Publish = 0,
        Read = 1,
    }

    /// <summary>All inputs of <see cref="SrtUrl.Build"/>.</summary>
    public sealed class SrtUrlOptions
    {
        public SrtTopology Topology { get; set; } = SrtTopology.Relay;
        public SrtRole Role { get; set; } = SrtRole.Read;

        /// <summary>Relay host or remote peer; ignored (0.0.0.0) for listeners.</summary>
        public string Host { get; set; } = "";
        public int Port { get; set; } = 8890;

        /// <summary>MediaMTX path (relay only).</summary>
        public string StreamPath { get; set; } = "screen";
        /// <summary>MediaMTX user for the streamid (relay only, optional).</summary>
        public string? User { get; set; }
        /// <summary>MediaMTX password for the streamid (relay only, optional).</summary>
        public string? Password { get; set; }

        /// <summary>AES passphrase, 10-79 characters; null/empty = unencrypted.</summary>
        public string? Passphrase { get; set; }
        /// <summary>pbkeylen: 16 / 24 / 32 (AES-128/192/256). Only emitted with a passphrase.</summary>
        public int PbKeyLen { get; set; } = 32;

        /// <summary>SRT latency in milliseconds (the URL carries microseconds).</summary>
        public int LatencyMs { get; set; } = 1500;
        /// <summary>Expected stream bitrate in bits per second; drives buffer sizing and inputbw.</summary>
        public long BitrateBps { get; set; } = 100_000_000;

        public int PktSize { get; set; } = SrtBufferSizing.PayloadBytes;
        public bool LiveTransType { get; set; } = true;

        /// <summary>Emit maxbw=0&amp;inputbw=&lt;bitrate/8&gt;&amp;oheadbw=&lt;pct&gt; (sender-side pacing).</summary>
        public bool RateControl { get; set; }
        /// <summary>oheadbw percentage (5..100).</summary>
        public int OverheadPercent { get; set; } = 25;

        /// <summary>connect_timeout in milliseconds (callers only; 0 = omit).</summary>
        public int ConnectTimeoutMs { get; set; } = 5000;
        /// <summary>Emit recv_buffer_size=8000000 for readers.</summary>
        public bool IncludeRecvSocketBuffer { get; set; } = true;
        public int RecvSocketBufferBytes { get; set; } = SrtBufferSizing.RecvSocketBufferBytes;
    }

    /// <summary>Decoded pieces of an srt:// URL (see <see cref="SrtUrl.Parse"/>).</summary>
    public sealed class SrtUrlParts
    {
        public string Host { get; set; } = "";
        public int Port { get; set; }
        /// <summary>Query parameters in URL order, values percent-decoded like ffmpeg does.</summary>
        public List<KeyValuePair<string, string>> Query { get; } = new List<KeyValuePair<string, string>>();

        public string? Get(string key)
        {
            foreach (var kv in Query)
            {
                if (kv.Key == key) return kv.Value;
            }
            return null;
        }

        public bool Has(string key) => Get(key) != null;
    }

    /// <summary>
    /// Builds srt:// URLs for ffmpeg / mpv (libavformat's libsrt.c reads every option from the ?query;
    /// all latency-style values are MICROSECONDS, maxbw/inputbw are bytes per second, ffs is packets).
    /// </summary>
    public static class SrtUrl
    {
        public const string ListenerHost = "0.0.0.0";
        public const int MinPassphraseLength = 10;
        public const int MaxPassphraseLength = 79;

        public static string Build(SrtUrlOptions o)
        {
            if (o == null) throw new ArgumentNullException(nameof(o));
            if (o.Port < 1 || o.Port > 65535) throw new ArgumentException("port must be 1..65535", nameof(o));
            if (o.LatencyMs <= 0) throw new ArgumentException("latency must be positive", nameof(o));
            if (o.BitrateBps <= 0) throw new ArgumentException("bitrate must be positive", nameof(o));
            if (o.PbKeyLen != 0 && o.PbKeyLen != 16 && o.PbKeyLen != 24 && o.PbKeyLen != 32)
                throw new ArgumentException("pbkeylen must be 0, 16, 24 or 32", nameof(o));
            if (o.OverheadPercent < 5 || o.OverheadPercent > 100)
                throw new ArgumentException("oheadbw must be 5..100", nameof(o));

            bool listener = o.Topology == SrtTopology.DirectListener;
            string host = listener ? ListenerHost : (o.Host ?? "").Trim();
            if (host.Length == 0) throw new ArgumentException("host is required", nameof(o));
            if (host.IndexOf(':') >= 0 && !host.StartsWith("[", StringComparison.Ordinal)) host = "[" + host + "]";

            var q = new List<KeyValuePair<string, string>>();
            void Add(string k, string v) => q.Add(new KeyValuePair<string, string>(k, v));

            switch (o.Topology)
            {
                case SrtTopology.DirectListener: Add("mode", "listener"); break;
                case SrtTopology.DirectCaller: Add("mode", "caller"); break;
                case SrtTopology.Relay:
                    Add("streamid", BuildStreamId(o.Role, o.StreamPath, o.User, o.Password));
                    break;
                default: throw new ArgumentOutOfRangeException(nameof(o), "unknown topology");
            }

            if (!string.IsNullOrEmpty(o.Passphrase))
            {
                ValidatePassphrase(o.Passphrase);
                Add("passphrase", Encode(o.Passphrase!));
                if (o.PbKeyLen != 0) Add("pbkeylen", o.PbKeyLen.ToString(CultureInfo.InvariantCulture));
            }

            if (o.PktSize > 0) Add("pkt_size", o.PktSize.ToString(CultureInfo.InvariantCulture));
            if (o.LiveTransType) Add("transtype", "live");
            Add("latency", ((long)o.LatencyMs * 1000L).ToString(CultureInfo.InvariantCulture));

            if (o.RateControl)
            {
                Add("maxbw", "0");
                Add("inputbw", (o.BitrateBps / 8L).ToString(CultureInfo.InvariantCulture));
                Add("oheadbw", o.OverheadPercent.ToString(CultureInfo.InvariantCulture));
            }

            var plan = SrtBufferSizing.Compute(o.LatencyMs, o.BitrateBps);
            Add(o.Role == SrtRole.Publish ? "sndbuf" : "rcvbuf", plan.BufferBytes.ToString(CultureInfo.InvariantCulture));
            Add("ffs", plan.FlowWindow.ToString(CultureInfo.InvariantCulture));
            if (o.Role == SrtRole.Read && o.IncludeRecvSocketBuffer && o.RecvSocketBufferBytes > 0)
                Add("recv_buffer_size", o.RecvSocketBufferBytes.ToString(CultureInfo.InvariantCulture));

            if (listener)
                Add("listen_timeout", "-1");
            else if (o.ConnectTimeoutMs > 0)
                Add("connect_timeout", o.ConnectTimeoutMs.ToString(CultureInfo.InvariantCulture));

            var sb = new StringBuilder();
            sb.Append("srt://").Append(host).Append(':').Append(o.Port.ToString(CultureInfo.InvariantCulture)).Append('?');
            for (int i = 0; i < q.Count; i++)
            {
                if (i > 0) sb.Append('&');
                sb.Append(q[i].Key).Append('=').Append(q[i].Value);
            }
            return sb.ToString();
        }

        /// <summary>
        /// MediaMTX custom stream id: publish|read:&lt;path&gt;[:&lt;user&gt;:&lt;pass&gt;]. Components are
        /// percent-encoded individually (ffmpeg decodes the whole streamid before sending it) and must not
        /// contain ':' because MediaMTX splits on it after decoding.
        /// </summary>
        public static string BuildStreamId(SrtRole role, string path, string? user, string? password)
        {
            path = (path ?? "").Trim();
            if (path.Length == 0) throw new ArgumentException("stream path is required", nameof(path));
            CheckNoColon(path, "path");
            var sb = new StringBuilder();
            sb.Append(role == SrtRole.Publish ? "publish:" : "read:").Append(Encode(path));
            if (!string.IsNullOrEmpty(user))
            {
                CheckNoColon(user!, "user");
                CheckNoColon(password ?? "", "password");
                sb.Append(':').Append(Encode(user!)).Append(':').Append(Encode(password ?? ""));
            }
            else if (!string.IsNullOrEmpty(password))
            {
                throw new ArgumentException("a password needs a user name", nameof(user));
            }
            return sb.ToString();
        }

        public static void ValidatePassphrase(string? passphrase)
        {
            if (string.IsNullOrEmpty(passphrase)) return;
            int len = passphrase!.Length;
            if (len < MinPassphraseLength || len > MaxPassphraseLength)
                throw new ArgumentException("passphrase must be 10-79 characters", nameof(passphrase));
        }

        /// <summary>True when the passphrase is empty (unencrypted) or 10-79 characters.</summary>
        public static bool IsValidPassphrase(string? passphrase)
        {
            if (string.IsNullOrEmpty(passphrase)) return true;
            int len = passphrase!.Length;
            return len >= MinPassphraseLength && len <= MaxPassphraseLength;
        }

        /// <summary>
        /// RFC 3986 percent-encoding of a query value: unreserved characters (ALPHA / DIGIT / "-" / "." / "_" / "~")
        /// pass through, everything else (including '&amp;', '#', '%', '+', ':', '=', spaces and non-ASCII as UTF-8)
        /// becomes %XX. '+' must be encoded because av_find_info_tag turns a raw '+' into a space.
        /// </summary>
        public static string Encode(string value)
        {
            if (string.IsNullOrEmpty(value)) return "";
            var sb = new StringBuilder(value.Length + 8);
            foreach (byte b in Encoding.UTF8.GetBytes(value))
            {
                char c = (char)b;
                if ((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') ||
                    c == '-' || c == '.' || c == '_' || c == '~')
                {
                    sb.Append(c);
                }
                else
                {
                    sb.Append('%').Append("0123456789ABCDEF"[b >> 4]).Append("0123456789ABCDEF"[b & 0xF]);
                }
            }
            return sb.ToString();
        }

        /// <summary>Decodes like ffmpeg: av_find_info_tag maps '+' to ' ', ff_urldecode expands %XX (UTF-8).</summary>
        public static string Decode(string value)
        {
            if (string.IsNullOrEmpty(value)) return "";
            var bytes = new List<byte>(value.Length);
            for (int i = 0; i < value.Length; i++)
            {
                char c = value[i];
                if (c == '+')
                {
                    bytes.Add((byte)' ');
                }
                else if (c == '%' && i + 2 < value.Length && IsHex(value[i + 1]) && IsHex(value[i + 2]))
                {
                    bytes.Add((byte)((HexVal(value[i + 1]) << 4) | HexVal(value[i + 2])));
                    i += 2;
                }
                else
                {
                    foreach (byte b in Encoding.UTF8.GetBytes(c.ToString())) bytes.Add(b);
                }
            }
            return Encoding.UTF8.GetString(bytes.ToArray());
        }

        /// <summary>Splits srt://host:port?k=v&amp;k=v into parts, decoding values the way ffmpeg does.</summary>
        public static SrtUrlParts Parse(string url)
        {
            if (url == null) throw new ArgumentNullException(nameof(url));
            const string scheme = "srt://";
            if (!url.StartsWith(scheme, StringComparison.OrdinalIgnoreCase))
                throw new FormatException("not an srt:// URL");
            string rest = url.Substring(scheme.Length);
            string authority;
            string query;
            int qm = rest.IndexOf('?');
            if (qm >= 0)
            {
                authority = rest.Substring(0, qm);
                query = rest.Substring(qm + 1);
            }
            else
            {
                authority = rest;
                query = "";
            }
            int slash = authority.IndexOf('/');
            if (slash >= 0) authority = authority.Substring(0, slash);

            var parts = new SrtUrlParts();
            string hostPart;
            string portPart;
            if (authority.StartsWith("[", StringComparison.Ordinal))
            {
                int close = authority.IndexOf(']');
                if (close < 0) throw new FormatException("unterminated IPv6 literal");
                hostPart = authority.Substring(1, close - 1);
                portPart = close + 1 < authority.Length && authority[close + 1] == ':' ? authority.Substring(close + 2) : "";
            }
            else
            {
                int colon = authority.LastIndexOf(':');
                hostPart = colon >= 0 ? authority.Substring(0, colon) : authority;
                portPart = colon >= 0 ? authority.Substring(colon + 1) : "";
            }
            parts.Host = hostPart;
            if (portPart.Length > 0)
            {
                if (!int.TryParse(portPart, NumberStyles.None, CultureInfo.InvariantCulture, out int port))
                    throw new FormatException("invalid port");
                parts.Port = port;
            }

            if (query.Length > 0)
            {
                foreach (string pair in query.Split('&'))
                {
                    if (pair.Length == 0) continue;
                    int eq = pair.IndexOf('=');
                    string k = eq >= 0 ? pair.Substring(0, eq) : pair;
                    string v = eq >= 0 ? pair.Substring(eq + 1) : "";
                    parts.Query.Add(new KeyValuePair<string, string>(k, Decode(v)));
                }
            }
            return parts;
        }

        private static void CheckNoColon(string value, string what)
        {
            if (value.IndexOf(':') >= 0)
                throw new ArgumentException("the " + what + " must not contain ':' (MediaMTX splits the stream id on it)", what);
        }

        private static bool IsHex(char c) =>
            (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F');

        private static int HexVal(char c)
        {
            if (c >= '0' && c <= '9') return c - '0';
            if (c >= 'a' && c <= 'f') return c - 'a' + 10;
            return c - 'A' + 10;
        }
    }
}
