using System;
using System.Globalization;
using System.Net;
using System.Net.Sockets;
using System.Text;

namespace HdrCast.Common
{
    public enum EndpointKind
    {
        Hostname = 0,
        IPv4 = 1,
        IPv6 = 2,
    }

    /// <summary>A host with an optional port, as typed by a person or carried in an invite / candidate list.</summary>
    public sealed class HostPort
    {
        public HostPort(string host, int? port, EndpointKind kind)
        {
            Host = host;
            Port = port;
            Kind = kind;
        }

        /// <summary>Bare host: a name, a dotted IPv4 address or a compressed IPv6 address (never bracketed).</summary>
        public string Host { get; }
        public int? Port { get; }
        public EndpointKind Kind { get; }

        public bool IsLiteral => Kind != EndpointKind.Hostname;

        /// <summary>"host:port" with IPv6 in brackets, using <paramref name="defaultPort"/> when none was given.</summary>
        public string ToAuthority(int defaultPort) =>
            Endpoint.FormatHost(Host) + ":" + (Port ?? defaultPort).ToString(CultureInfo.InvariantCulture);

        /// <summary>The address to send to (literals only; names need DNS first).</summary>
        public IPEndPoint ToIPEndPoint(int defaultPort)
        {
            if (!IsLiteral) throw new InvalidOperationException("a host name has no address before DNS resolution");
            return new IPEndPoint(IPAddress.Parse(Host), Port ?? defaultPort);
        }

        public override string ToString() =>
            Port.HasValue ? ToAuthority(Port.Value) : Endpoint.FormatHost(Host);
    }

    /// <summary>
    /// Parses and formats network addresses the way people actually type them: "host", "host:port", a bare
    /// IPv6 address, "[v6]" or "[v6]:port", even pasted as a whole srt:// URL or typed with a CJK input method
    /// (full-width "：８８９０"). Link-local and zone-scoped addresses are rejected: they only work on one network
    /// segment and ffmpeg's SRT URL parser cannot carry a zone id.
    /// </summary>
    public static class Endpoint
    {
        /// <summary>Parses <paramref name="input"/>; on failure <paramref name="error"/> is a sentence for the user.</summary>
        public static bool TryParse(string? input, out HostPort? result, out string error, bool allowUnspecified = false)
        {
            result = null;
            error = "";
            string text = Normalize(input);
            if (text.Length == 0)
            {
                error = "the address is empty";
                return false;
            }

            if (text.StartsWith("srt://", StringComparison.OrdinalIgnoreCase)) text = text.Substring(6);
            int cut = text.IndexOfAny(new[] { '?', '/' });
            if (cut >= 0) text = text.Substring(0, cut);
            int at = text.LastIndexOf('@');
            if (at >= 0) text = text.Substring(at + 1);   // user@host from a pasted URL

            string host;
            string? portText = null;
            if (text.StartsWith("[", StringComparison.Ordinal))
            {
                int close = text.IndexOf(']');
                if (close < 0)
                {
                    error = "the IPv6 address has no closing ']'";
                    return false;
                }
                host = text.Substring(1, close - 1);
                string rest = text.Substring(close + 1);
                if (rest.Length > 0)
                {
                    if (rest[0] != ':' || rest.Length == 1)
                    {
                        error = "expected ':port' after ']'";
                        return false;
                    }
                    portText = rest.Substring(1);
                }
                if (host.IndexOf(':') < 0)
                {
                    error = "only IPv6 addresses go in [brackets]";
                    return false;
                }
            }
            else
            {
                int colons = 0;
                foreach (char c in text) if (c == ':') colons++;
                if (colons == 1)
                {
                    int colon = text.IndexOf(':');
                    host = text.Substring(0, colon);
                    portText = text.Substring(colon + 1);
                }
                else
                {
                    // 0 colons: a name or IPv4; 2+: a bare IPv6 address (a port needs brackets: [v6]:port)
                    host = text;
                }
            }

            int? port = null;
            if (portText != null)
            {
                if (!int.TryParse(portText, NumberStyles.None, CultureInfo.InvariantCulture, out int p) || p < 1 || p > 65535)
                {
                    error = "the port must be a number from 1 to 65535";
                    return false;
                }
                port = p;
            }

            if (!TryClassifyHost(host, allowUnspecified, out string canonical, out EndpointKind kind, out error)) return false;
            result = new HostPort(canonical, port, kind);
            return true;
        }

        /// <summary>Like <see cref="TryParse"/> but throws <see cref="FormatException"/>.</summary>
        public static HostPort Parse(string? input, bool allowUnspecified = false)
        {
            if (!TryParse(input, out HostPort? hp, out string error, allowUnspecified)) throw new FormatException(error);
            return hp!;
        }

        /// <summary>Puts IPv6 literals in brackets for a URL authority; everything else is returned unchanged.</summary>
        public static string FormatHost(string host)
        {
            if (string.IsNullOrEmpty(host)) return host ?? "";
            if (host.StartsWith("[", StringComparison.Ordinal)) return host;
            return host.IndexOf(':') >= 0 ? "[" + host + "]" : host;
        }

