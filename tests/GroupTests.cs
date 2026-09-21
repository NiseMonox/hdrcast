using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Text;
using System.Threading.Tasks;
using HdrCast.Common;
using HdrCast.Group;
using Xunit;

namespace HdrCast.Tests
{
    /// <summary>Reads tests/vectors/group-v1.json (made by make_group_v1.py, the reference implementation).</summary>
    public static class Vectors
    {
        static object? _root;

        public static string RepoRoot()
        {
            var dir = new DirectoryInfo(AppContext.BaseDirectory);
            while (dir != null && !File.Exists(Path.Combine(dir.FullName, "build.sh"))) dir = dir.Parent;
            return dir?.FullName ?? throw new InvalidOperationException("repo root not found");
        }

        public static object? Root => _root ??= MiniJson.Parse(File.ReadAllText(Path.Combine(RepoRoot(), "tests", "vectors", "group-v1.json")));

        public static string S(string key) => MiniJson.GetString(Root, key) ?? throw new KeyNotFoundException(key);
        public static object? Get(params string[] path)
        {
            object? o = Root;
            foreach (string p in path) o = MiniJson.Get(o, p);
            return o;
        }

        public static byte[] Key => Base64Url.Decode(S("groupKey"));
        public static GroupKeys Keys => new GroupKeys(Key);

        public static byte[] Hex(string hex)
        {
            var b = new byte[hex.Length / 2];
            for (int i = 0; i < b.Length; i++) b[i] = Convert.ToByte(hex.Substring(i * 2, 2), 16);
            return b;
        }
    }

    public class GroupKeyTests
    {
        [Fact]
        public void Derived_keys_match_the_reference()
        {
            GroupKeys k = Vectors.Keys;
            Assert.Equal(Vectors.S("serverKey"), Base64Url.Encode(k.ServerKey));
            Assert.Equal(Vectors.S("msgKey"), Base64Url.Encode(k.MsgKey));
            Assert.Equal(Vectors.S("srtPassphrase"), k.SrtPassphrase);
            Assert.Equal(Vectors.S("punchKey"), Base64Url.Encode(k.PunchKey(Vectors.S("sid"))));
            Assert.True(SrtUrl.IsValidPassphrase(k.SrtPassphrase));
            Assert.Equal(32, k.SrtPassphrase.Length);
        }

        [Fact]
        public void Ids_are_22_base64url_characters()
        {
            string id = GroupKeys.NewId();
            Assert.Equal(22, id.Length);
            Assert.True(GroupKeys.IsValidId(id));
            Assert.True(GroupKeys.IsValidId(Vectors.S("gid")));
            Assert.False(GroupKeys.IsValidId("short"));
            Assert.False(GroupKeys.IsValidId("oKGio6SlpqeoqaqrrK2ur+"));
            Assert.False(GroupKeys.IsValidId(null));
        }

        [Fact]
        public void Base64url_round_trips_and_accepts_padding()
        {
            byte[] data = { 0xfb, 0xff, 0x00, 0x10 };
            string s = Base64Url.Encode(data);
            Assert.Equal("-_8AEA", s);
            Assert.Equal(data, Base64Url.Decode(s));
            Assert.Equal(data, Base64Url.Decode(s + "=="));
            Assert.False(Base64Url.TryDecode("a", out _));
            Assert.False(Base64Url.TryDecode("ab+c", out _));
        }
    }

    public class InviteTests
    {
        static Invite VectorInvite() =>
            new Invite("http://101.43.23.94:18650", Vectors.S("gid"), Vectors.Key, "测试组");

        [Fact]
        public void Encoding_matches_the_reference_byte_for_byte()
        {
            Assert.Equal(Vectors.S("inviteCode"), InviteCodec.Encode(VectorInvite()));
        }

        [Fact]
        public void The_reference_code_decodes()
        {
            Invite inv = InviteCodec.Decode(Vectors.S("inviteCode"));
            Assert.Equal("http://101.43.23.94:18650", inv.SignalUrl);
            Assert.Equal(Vectors.S("gid"), inv.GroupId);
            Assert.Equal(Vectors.Key, inv.GroupKey);
            Assert.Equal("测试组", inv.GroupName);
        }

        [Fact]
        public void A_new_group_round_trips()
        {
            Invite created = Invite.Create("宇晨的朋友们");
            Invite back = InviteCodec.Decode(InviteCodec.Encode(created));
            Assert.Equal(created.GroupId, back.GroupId);
            Assert.Equal(created.GroupKey, back.GroupKey);
            Assert.Equal(Invite.DefaultSignalUrl, back.SignalUrl);
            Assert.True(InviteCodec.Encode(created).Length < 260);
        }

