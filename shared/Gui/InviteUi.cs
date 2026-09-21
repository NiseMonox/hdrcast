using System;
using System.Drawing;
using System.IO;
using System.Runtime.InteropServices;
using System.Windows.Forms;
using HdrCast.Common;
using HdrCast.Group;

namespace HdrCast.Gui
{
    /// <summary>
    /// Group and invite dialogs shared by both apps (WinForms, code-only). Everything here edits the shared
    /// %APPDATA%\HdrCast\group.json, so a group created or joined in one app is there in the other.
    /// </summary>
    internal static class InviteUi
    {
        public const string Title = "HdrCast";

        /// <summary>The message people paste into WeChat / LINE / Discord: a line of explanation plus the code.</summary>
        public static string InviteMessage(Invite invite) =>
            "HdrCast invite to \"" + invite.GroupName + "\". Copy this whole message, then open HdrCastReceiver " +
            "(to watch) or HdrCastSender (to stream) and it will offer to join. Keep it among friends: anyone with it can watch.\r\n" +
            InviteCodec.Encode(invite);

        public static void CopyInvite(IWin32Window owner, GroupConfig group)
        {
            if (!group.TryGetInvite(out Invite? invite))
            {
                MessageBox.Show(owner, "There is no group yet. Create one, or paste an invite from a friend.", Title,
                                MessageBoxButtons.OK, MessageBoxIcon.Information);
                return;
            }
            try
            {
                Clipboard.SetText(InviteMessage(invite!));
                MessageBox.Show(owner, "The invite is on the clipboard. Paste it to your friends in a chat.\n\n" +
                                       "Anyone who has it can join the group, so only send it to people you trust.",
                                Title, MessageBoxButtons.OK, MessageBoxIcon.Information);
            }
            catch (Exception ex)
            {
                MessageBox.Show(owner, "Could not use the clipboard: " + ex.Message, Title, MessageBoxButtons.OK, MessageBoxIcon.Warning);
            }
        }

        /// <summary>Asks for a group name and the user's display name; returns the new config (not saved yet) or null.</summary>
        public static GroupConfig? CreateGroup(IWin32Window owner, GroupConfig current)
        {
            if (current.IsConfigured &&
                MessageBox.Show(owner, "This PC is already in the group \"" + current.GroupName + "\".\n\n" +
                                       "Create a new group anyway? Friends will need the new invite; the old one stops working for you.",
                                Title, MessageBoxButtons.YesNo, MessageBoxIcon.Question) != DialogResult.Yes)
                return null;

            string? groupName = PromptText(owner, "Create a group", "Group name (what your friends will see):",
                                           current.IsConfigured ? current.GroupName : "Friends", Invite.MaxNameLength);
            if (groupName == null) return null;
            string? me = PromptText(owner, "Create a group", "Your name in the group:", DefaultName(current), GroupConfig.MaxDisplayNameLength);
            if (me == null) return null;
            try
            {
                return current.Adopt(Invite.Create(groupName), me);
            }
            catch (ArgumentException ex)
            {
                MessageBox.Show(owner, ex.Message, Title, MessageBoxButtons.OK, MessageBoxIcon.Warning);
                return null;
            }
        }

        /// <summary>"Paste invite": from the clipboard, or from a box the user pastes into. Returns the new config or null.</summary>
        public static GroupConfig? PasteInvite(IWin32Window owner, GroupConfig current)
        {
            string text = ReadClipboardText();
            if (!InviteCodec.TryExtract(text, out _, out _))
            {
                text = PromptText(owner, "Paste invite", "Paste the whole invite message here:", "", 4000, multiline: true) ?? "";
                if (text.Length == 0) return null;
            }
            return Import(owner, current, text);
        }

        /// <summary>Decodes, confirms a group switch, asks for the display name. Null when cancelled or invalid.</summary>
        public static GroupConfig? Import(IWin32Window owner, GroupConfig current, string text)
        {
            if (!InviteCodec.TryExtract(text, out Invite? invite, out InviteError error))
            {
                MessageBox.Show(owner, InviteCodec.Describe(error), Title, MessageBoxButtons.OK, MessageBoxIcon.Warning);
                return null;
            }
            if (current.TryGetInvite(out Invite? mine) && mine!.GroupId == invite!.GroupId &&
                Convert.ToBase64String(mine.GroupKey) == Convert.ToBase64String(invite.GroupKey))
            {
                MessageBox.Show(owner, "This PC is already in \"" + invite.GroupName + "\".", Title, MessageBoxButtons.OK, MessageBoxIcon.Information);
                return null;
            }
            if (current.IsConfigured &&
                MessageBox.Show(owner, "Leave \"" + current.GroupName + "\" and join \"" + invite!.GroupName + "\"?", Title,
                                MessageBoxButtons.YesNo, MessageBoxIcon.Question) != DialogResult.Yes)
                return null;
            string? me = PromptText(owner, "Join \"" + invite!.GroupName + "\"", "Your name in the group:", DefaultName(current),
                                    GroupConfig.MaxDisplayNameLength);
            if (me == null) return null;
            return current.Adopt(invite, me);
        }

        static string DefaultName(GroupConfig current) =>
            string.IsNullOrWhiteSpace(current.DisplayName) ? GroupConfig.CleanDisplayName(Environment.UserName) : current.DisplayName;

        public static string ReadClipboardText()
        {
            try
            {
                if (!Clipboard.ContainsText()) return "";
                string t = Clipboard.GetText();
                return t.Length > 65536 ? t.Substring(0, 65536) : t;
            }
            catch (Exception)
            {
                return "";      // another app (often a chat client) holds the clipboard
            }
        }