        /// <summary>"[v6]:port" or "v4:port" for an <see cref="IPEndPoint"/> (IPv4-mapped IPv6 is shown as IPv4).</summary>
        public static string Format(IPEndPoint ep)
        {
            IPAddress a = Unmap(ep.Address);
            return FormatHost(a.ToString()) + ":" + ep.Port.ToString(CultureInfo.InvariantCulture);
        }

        /// <summary>::ffff:a.b.c.d → a.b.c.d (dual-stack sockets report IPv4 peers this way).</summary>
        public static IPAddress Unmap(IPAddress a) =>
            a.AddressFamily == AddressFamily.InterNetworkV6 && a.IsIPv4MappedToIPv6 ? a.MapToIPv4() : a;

        /// <summary>
        /// NFKC (full-width letters, digits and '：' become ASCII), ideographic full stops become '.', surrounding
        /// quotes / corner brackets and whitespace are removed.
        /// </summary>
        public static string Normalize(string? input)
        {
            if (string.IsNullOrEmpty(input)) return "";
            string s = input!.Normalize(NormalizationForm.FormKC).Replace('\u3002', '.').Trim();
            string[] pairs = { "\"\"", "''", "「」", "『』", "<>", "（）", "()" };
            bool changed = true;
            while (changed && s.Length >= 2)
            {
                changed = false;
                foreach (string pair in pairs)
                {
                    if (s[0] == pair[0] && s[s.Length - 1] == pair[1])
                    {
                        s = s.Substring(1, s.Length - 2).Trim();
                        changed = true;
                        break;
                    }
                }
            }
            return s;
        }

        static bool TryClassifyHost(string host, bool allowUnspecified, out string canonical, out EndpointKind kind, out string error)
        {
            canonical = "";
            kind = EndpointKind.Hostname;
            error = "";
            if (host.Length == 0)
            {
                error = "the host is empty";
                return false;
            }
            if (host.IndexOf('%') >= 0)
            {
                error = "zone ids (%...) are not supported; use the global address instead of fe80::";
                return false;
            }

            if (host.IndexOf(':') >= 0)
            {
                if (!IPAddress.TryParse(host, out IPAddress? v6) || v6!.AddressFamily != AddressFamily.InterNetworkV6)
                {
                    error = "'" + host + "' is not a valid IPv6 address";
                    return false;
                }
                if (v6.IsIPv4MappedToIPv6)
                {
                    return TryClassifyHost(v6.MapToIPv4().ToString(), allowUnspecified, out canonical, out kind, out error);
                }
                if (v6.IsIPv6LinkLocal)
                {
                    error = "fe80:: link-local addresses only work inside one network; use the global IPv6 address";
                    return false;
                }
                if (v6.IsIPv6Multicast)
                {
                    error = "that is a multicast address";
                    return false;
                }
                if (v6.Equals(IPAddress.IPv6Any) && !allowUnspecified)
                {
                    error = ":: is not an address you can connect to";
                    return false;
                }
                canonical = v6.ToString();
                kind = EndpointKind.IPv6;
                return true;
            }

            if (LooksNumeric(host))
            {
                if (!IsStrictIPv4(host))
                {
                    error = "'" + host + "' is not a valid IPv4 address";
                    return false;
                }
                if (host == "0.0.0.0" && !allowUnspecified)
                {
                    error = "0.0.0.0 is not an address you can connect to";
                    return false;
                }
                canonical = host;
                kind = EndpointKind.IPv4;
                return true;
            }

            if (!IsHostName(host))
            {
                error = "'" + host + "' is not a valid host name";
                return false;
            }
            canonical = host.TrimEnd('.');
            kind = EndpointKind.Hostname;
            return true;
        }

        static bool LooksNumeric(string s)
        {
            foreach (char c in s)
                if (!(c >= '0' && c <= '9') && c != '.') return false;
            return true;
        }

        /// <summary>Exactly four decimal octets 0-255 (IPAddress.Parse also takes "1" or octal forms; people don't mean those).</summary>
        static bool IsStrictIPv4(string s)
        {
            string[] parts = s.Split('.');
            if (parts.Length != 4) return false;
            foreach (string p in parts)
            {
                if (p.Length == 0 || p.Length > 3) return false;
                if (p.Length > 1 && p[0] == '0') return false;
                if (!int.TryParse(p, NumberStyles.None, CultureInfo.InvariantCulture, out int v) || v > 255) return false;
            }
            return true;
        }

        static bool IsHostName(string s)
        {
            if (s.Length > 253) return false;
            string trimmed = s.EndsWith(".", StringComparison.Ordinal) ? s.Substring(0, s.Length - 1) : s;
            if (trimmed.Length == 0) return false;
            foreach (string label in trimmed.Split('.'))
            {
                if (label.Length == 0 || label.Length > 63) return false;
                if (label[0] == '-' || label[label.Length - 1] == '-') return false;
                foreach (char c in label)
                {
                    bool ok = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '-';
                    if (!ok) return false;
                }
            }
            return true;
        }
    }
}
