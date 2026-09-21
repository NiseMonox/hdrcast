#pragma warning disable SYSLIB0014 // HttpWebRequest: the net48 apps have no HttpClient without extra packages
using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Net;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using HdrCast.Common;
using HdrCast.Group;

namespace HdrCast.P2p
{
    /// <summary>A failed signaling call. <see cref="Unreachable"/> = no HTTP answer at all (network, DNS, timeout).</summary>
    public sealed class SignalingException : Exception
    {
        public SignalingException(string message, int status, bool unreachable, Exception? inner = null) : base(message, inner)
        {
            Status = status;
            Unreachable = unreachable;
        }

        public int Status { get; }
        public bool Unreachable { get; }
    }

    /// <summary>A verified entry of the live list.</summary>
    public sealed class LiveEntry
    {
        public LiveEntry(LiveRecord record, long ageMs) { Record = record; AgeMs = ageMs; }
        public LiveRecord Record { get; }
        public long AgeMs { get; }
    }

    /// <summary>
    /// The signaling HTTP API (docs/design.md §8.5). Every call authenticates with the group's server key; every
    /// envelope that comes back is verified with the message key before anyone looks inside, so a compromised or
    /// impersonated server can withhold information but cannot point a viewer somewhere else.
    /// </summary>
    public sealed class SignalingClient
    {
        public const int DefaultTimeoutMs = 8000;
        public const int MaxWaitSeconds = 25;

        readonly string _base;
        readonly GroupKeys _keys;

        public SignalingClient(string baseUrl, string groupId, GroupKeys keys, int timeoutMs = DefaultTimeoutMs)
        {
            _base = (baseUrl ?? "").TrimEnd('/');
            GroupId = groupId;
            _keys = keys;
            TimeoutMs = timeoutMs;
            if (Uri.TryCreate(_base, UriKind.Absolute, out Uri? u))
            {
                try
                {
                    ServicePoint sp = ServicePointManager.FindServicePoint(u);
                    // net48 allows 2 connections per host by default; a long-poll plus a heartbeat plus a live-list
                    // refresh would queue behind each other.
                    sp.ConnectionLimit = 16;
                    // The server closes idle keep-alive connections after 30 s; the default 100 s would reuse dead ones.
                    sp.MaxIdleTime = 20000;
                    // Every heartbeat / join / answer would otherwise wait a round trip for "100 Continue".
                    sp.Expect100Continue = false;
                }
                catch (Exception) { }
            }
        }

        public SignalingClient(GroupConfig group) : this(group.SignalUrl, group.GroupId, group.Keys()) { }

        public string GroupId { get; }
        public int TimeoutMs { get; set; }
        public string BaseUrl => _base;

        /// <summary>Host name or address of the signaling server (the UDP echo lives there too).</summary>
        public string Host => Uri.TryCreate(_base, UriKind.Absolute, out Uri? u) ? u.Host.Trim('[', ']') : "";

        public bool Health(CancellationToken ct)
        {
            try
            {
                Send("GET", "/v1/health", null, false, TimeoutMs, ct);
                return true;
            }
            catch (SignalingException)
            {
                return false;
            }
        }

        /// <summary>Registers the group (first come); 200/201 are both success, 409 means the id is taken.</summary>
        public void RegisterGroup(CancellationToken ct)
        {
            string body = MiniJson.Serialize(new Dictionary<string, object?> { ["serverKey"] = Base64Url.Encode(_keys.ServerKey) });
            Send("PUT", "/v1/g/" + GroupId, body, true, TimeoutMs, ct);
        }

        public void PutLive(string hostId, SignedEnvelope envelope, CancellationToken ct) =>
            Send("PUT", "/v1/g/" + GroupId + "/live/" + hostId, envelope.ToJson(), true, TimeoutMs, ct);

        public void DeleteLive(string hostId, CancellationToken ct) =>
            Send("DELETE", "/v1/g/" + GroupId + "/live/" + hostId, null, true, TimeoutMs, ct);