        /// <summary>A small modal text prompt; null when cancelled.</summary>
        public static string? PromptText(IWin32Window owner, string title, string prompt, string initial, int maxLength, bool multiline = false)
        {
            using (var f = new Form())
            {
                f.Text = title;
                f.FormBorderStyle = FormBorderStyle.FixedDialog;
                f.MinimizeBox = f.MaximizeBox = false;
                f.ShowInTaskbar = false;
                f.StartPosition = FormStartPosition.CenterParent;
                f.AutoScaleMode = AutoScaleMode.Dpi;
                f.Font = SystemFonts.MessageBoxFont ?? f.Font;
                int em = f.Font.Height;
                f.ClientSize = new Size(em * 30, multiline ? em * 14 : em * 7);

                var label = new Label { Text = prompt, AutoSize = true, Location = new Point(em, em) };
                var box = new TextBox
                {
                    Text = initial, MaxLength = maxLength, Multiline = multiline, ScrollBars = multiline ? ScrollBars.Vertical : ScrollBars.None,
                    Location = new Point(em, em * 2 + em / 2), Width = em * 28, Height = multiline ? em * 8 : em,
                    Anchor = AnchorStyles.Left | AnchorStyles.Right | AnchorStyles.Top,
                };
                var ok = new Button { Text = "OK", DialogResult = DialogResult.OK, AutoSize = true };
                var cancel = new Button { Text = "Cancel", DialogResult = DialogResult.Cancel, AutoSize = true };
                ok.Location = new Point(f.ClientSize.Width - em * 12, f.ClientSize.Height - em * 2 - em / 2);
                cancel.Location = new Point(f.ClientSize.Width - em * 6, f.ClientSize.Height - em * 2 - em / 2);
                f.Controls.AddRange(new Control[] { label, box, ok, cancel });
                if (!multiline) f.AcceptButton = ok;
                f.CancelButton = cancel;
                if (f.ShowDialog(owner) != DialogResult.OK) return null;
                string text = box.Text.Trim();
                return text.Length == 0 ? null : text;
            }
        }
    }

    /// <summary>
    /// Offers to join when the clipboard holds an invite and a window is activated (the friend just copied the
    /// message from a chat and switched to HdrCast). Reads the clipboard only when its sequence number changed,
    /// asks once per invite, and can be switched off in group.json ("detectClipboard").
    /// </summary>
    internal sealed class ClipboardInviteWatcher
    {
        [DllImport("user32.dll")]
        static extern uint GetClipboardSequenceNumber();

        readonly Form _form;
        readonly Func<GroupConfig> _getGroup;
        readonly Action<GroupConfig> _save;
        uint _lastSequence;
        bool _busy;

        public ClipboardInviteWatcher(Form form, Func<GroupConfig> getGroup, Action<GroupConfig> save)
        {
            _form = form;
            _getGroup = getGroup;
            _save = save;
            // _lastSequence starts at 0, so an invite copied before the app was opened is offered on first activation.
            form.Activated += (s, e) => Check();
        }

        public void Check()
        {
            if (_busy) return;
            GroupConfig group = _getGroup();
            if (!group.DetectClipboardInvites) return;
            uint seq;
            try { seq = GetClipboardSequenceNumber(); } catch (Exception) { return; }
            if (seq == _lastSequence) return;
            string text = InviteUi.ReadClipboardText();
            _lastSequence = seq;
            if (!InviteCodec.TryExtract(text, out Invite? invite, out _)) return;
            if (group.TryGetInvite(out Invite? mine) && mine!.GroupId == invite!.GroupId) return;
            if (invite!.Fingerprint == group.DismissedFingerprint) return;

            _busy = true;
            _form.BeginInvoke(new Action(() =>
            {
                try
                {
                    DialogResult r = MessageBox.Show(_form,
                        "The clipboard holds an HdrCast invite to \"" + invite.GroupName + "\".\n\nJoin this group?",
                        InviteUi.Title, MessageBoxButtons.YesNo, MessageBoxIcon.Question);
                    GroupConfig current = _getGroup();
                    if (r == DialogResult.Yes)
                    {
                        GroupConfig? next = InviteUi.Import(_form, current, text);
                        if (next != null) _save(next);
                    }
                    else
                    {
                        current.DismissedFingerprint = invite.Fingerprint;
                        _save(current);
                    }
                }
                finally
                {
                    _busy = false;
                }
            }));
        }
    }

    /// <summary>Loads group.json again when the other app changed it (checked cheaply by write time).</summary>
    internal sealed class SharedGroupFile
    {
        readonly string _path;
        DateTime _stamp = DateTime.MinValue;

        public SharedGroupFile(string path)
        {
            _path = path;
            Current = Reload();
        }

        public GroupConfig Current { get; private set; }

        public string Path => _path;

        /// <summary>True when the file changed on disk since the last load (and <see cref="Current"/> was refreshed).</summary>
        public bool RefreshIfChanged()
        {
            DateTime stamp = Stamp();
            if (stamp == _stamp) return false;
            Current = Reload();
            return true;
        }

        public void Save(GroupConfig config)
        {
            AppPaths.EnsureDir(System.IO.Path.GetDirectoryName(_path) ?? AppPaths.ConfigDir);
            config.Normalize().Save(_path);
            Current = config;
            _stamp = Stamp();
        }

        GroupConfig Reload()
        {
            _stamp = Stamp();
            return GroupConfig.Load(_path);
        }

        DateTime Stamp()
        {
            try { return File.Exists(_path) ? File.GetLastWriteTimeUtc(_path) : DateTime.MinValue; }
            catch (Exception) { return DateTime.MinValue; }
        }
    }
}
