using System;
using System.Runtime.Serialization;
using HdrCast.Common;

namespace HdrCast.Group
{
    /// <summary>
    /// The group this PC belongs to, shared by both apps in %APPDATA%\HdrCast\group.json: pasting the invite
    /// into either app configures both. Written only when an invite is imported or a group is created;
    /// holds the group key, so treat the file like a password.
    /// </summary>
    [DataContract]
    public sealed class GroupConfig
    {
        public const string FileName = "group.json";
        public const int MaxDisplayNameLength = 24;

        [DataMember(Name = "v", Order = 0)] public int Version { get; set; }
        [DataMember(Name = "signal", Order = 1)] public string SignalUrl { get; set; } = "";
        [DataMember(Name = "gid", Order = 2)] public string GroupId { get; set; } = "";
        [DataMember(Name = "key", Order = 3)] public string GroupKey { get; set; } = "";
        [DataMember(Name = "name", Order = 4)] public string GroupName { get; set; } = "";
        /// <summary>How friends see this person in the live list and the viewer list.</summary>
        [DataMember(Name = "me", Order = 5)] public string DisplayName { get; set; } = "";
        /// <summary>This install's id: the host id while streaming, the viewer id while watching.</summary>
        [DataMember(Name = "peer", Order = 6)] public string PeerId { get; set; } = "";
        [DataMember(Name = "importedAt", Order = 7)] public long ImportedAtMs { get; set; }
        /// <summary>Offer to import an invite found on the clipboard when a window is activated.</summary>
        [DataMember(Name = "detectClipboard", Order = 8)] public bool DetectClipboardInvites { get; set; } = true;
        /// <summary>Fingerprint of the last clipboard invite the user said no to (asked once, not every time).</summary>
        [DataMember(Name = "dismissed", Order = 9)] public string DismissedFingerprint { get; set; } = "";

        [OnDeserializing]
        void OnDeserializing(StreamingContext _)
        {
            SignalUrl = GroupId = GroupKey = GroupName = DisplayName = PeerId = DismissedFingerprint = "";
            DetectClipboardInvites = true;
        }

        /// <summary>True when this describes a usable group.</summary>
        public bool IsConfigured => TryGetInvite(out _);

        public bool TryGetInvite(out Invite? invite)
        {
            invite = null;
            if (!Base64Url.TryDecode(GroupKey, out byte[] key)) return false;
            if (Invite.Problem(SignalUrl, GroupId, key, GroupName) != null) return false;
            invite = new Invite(SignalUrl, GroupId, key, GroupName);
            return true;
        }

        public Invite ToInvite()
        {
            if (!TryGetInvite(out Invite? invite)) throw new InvalidOperationException("no group is configured");
            return invite!;
        }

        public GroupKeys Keys() => new GroupKeys(ToInvite().GroupKey);

        /// <summary>Joins <paramref name="invite"/>'s group, keeping this install's peer id and the user's preferences.</summary>
        public GroupConfig Adopt(Invite invite, string displayName)
        {
            var next = new GroupConfig
            {
                Version = 1,
                SignalUrl = invite.SignalUrl,
                GroupId = invite.GroupId,
                GroupKey = Base64Url.Encode(invite.GroupKey),
                GroupName = invite.GroupName,
                DisplayName = CleanDisplayName(displayName),
                PeerId = GroupKeys.IsValidId(PeerId) ? PeerId : GroupKeys.NewId(),
                ImportedAtMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(),
                DetectClipboardInvites = DetectClipboardInvites,
                DismissedFingerprint = DismissedFingerprint,
            };
            return next;
        }

        /// <summary>Trimmed, control characters removed, at most 24 characters; falls back to the Windows user name.</summary>
        public static string CleanDisplayName(string? name)
        {
            var sb = new System.Text.StringBuilder();
            foreach (char c in (name ?? "").Trim())
                if (!char.IsControl(c)) sb.Append(c);
            string s = sb.ToString().Trim();
            if (s.Length == 0) s = Environment.UserName ?? "friend";
            if (s.Length > MaxDisplayNameLength) s = s.Substring(0, MaxDisplayNameLength);
            return s;
        }

        /// <summary>Makes sure a peer id exists (older files, or a file written by hand).</summary>
        public GroupConfig Normalize()
        {
            SignalUrl ??= "";
            GroupId ??= "";
            GroupKey ??= "";
            GroupName ??= "";
            DismissedFingerprint ??= "";
            DisplayName = CleanDisplayName(DisplayName);
            if (!GroupKeys.IsValidId(PeerId)) PeerId = GroupKeys.NewId();
            return this;
        }

        public static string DefaultPath => AppPaths.SettingsFile(FileName);

        public static GroupConfig Load(string path) => (Json.LoadFile<GroupConfig>(path) ?? new GroupConfig()).Normalize();

        public void Save(string path) => Json.SaveFile(path, this);
    }
}
