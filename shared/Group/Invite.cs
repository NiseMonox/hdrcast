using System;
using System.Collections.Generic;
using System.Text;
using HdrCast.Common;

namespace HdrCast.Group
{
    /// <summary>What an invite carries (docs/design.md §8.2): where to meet, which group, and the group key.</summary>
    public sealed class Invite
    {
        public const int CurrentVersion = 1;
        public const int MaxNameLength = 40;
        /// <summary>The signaling server new groups use unless told otherwise (metadata only, never video).</summary>
        public const string DefaultSignalUrl = "http://101.43.23.94:18650";

        public Invite(string signalUrl, string groupId, byte[] groupKey, string groupName)
        {
            string? problem = Problem(signalUrl, groupId, groupKey, groupName);
            if (problem != null) throw new ArgumentException(problem);
            SignalUrl = NormalizeUrl(signalUrl);
            GroupId = groupId;
            GroupKey = (byte[])groupKey.Clone();
            GroupName = groupName.Trim();
        }

        public string SignalUrl { get; }
        public string GroupId { get; }
        public byte[] GroupKey { get; }
        public string GroupName { get; }

        /// <summary>Stable short tag for "the user already saw / dismissed this invite" (not secret-bearing).</summary>
        public string Fingerprint =>
            Base64Url.Encode(GroupKeys.Sha256(Encoding.UTF8.GetBytes(GroupId + "\n" + Base64Url.Encode(GroupKey)))).Substring(0, 12);

        /// <summary>A brand-new group: random id and key.</summary>
        public static Invite Create(string groupName, string? signalUrl = null) =>
            new Invite(string.IsNullOrWhiteSpace(signalUrl) ? DefaultSignalUrl : signalUrl!, GroupKeys.NewId(),
                       GroupKeys.RandomBytes(GroupKeys.KeyLength), groupName);

        /// <summary>Null when the parts form a valid invite, otherwise a sentence for the user.</summary>
        public static string? Problem(string? signalUrl, string? groupId, byte[]? groupKey, string? groupName)
        {
            string url = NormalizeUrl(signalUrl ?? "");
            if (!(url.StartsWith("http://", StringComparison.OrdinalIgnoreCase) ||
                  url.StartsWith("https://", StringComparison.OrdinalIgnoreCase)) || url.IndexOf(' ') >= 0 ||
                !Uri.TryCreate(url, UriKind.Absolute, out Uri? _))
                return "the signaling address must be an http:// or https:// URL";
            if (!GroupKeys.IsValidId(groupId)) return "the group id is not valid";
            if (groupKey == null || groupKey.Length != GroupKeys.KeyLength) return "the group key must be 32 bytes";
            string name = (groupName ?? "").Trim();
            if (name.Length == 0 || name.Length > MaxNameLength) return "the group name must be 1-40 characters";
            return null;
        }

        static string NormalizeUrl(string url) => url.Trim().TrimEnd('/');
    }

    public enum InviteError
    {
        None = 0,
        /// <summary>No "hdrcast:" in the text.</summary>
        NotFound = 1,
        /// <summary>Cut off: no checksum or a short one.</summary>
        Incomplete = 2,
        /// <summary>Checksum mismatch or undecodable.</summary>
        Damaged = 3,
        /// <summary>v &gt; 1: made by a newer HdrCast.</summary>
        NewerVersion = 4,
        /// <summary>Decodes, but a field is wrong.</summary>
        Invalid = 5,
    }

    /// <summary>
    /// hdrcast:&lt;b64u(JSON)&gt;.&lt;first 8 chars of b64u(SHA-256(JSON))&gt; — pasted whole from a chat message.
    /// The extractor forgives what chat apps do to text: full-width characters, inserted line breaks and
    /// zero-width characters, surrounding words.
    /// </summary>
    public static class InviteCodec
    {
        public const string Prefix = "hdrcast:";
        public const int ChecksumLength = 8;

        public static string Encode(Invite invite)
        {
            if (invite == null) throw new ArgumentNullException(nameof(invite));
            var obj = new Dictionary<string, object?>
            {
                ["v"] = Invite.CurrentVersion,
                ["s"] = invite.SignalUrl,
                ["g"] = invite.GroupId,
                ["k"] = Base64Url.Encode(invite.GroupKey),
                ["n"] = invite.GroupName,
            };
            byte[] json = Encoding.UTF8.GetBytes(MiniJson.Serialize(obj));
            return Prefix + Base64Url.Encode(json) + "." + Checksum(json);
        }