        /// <summary>The live list, keeping only records with a valid signature.</summary>
        public List<LiveEntry> GetLive(CancellationToken ct)
        {
            string json = Send("GET", "/v1/g/" + GroupId + "/live", null, true, TimeoutMs, ct).Body;
            var result = new List<LiveEntry>();
            var hosts = MiniJson.AsArray(MiniJson.Get(Parse(json), "hosts"));
            if (hosts == null) return result;
            foreach (object? h in hosts)
            {
                string? id = MiniJson.GetString(h, "host");
                SignedEnvelope? env = SignedEnvelope.FromJsonObject(h);
                if (id == null || env == null || !GroupKeys.IsValidId(id)) continue;
                if (!env.TryOpen(_keys, SignedEnvelope.KindLive, GroupId, id, out var payload)) continue;
                LiveRecord? rec = LiveRecord.FromPayload(payload!, id);
                if (rec != null) result.Add(new LiveEntry(rec, MiniJson.AsLong(MiniJson.Get(h, "age_ms")) ?? 0));
            }
            return result;
        }

        /// <summary>Drops a join request in the host's mailbox; throws with Status 404 when the host is not live.</summary>
        public long PostJoin(string hostId, SignedEnvelope envelope, CancellationToken ct)
        {
            string json = Send("POST", "/v1/g/" + GroupId + "/live/" + hostId + "/join", envelope.ToJson(), true, TimeoutMs, ct).Body;
            return MiniJson.AsLong(MiniJson.Get(Parse(json), "seq")) ?? 0;
        }

        /// <summary>Long-polls the host's mailbox for joins after <paramref name="after"/>; returns (seq, envelope) pairs and the next cursor.</summary>
        public List<KeyValuePair<long, SignedEnvelope>> PollJoins(string hostId, long after, int waitSeconds, out long next, CancellationToken ct)
        {
            int wait = Math.Max(0, Math.Min(MaxWaitSeconds, waitSeconds));
            string path = "/v1/g/" + GroupId + "/live/" + hostId + "/joins?after=" + after.ToString(CultureInfo.InvariantCulture) +
                          "&wait=" + wait.ToString(CultureInfo.InvariantCulture);
            string json = Send("GET", path, null, true, TimeoutMs + wait * 1000, ct).Body;
            object? root = Parse(json);
            next = MiniJson.AsLong(MiniJson.Get(root, "next")) ?? after;
            var list = new List<KeyValuePair<long, SignedEnvelope>>();
            var joins = MiniJson.AsArray(MiniJson.Get(root, "joins"));
            if (joins == null) return list;
            foreach (object? j in joins)
            {
                long seq = MiniJson.AsLong(MiniJson.Get(j, "seq")) ?? 0;
                SignedEnvelope? env = SignedEnvelope.FromJsonObject(j);
                if (env != null) list.Add(new KeyValuePair<long, SignedEnvelope>(seq, env));
                if (seq > next) next = seq;
            }
            return list;
        }

        public void PutAnswer(string sid, SignedEnvelope envelope, CancellationToken ct) =>
            Send("PUT", "/v1/g/" + GroupId + "/answer/" + sid, envelope.ToJson(), true, TimeoutMs, ct);

        /// <summary>Long-polls for the host's answer; null when none arrived within <paramref name="waitSeconds"/>.</summary>
        public SignedEnvelope? WaitAnswer(string sid, int waitSeconds, CancellationToken ct)
        {
            int wait = Math.Max(0, Math.Min(MaxWaitSeconds, waitSeconds));
            string path = "/v1/g/" + GroupId + "/answer/" + sid + "?wait=" + wait.ToString(CultureInfo.InvariantCulture);
            Response r = Send("GET", path, null, true, TimeoutMs + wait * 1000, ct);
            if (r.Status == 204) return null;
            return SignedEnvelope.FromJsonObject(Parse(r.Body));
        }

        struct Response
        {
            public int Status;
            public string Body;
        }

