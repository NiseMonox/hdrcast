using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;
using HdrCast.Common;

namespace HdrCast.Group
{
    /// <summary>
    /// {"p": b64u(payload JSON), "m": b64u(HMAC-SHA256(msgKey, kind\ngid\nkey\np))} (docs/design.md §8.4).
    /// The MAC covers the exact base64url text, so nobody has to agree on JSON formatting; binding kind, group and
    /// key (host id or session id) stops a signed message from being replayed somewhere else.
    /// </summary>
    public sealed class SignedEnvelope
    {
        public const string KindLive = "live";
        public const string KindJoin = "join";
        public const string KindAnswer = "answer";

        public SignedEnvelope(string p, string m)
        {
            P = p ?? "";
            M = m ?? "";
        }

        public string P { get; }
        public string M { get; }

        public static SignedEnvelope Seal(GroupKeys keys, string kind, string gid, string key, IDictionary<string, object?> payload)
        {
            string p = Base64Url.Encode(Encoding.UTF8.GetBytes(MiniJson.Serialize(payload)));
            return new SignedEnvelope(p, Mac(keys, kind, gid, key, p));
        }

        /// <summary>Verifies the MAC, then decodes the payload object; false on any mismatch.</summary>
        public bool TryOpen(GroupKeys keys, string kind, string gid, string key, out Dictionary<string, object?>? payload)
        {
            payload = null;
            if (P.Length == 0 || P.Length > 16384) return false;
            if (!Base64Url.TryDecode(M, out byte[] mac)) return false;
            byte[] expected = Base64Url.Decode(Mac(keys, kind, gid, key, P));
            if (!GroupKeys.FixedTimeEquals(mac, expected)) return false;
            if (!Base64Url.TryDecode(P, out byte[] json)) return false;
            try
            {
                payload = MiniJson.AsObject(MiniJson.Parse(Encoding.UTF8.GetString(json)));
            }
            catch (FormatException)
            {
                return false;
            }
            return payload != null;
        }

        public Dictionary<string, object?> ToJsonObject() => new Dictionary<string, object?> { ["p"] = P, ["m"] = M };

        public string ToJson() => MiniJson.Serialize(ToJsonObject());

        public static SignedEnvelope? FromJsonObject(object? obj)
        {
            string? p = MiniJson.GetString(obj, "p");
            string? m = MiniJson.GetString(obj, "m");
            return p == null || m == null ? null : new SignedEnvelope(p, m);
        }

        static string Mac(GroupKeys keys, string kind, string gid, string key, string p) =>
            Base64Url.Encode(GroupKeys.HmacSha256(keys.MsgKey, kind + "\n" + gid + "\n" + key + "\n" + p));
    }

    /// <summary>X-Hdrcast-Auth: &lt;ts ms&gt;.&lt;nonce&gt;.&lt;mac&gt; (docs/design.md §8.5).</summary>
    public static class RequestAuth
    {
        public const string HeaderName = "X-Hdrcast-Auth";

        public static string Header(byte[] serverKey, string method, string pathAndQuery, byte[] body, long tsMs, string nonce)
        {
            string bodyHash = Base64Url.Encode(GroupKeys.Sha256(body ?? new byte[0]));
            string msg = method + "\n" + pathAndQuery + "\n" + tsMs.ToString(CultureInfo.InvariantCulture) + "\n" + nonce + "\n" + bodyHash;
            string mac = Base64Url.Encode(GroupKeys.HmacSha256(serverKey, msg));
            return tsMs.ToString(CultureInfo.InvariantCulture) + "." + nonce + "." + mac;
        }

        public static string Header(byte[] serverKey, string method, string pathAndQuery, byte[] body) =>
            Header(serverKey, method, pathAndQuery, body, DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(),
                   Base64Url.Encode(GroupKeys.RandomBytes(8)));
    }

    /// <summary>What a live host advertises every 10 s (kind "live").</summary>
    public sealed class LiveRecord
    {
        public string HostId { get; set; } = "";
        public string Name { get; set; } = "";
        public long TimeMs { get; set; }
        public long SinceMs { get; set; }
        public List<string> Candidates { get; set; } = new List<string>();
        public string Codec { get; set; } = "hevc";
        public int Mbps { get; set; }
        public int MaxViewers { get; set; }
        public int Viewers { get; set; }
        /// <summary>
        /// The host's IPv4 port takes packets from any address and port (router forward + firewall rule), so viewers
        /// whose routers change ports can reach it. Null = not known yet, or a host older than 0.2.4.
        /// </summary>
        public bool? OpenIpv4 { get; set; }

        public Dictionary<string, object?> ToPayload()
        {
            var p = new Dictionary<string, object?>
            {
                ["v"] = 1, ["t"] = TimeMs, ["host"] = HostId, ["name"] = Name, ["since"] = SinceMs,
                ["cands"] = Candidates.ToArray(), ["codec"] = Codec, ["mbps"] = Mbps, ["max"] = MaxViewers, ["viewers"] = Viewers,
            };
            if (OpenIpv4.HasValue) p["open4"] = OpenIpv4.Value;
            return p;
        }