        /// <summary>Finds and decodes the first valid invite in <paramref name="text"/>.</summary>
        public static bool TryExtract(string? text, out Invite? invite, out InviteError error)
        {
            invite = null;
            error = InviteError.NotFound;
            if (string.IsNullOrEmpty(text)) return false;
            // Zero-width characters go everywhere first: some chat apps put one inside "hdrcast:" to stop auto-linking.
            var clean = new StringBuilder(text!.Length);
            foreach (char c in text)
                if (!IsZeroWidth(c)) clean.Append(c);
            string s = clean.ToString().Normalize(NormalizationForm.FormKC);

            int from = 0;
            while (true)
            {
                int at = s.IndexOf(Prefix, from, StringComparison.OrdinalIgnoreCase);
                if (at < 0) return false;
                from = at + Prefix.Length;
                InviteError e = TryDecodeAt(s, from, out Invite? found);
                if (e == InviteError.None)
                {
                    invite = found;
                    error = InviteError.None;
                    return true;
                }
                error = e;
            }
        }

        /// <summary>Decodes a code (or any text containing one); throws <see cref="FormatException"/> with <see cref="Describe"/>.</summary>
        public static Invite Decode(string text)
        {
            if (!TryExtract(text, out Invite? invite, out InviteError error)) throw new FormatException(Describe(error));
            return invite!;
        }

        /// <summary>A sentence for the user.</summary>
        public static string Describe(InviteError error)
        {
            switch (error)
            {
                case InviteError.None: return "OK";
                case InviteError.NotFound: return "No HdrCast invite found (it starts with \"hdrcast:\").";
                case InviteError.Incomplete: return "The invite is cut off. Copy the whole message again.";
                case InviteError.Damaged: return "The invite is damaged (checksum mismatch). Ask for it again, or copy the whole message.";
                case InviteError.NewerVersion: return "This invite comes from a newer HdrCast. Update the app.";
                case InviteError.Invalid: return "This invite is not valid.";
                default: return "Unknown invite problem.";
            }
        }

        static InviteError TryDecodeAt(string s, int start, out Invite? invite)
        {
            invite = null;
            // Collect base64url characters and the '.', skipping whitespace / zero-width characters that chat apps
            // insert; stop at anything else. The payload never contains '.', so the first '.' ends it and the next
            // 8 characters are the checksum (anything after that belongs to the surrounding message).
            var payload = new StringBuilder();
            var checksum = new StringBuilder();
            bool inChecksum = false;
            for (int i = start; i < s.Length; i++)
            {
                char c = s[i];
                if (IsSkippable(c)) continue;
                if (c == '.')
                {
                    if (inChecksum) break;
                    inChecksum = true;
                    continue;
                }
                if (!Base64Url.IsAlphabet(c)) break;
                if (inChecksum)
                {
                    checksum.Append(c);
                    if (checksum.Length == ChecksumLength) break;
                }
                else
                {
                    payload.Append(c);
                }
            }

            if (payload.Length == 0 || !inChecksum || checksum.Length < ChecksumLength) return InviteError.Incomplete;
            if (!Base64Url.TryDecode(payload.ToString(), out byte[] json)) return InviteError.Damaged;
            if (!string.Equals(Checksum(json), checksum.ToString(), StringComparison.Ordinal)) return InviteError.Damaged;

            object? root;
            try
            {
                root = MiniJson.Parse(Encoding.UTF8.GetString(json));
            }
            catch (FormatException)
            {
                return InviteError.Invalid;
            }
            if (MiniJson.AsObject(root) == null) return InviteError.Invalid;

            long? v = MiniJson.AsLong(MiniJson.Get(root, "v"));
            if (v == null) return InviteError.Invalid;
            if (v.Value > Invite.CurrentVersion) return InviteError.NewerVersion;
            if (v.Value != Invite.CurrentVersion) return InviteError.Invalid;

            string? url = MiniJson.GetString(root, "s");
            string? gid = MiniJson.GetString(root, "g");
            string? key = MiniJson.GetString(root, "k");
            string? name = MiniJson.GetString(root, "n");
            if (key == null || !Base64Url.TryDecode(key, out byte[] groupKey)) return InviteError.Invalid;
            if (Invite.Problem(url, gid, groupKey, name) != null) return InviteError.Invalid;
            invite = new Invite(url!, gid!, groupKey, name!);
            return InviteError.None;
        }

        static string Checksum(byte[] json) => Base64Url.Encode(GroupKeys.Sha256(json)).Substring(0, ChecksumLength);

        static bool IsSkippable(char c) => c == ' ' || c == '\t' || c == '\r' || c == '\n' || c == '\u00A0' || IsZeroWidth(c);

        static bool IsZeroWidth(char c) => c == '\u200B' || c == '\u200C' || c == '\u200D' || c == '\u2060' || c == '\uFEFF';
    }
}