        [Fact]
        public void Survives_what_chat_apps_do_to_text()
        {
            string code = Vectors.S("inviteCode");
            // wrapped every 40 characters with CRLF, zero-width spaces sprinkled in, text around it
            var wrapped = new StringBuilder();
            for (int i = 0; i < code.Length; i++)
            {
                if (i > 0 && i % 40 == 0) wrapped.Append("\r\n");
                if (i % 17 == 5) wrapped.Append('\u200B');
                wrapped.Append(code[i]);
            }
            string message = "宇晨：来看我直播！\n" + wrapped + "\n复制整段就行。";
            Assert.True(InviteCodec.TryExtract(message, out Invite? inv, out InviteError err), err.ToString());
            Assert.Equal(Vectors.S("gid"), inv!.GroupId);

            // full-width prefix typed by an IME, and upper-case
            string fullWidth = "ｈｄｒｃａｓｔ：" + code.Substring("hdrcast:".Length);
            Assert.True(InviteCodec.TryExtract(fullWidth, out _, out _));
            Assert.True(InviteCodec.TryExtract("HDRCAST:" + code.Substring(8) + "，谢谢", out _, out _));
        }

        [Fact]
        public void Text_directly_after_the_checksum_is_ignored()
        {
            string code = Vectors.S("inviteCode");
            Assert.True(InviteCodec.TryExtract(code + "ok.bye", out _, out _));
            Assert.True(InviteCodec.TryExtract(code + "\nsee you.", out _, out _));
        }

        [Fact]
        public void Truncated_damaged_newer_and_missing_codes_are_told_apart()
        {
            string code = Vectors.S("inviteCode");
            Assert.False(InviteCodec.TryExtract("hello", out _, out InviteError e1));
            Assert.Equal(InviteError.NotFound, e1);

            Assert.False(InviteCodec.TryExtract(code.Substring(0, code.Length - 20), out _, out InviteError e2));
            Assert.Equal(InviteError.Incomplete, e2);
            Assert.False(InviteCodec.TryExtract(code.Substring(0, code.Length - 3), out _, out InviteError e3));
            Assert.Equal(InviteError.Incomplete, e3);

            char[] flipped = code.ToCharArray();
            flipped[20] = flipped[20] == 'A' ? 'B' : 'A';
            Assert.False(InviteCodec.TryExtract(new string(flipped), out _, out InviteError e4));
            Assert.Equal(InviteError.Damaged, e4);

            Assert.False(InviteCodec.TryExtract(Craft("{\"v\":2,\"s\":\"http://x:1\"}"), out _, out InviteError e5));
            Assert.Equal(InviteError.NewerVersion, e5);

            string badKey = "{\"v\":1,\"s\":\"http://101.43.23.94:18650\",\"g\":\"" + Vectors.S("gid") + "\",\"k\":\"AAAA\",\"n\":\"x\"}";
            Assert.False(InviteCodec.TryExtract(Craft(badKey), out _, out InviteError e6));
            Assert.Equal(InviteError.Invalid, e6);

            foreach (InviteError e in Enum.GetValues(typeof(InviteError)))
                Assert.False(string.IsNullOrWhiteSpace(InviteCodec.Describe(e)));
        }

        [Fact]
        public void The_first_valid_code_wins_over_a_broken_one_before_it()
        {
            string code = Vectors.S("inviteCode");
            string text = "old: " + code.Substring(0, 30) + "\nnew: " + code;
            Assert.True(InviteCodec.TryExtract(text, out Invite? inv, out _));
            Assert.Equal(Vectors.S("gid"), inv!.GroupId);
        }

        static string Craft(string json)
        {
            byte[] b = Encoding.UTF8.GetBytes(json);
            return "hdrcast:" + Base64Url.Encode(b) + "." + Base64Url.Encode(GroupKeys.Sha256(b)).Substring(0, 8);
        }
    }

    public class SignedEnvelopeTests
    {
        static Dictionary<string, object?> Payload(string kind) =>
            MiniJson.AsObject(Vectors.Get(kind, "payload")) ?? throw new InvalidOperationException(kind);

        [Theory]
        [InlineData("live", "host")]
        [InlineData("join", "host")]
        [InlineData("answer", "sid")]
        public void Sealing_matches_the_reference_and_opens_again(string kind, string keyField)
        {
            GroupKeys keys = Vectors.Keys;
            string gid = Vectors.S("gid");
            string key = Vectors.S(keyField);
            SignedEnvelope env = SignedEnvelope.Seal(keys, kind, gid, key, Payload(kind));
            Assert.Equal(MiniJson.GetString(Vectors.Get(kind, "envelope"), "p"), env.P);
            Assert.Equal(MiniJson.GetString(Vectors.Get(kind, "envelope"), "m"), env.M);

            Assert.True(env.TryOpen(keys, kind, gid, key, out var opened));
            Assert.Equal(1L, MiniJson.AsLong(MiniJson.Get(opened, "v")));
        }