        public static LiveRecord? FromPayload(Dictionary<string, object?> p, string expectedHost)
        {
            var r = new LiveRecord
            {
                HostId = MiniJson.GetString(p, "host") ?? "",
                Name = GroupConfig.CleanDisplayName(MiniJson.GetString(p, "name")),
                TimeMs = MiniJson.AsLong(MiniJson.Get(p, "t")) ?? 0,
                SinceMs = MiniJson.AsLong(MiniJson.Get(p, "since")) ?? 0,
                Candidates = Messages.Strings(MiniJson.Get(p, "cands")),
                Codec = MiniJson.GetString(p, "codec") ?? "hevc",
                Mbps = (int)(MiniJson.AsLong(MiniJson.Get(p, "mbps")) ?? 0),
                MaxViewers = (int)(MiniJson.AsLong(MiniJson.Get(p, "max")) ?? 0),
                Viewers = (int)(MiniJson.AsLong(MiniJson.Get(p, "viewers")) ?? 0),
                OpenIpv4 = MiniJson.AsBool(MiniJson.Get(p, "open4")),
            };
            return r.HostId == expectedHost ? r : null;
        }
    }

    /// <summary>A viewer asking a host for a session (kind "join").</summary>
    public sealed class JoinRequest
    {
        public string SessionId { get; set; } = "";
        public string ViewerId { get; set; } = "";
        public string Name { get; set; } = "";
        public long TimeMs { get; set; }
        public List<string> Candidates { get; set; } = new List<string>();
        /// <summary>SRT latency the viewer wants (its region preset), ms.</summary>
        public int LatencyMs { get; set; }
        /// <summary>oheadbw the viewer wants (its region preset), percent.</summary>
        public int OverheadPercent { get; set; }

        public Dictionary<string, object?> ToPayload() => new Dictionary<string, object?>
        {
            ["v"] = 1, ["t"] = TimeMs, ["sid"] = SessionId, ["viewer"] = ViewerId, ["name"] = Name,
            ["cands"] = Candidates.ToArray(), ["latency"] = LatencyMs, ["overhead"] = OverheadPercent,
        };

        public static JoinRequest? FromPayload(Dictionary<string, object?> p)
        {
            var j = new JoinRequest
            {
                SessionId = MiniJson.GetString(p, "sid") ?? "",
                ViewerId = MiniJson.GetString(p, "viewer") ?? "",
                Name = GroupConfig.CleanDisplayName(MiniJson.GetString(p, "name")),
                TimeMs = MiniJson.AsLong(MiniJson.Get(p, "t")) ?? 0,
                Candidates = Messages.Strings(MiniJson.Get(p, "cands")),
                LatencyMs = (int)(MiniJson.AsLong(MiniJson.Get(p, "latency")) ?? 0),
                OverheadPercent = (int)(MiniJson.AsLong(MiniJson.Get(p, "overhead")) ?? 0),
            };
            if (!GroupKeys.IsValidId(j.SessionId) || !GroupKeys.IsValidId(j.ViewerId)) return null;
            if (j.LatencyMs < 20 || j.LatencyMs > 30000) j.LatencyMs = 1500;
            if (j.OverheadPercent < 5 || j.OverheadPercent > 100) j.OverheadPercent = 25;
            return j;
        }
    }

    /// <summary>The host's reply to a join (kind "answer").</summary>
    public sealed class JoinAnswer
    {
        public const string ReasonFull = "full";
        public const string ReasonStopped = "stopped";
        public const string ReasonBusy = "busy";

        public string SessionId { get; set; } = "";
        public long TimeMs { get; set; }
        public bool Ok { get; set; }
        public string Reason { get; set; } = "";
        public List<string> Candidates { get; set; } = new List<string>();

        public Dictionary<string, object?> ToPayload() => new Dictionary<string, object?>
        {
            ["v"] = 1, ["t"] = TimeMs, ["sid"] = SessionId, ["ok"] = Ok, ["reason"] = Reason, ["cands"] = Candidates.ToArray(),
        };

        public static JoinAnswer? FromPayload(Dictionary<string, object?> p, string expectedSid)
        {
            var a = new JoinAnswer
            {
                SessionId = MiniJson.GetString(p, "sid") ?? "",
                TimeMs = MiniJson.AsLong(MiniJson.Get(p, "t")) ?? 0,
                Ok = MiniJson.AsBool(MiniJson.Get(p, "ok")) ?? false,
                Reason = MiniJson.GetString(p, "reason") ?? "",
                Candidates = Messages.Strings(MiniJson.Get(p, "cands")),
            };
            return a.SessionId == expectedSid ? a : null;
        }
    }

    static class Messages
    {
        public const int MaxCandidates = 8;

        /// <summary>A JSON array of strings, keeping only entries Endpoint can parse (at most 8).</summary>
        public static List<string> Strings(object? value)
        {
            var list = new List<string>();
            var arr = MiniJson.AsArray(value);
            if (arr == null) return list;
            foreach (object? o in arr)
            {
                if (list.Count >= MaxCandidates) break;
                if (o is string s && Endpoint.TryParse(s, out HostPort? hp, out _) && hp!.Port.HasValue && hp.IsLiteral) list.Add(s);
            }
            return list;
        }
    }
}