        Response Send(string method, string pathAndQuery, string? body, bool authenticate, int timeoutMs, CancellationToken ct)
        {
            ct.ThrowIfCancellationRequested();
            byte[] bytes = body == null ? new byte[0] : Encoding.UTF8.GetBytes(body);
            HttpWebRequest req;
            try
            {
                req = (HttpWebRequest)WebRequest.Create(_base + pathAndQuery);
            }
            catch (Exception ex) when (ex is UriFormatException || ex is NotSupportedException)
            {
                throw new SignalingException("bad signaling address: " + _base, 0, unreachable: true, inner: ex);
            }
            req.Method = method;
            req.Proxy = null;                   // system proxies (and WPAD lookups) would add seconds or break it
            req.Timeout = timeoutMs;
            req.ReadWriteTimeout = timeoutMs;
            req.KeepAlive = true;
            req.UserAgent = "HdrCast/1";
            req.Accept = "application/json";
            req.ServicePoint.Expect100Continue = false;
            if (authenticate) req.Headers[RequestAuth.HeaderName] = RequestAuth.Header(_keys.ServerKey, method, pathAndQuery, bytes);

            try
            {
                if (method == "PUT" || method == "POST")
                {
                    req.ContentType = "application/json";
                    req.ContentLength = bytes.Length;
                    using (Stream s = req.GetRequestStream()) s.Write(bytes, 0, bytes.Length);
                }
            }
            catch (WebException ex)
            {
                ct.ThrowIfCancellationRequested();
                throw new SignalingException("signaling server unreachable (" + ex.Status + ")", 0, unreachable: true, inner: ex);
            }
            catch (IOException ex)
            {
                throw new SignalingException("signaling connection failed", 0, unreachable: true, inner: ex);
            }

            // Wait on the response with the token rather than blocking in GetResponse: HttpWebRequest.Abort() does not
            // interrupt a blocking GetResponse on every runtime, and a stop must not sit out a 20 s long-poll.
            Task<WebResponse> pending = req.GetResponseAsync();
            int done;
            try
            {
                done = Task.WaitAny(new Task[] { pending }, timeoutMs, ct);
            }
            catch (OperationCanceledException)
            {
                Abandon(req, pending);
                throw;
            }
            if (done < 0)
            {
                Abandon(req, pending);
                throw new SignalingException("signaling server did not answer in time", 0, unreachable: true);
            }

            try
            {
                using (var resp = (HttpWebResponse)pending.Result)
                    return new Response { Status = (int)resp.StatusCode, Body = ReadBody(resp) };
            }
            catch (AggregateException agg)
            {
                Exception inner = agg.InnerException ?? agg;
                if (inner is WebException ex && ex.Response is HttpWebResponse er)
                {
                    int status = (int)er.StatusCode;
                    string text;
                    using (er) text = ReadBody(er);
                    string error = MiniJson.GetString(SafeParse(text), "error") ?? er.StatusDescription;
                    throw new SignalingException("signaling " + status + ": " + error, status, unreachable: false, inner: ex);
                }
                ct.ThrowIfCancellationRequested();
                string why = inner is WebException wex ? "unreachable (" + wex.Status + ")" : "connection failed";
                throw new SignalingException("signaling server " + why, 0, unreachable: true, inner: inner);
            }
            catch (IOException ex)
            {
                ct.ThrowIfCancellationRequested();
                throw new SignalingException("signaling connection failed", 0, unreachable: true, inner: ex);
            }
        }

        /// <summary>Gives up on a request: aborts it and quietly disposes whatever arrives later.</summary>
        static void Abandon(HttpWebRequest req, Task<WebResponse> pending)
        {
            try { req.Abort(); } catch (Exception) { }
            pending.ContinueWith(t =>
            {
                if (t.IsFaulted) { Exception? _ = t.Exception; }
                else if (t.Status == TaskStatus.RanToCompletion) { try { t.Result.Dispose(); } catch (Exception) { } }
            }, TaskContinuationOptions.ExecuteSynchronously);
        }

        /// <summary>
        /// Largest body we accept: 32 live hosts or 32 queued joins at up to 8 KB each stay well below it;
        /// anything bigger is not our server.
        /// </summary>
        const int MaxBodyChars = 512 * 1024;

        static string ReadBody(HttpWebResponse resp)
        {
            using (Stream s = resp.GetResponseStream())
            using (var r = new StreamReader(s, Encoding.UTF8))
            {
                var sb = new StringBuilder();
                var buf = new char[16384];
                int n;
                while ((n = r.Read(buf, 0, buf.Length)) > 0)
                {
                    if (sb.Length + n > MaxBodyChars) throw new IOException("the signaling response is too large");
                    sb.Append(buf, 0, n);
                }
                return sb.ToString();
            }
        }

        static object? Parse(string json)
        {
            try { return MiniJson.Parse(json); }
            catch (FormatException ex) { throw new SignalingException("signaling sent something that is not JSON", 0, unreachable: false, inner: ex); }
        }

        static object? SafeParse(string json)
        {
            try { return MiniJson.Parse(json); } catch (FormatException) { return null; }
        }
    }
}