        [Fact]
        public void Any_change_breaks_the_seal()
        {
            GroupKeys keys = Vectors.Keys;
            string gid = Vectors.S("gid");
            string host = Vectors.S("host");
            SignedEnvelope env = SignedEnvelope.FromJsonObject(Vectors.Get("live", "envelope"))!;
            Assert.True(env.TryOpen(keys, "live", gid, host, out _));

            Assert.False(env.TryOpen(keys, "join", gid, host, out _));                       // other kind
            Assert.False(env.TryOpen(keys, "live", GroupKeys.NewId(), host, out _));         // other group
            Assert.False(env.TryOpen(keys, "live", gid, GroupKeys.NewId(), out _));          // other host
            Assert.False(env.TryOpen(new GroupKeys(GroupKeys.RandomBytes(32)), "live", gid, host, out _));
            string p = env.P.Substring(0, env.P.Length - 1) + (env.P.EndsWith("A") ? "B" : "A");
            Assert.False(new SignedEnvelope(p, env.M).TryOpen(keys, "live", gid, host, out _));
            Assert.False(new SignedEnvelope(env.P, "garbage").TryOpen(keys, "live", gid, host, out _));
        }

        [Fact]
        public void Payload_models_read_what_the_reference_wrote()
        {
            var live = LiveRecord.FromPayload(Payload("live"), Vectors.S("host"))!;
            Assert.Equal("小明", live.Name);
            Assert.Equal(2, live.Candidates.Count);
            Assert.Equal(40, live.Mbps);
            Assert.Null(LiveRecord.FromPayload(Payload("live"), GroupKeys.NewId()));

            var join = JoinRequest.FromPayload(Payload("join"))!;
            Assert.Equal(Vectors.S("sid"), join.SessionId);
            Assert.Equal(3500, join.LatencyMs);
            Assert.Equal(50, join.OverheadPercent);

            var answer = JoinAnswer.FromPayload(Payload("answer"), Vectors.S("sid"))!;
            Assert.True(answer.Ok);
            Assert.Single(answer.Candidates);
        }

        [Fact]
        public void Candidates_are_filtered_to_parseable_literals_with_ports()
        {
            var p = new Dictionary<string, object?>
            {
                ["sid"] = GroupKeys.NewId(), ["viewer"] = GroupKeys.NewId(),
                ["cands"] = new List<object?> { "[2001:db8::1]:4000", "example.com:4000", "1.2.3.4", "fe80::1", 5, "10.0.0.2:5000" },
            };
            JoinRequest j = JoinRequest.FromPayload(p)!;
            Assert.Equal(new[] { "[2001:db8::1]:4000", "10.0.0.2:5000" }, j.Candidates);
        }

        [Fact]
        public void Request_auth_header_matches_the_reference()
        {
            object? a = Vectors.Get("auth");
            string header = RequestAuth.Header(Vectors.Keys.ServerKey, MiniJson.GetString(a, "method")!,
                MiniJson.GetString(a, "path")!, Encoding.UTF8.GetBytes(MiniJson.GetString(a, "body")!),
                MiniJson.AsLong(MiniJson.Get(a, "ts"))!.Value, MiniJson.GetString(a, "nonce")!);
            Assert.Equal(MiniJson.GetString(a, "header"), header);
            Assert.Equal(MiniJson.GetString(a, "emptyBodyHash"), Base64Url.Encode(GroupKeys.Sha256(new byte[0])));
        }
    }

    public class GroupConfigTests
    {
        [Fact]
        public void Adopting_an_invite_keeps_this_installs_peer_id()
        {
            var cfg = new GroupConfig().Normalize();
            string peer = cfg.PeerId;
            Assert.True(GroupKeys.IsValidId(peer));
            Assert.False(cfg.IsConfigured);

            GroupConfig joined = cfg.Adopt(InviteCodec.Decode(Vectors.S("inviteCode")), "  小红\n ");
            Assert.True(joined.IsConfigured);
            Assert.Equal(peer, joined.PeerId);
            Assert.Equal("小红", joined.DisplayName);
            Assert.Equal(Vectors.S("serverKey"), Base64Url.Encode(joined.Keys().ServerKey));
        }

        [Fact]
        public void Saves_and_loads_and_survives_concurrent_writers()
        {
            string dir = Path.Combine(Path.GetTempPath(), "hdrcast-group-" + Guid.NewGuid().ToString("N"));
            string path = Path.Combine(dir, GroupConfig.FileName);
            try
            {
                GroupConfig cfg = new GroupConfig().Normalize().Adopt(InviteCodec.Decode(Vectors.S("inviteCode")), "me");
                cfg.Save(path);
                GroupConfig back = GroupConfig.Load(path);
                Assert.Equal(cfg.GroupId, back.GroupId);
                Assert.Equal(cfg.PeerId, back.PeerId);
                Assert.True(back.DetectClipboardInvites);

                Parallel.For(0, 16, i =>
                {
                    try { cfg.Save(path); } catch (IOException) { } catch (UnauthorizedAccessException) { }
                });
                Assert.True(GroupConfig.Load(path).IsConfigured);
                Assert.Empty(Directory.GetFiles(dir, "*.tmp"));
            }
            finally
            {
                try { Directory.Delete(dir, true); } catch (IOException) { }
            }
        }

        [Fact]
        public void Display_names_are_cleaned_and_bounded()
        {
            Assert.Equal("abc", GroupConfig.CleanDisplayName(" ab\tc "));
            Assert.Equal(24, GroupConfig.CleanDisplayName(new string('x', 100)).Length);
            Assert.False(string.IsNullOrEmpty(GroupConfig.CleanDisplayName("   ")));
        }
    }
}
