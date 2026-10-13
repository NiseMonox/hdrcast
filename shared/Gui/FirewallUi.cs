using System;
using System.Drawing;
using System.Runtime.InteropServices;
using System.Threading;
using System.Windows.Forms;
using HdrCast.Common;

namespace HdrCast.Gui
{
    /// <summary>
    /// The "Firewall" row both apps show (docs/network.md §5): whether Windows Firewall lets friends' UDP in, and an
    /// Allow button that adds the rule (Windows asks for administrator permission once). The check runs in the
    /// background; <see cref="InboundAllowed"/> is the cached answer the P2P code reads.
    /// </summary>
    internal sealed class FirewallRow
    {
        public const string RuleDescription =
            "Lets friends in your HdrCast group connect even when their router changes ports. Added by HdrCast's Allow button; delete it to undo.";

        const int BCM_SETSHIELD = 0x160C;

        readonly Control _owner;
        readonly string _ruleName;
        readonly Action<string> _log;
        volatile FirewallVerdict _verdict = FirewallVerdict.Unknown;
        volatile bool _busy;
        bool _offered;

        public FirewallRow(Control owner, string ruleName, Action<string> log)
        {
            _owner = owner;
            _ruleName = ruleName;
            _log = log;
            int em = owner.Font.Height;
            Status = new Label { AutoSize = true, Margin = new Padding(3, 6, 6, 3), MaximumSize = new Size(em * 26, 0), Text = "Checking…" };
            AllowButton = new Button { Text = "Allow…", AutoSize = true, FlatStyle = FlatStyle.System, Visible = false };
            AllowButton.HandleCreated += (s, e) => SendMessage(AllowButton.Handle, BCM_SETSHIELD, IntPtr.Zero, (IntPtr)1);   // the UAC shield
            AllowButton.Click += (s, e) => AllowNow();
            Panel = new FlowLayoutPanel { AutoSize = true, WrapContents = false, Dock = DockStyle.Fill, Margin = new Padding(0) };
            Panel.Controls.Add(Status);
            Panel.Controls.Add(AllowButton);
        }

        public Label Status { get; }
        public Button AllowButton { get; }
        public FlowLayoutPanel Panel { get; }

        /// <summary>Raised on the UI thread after a check finishes.</summary>
        public event Action? Changed;

        public FirewallVerdict Verdict => _verdict;

        /// <summary>True: friends' packets get in (a rule, or no firewall); false: they do not; null: not known.</summary>
        public bool? InboundAllowed
        {
            get
            {
                FirewallVerdict v = _verdict;
                if (v == FirewallVerdict.Allowed || v == FirewallVerdict.Off) return true;
                return v == FirewallVerdict.Unknown ? (bool?)null : false;
            }
        }

        /// <summary>The Allow button can fix it (a missing or blocking rule).</summary>
        public bool CanFix => _verdict == FirewallVerdict.NotAllowed || _verdict == FirewallVerdict.Blocked;

        static string ExePath => Application.ExecutablePath;

        /// <summary>Checks again in the background (at start, and after going live, in case the settings changed).</summary>
        public void Refresh()
        {
            if (_busy) return;
            new Thread(() =>
            {
                FirewallVerdict v = WindowsFirewall.Check(ExePath);
                OnUi(() => Show(v));
            }) { IsBackground = true, Name = "firewall-check" }.Start();
        }

        /// <summary>
        /// Asks once (per app run) whether to add the rule, when it matters: going live / watching with a firewall that
        /// only lets replies in. <paramref name="why"/> says what the user is about to miss.
        /// </summary>
        public void OfferOnce(string why)
        {
            if (_offered || !CanFix || _busy) return;
            _offered = true;
            DialogResult r = MessageBox.Show(_owner,
                "Windows Firewall only lets in replies to what HdrCast sent, so " + why + "\n\n" +
                "Allow " + _ruleName + " through Windows Firewall now? Windows asks for administrator permission once. " +
                "(You can also do it later with Allow… next to Firewall.)",
                "HdrCast", MessageBoxButtons.YesNo, MessageBoxIcon.Question);
            if (r == DialogResult.Yes) AllowNow();
        }

        void AllowNow()
        {
            if (_busy) return;
            _busy = true;
            AllowButton.Enabled = false;
            Status.Text = "Waiting for Windows (confirm the prompt)…";
            string exe = ExePath;
            new Thread(() =>
            {
                string? error = WindowsFirewall.AllowElevated(exe);
                FirewallVerdict v = WindowsFirewall.Check(exe);
                OnUi(() =>
                {
                    _busy = false;
                    AllowButton.Enabled = true;
                    _log(error == null
                        ? "[app] Windows Firewall: friends' UDP may now reach " + exe
                        : "[app] Windows Firewall rule not added: " + error);
                    Show(v);
                });
            }) { IsBackground = true, Name = "firewall-allow" }.Start();
        }

        void Show(FirewallVerdict v)
        {
            _verdict = v;
            switch (v)
            {
                case FirewallVerdict.Allowed:
                    Set("Friends can reach this PC through Windows Firewall.", SystemColors.GrayText, false);
                    break;
                case FirewallVerdict.Off:
                    Set("Windows Firewall is off.", SystemColors.GrayText, false);
                    break;
                case FirewallVerdict.NotAllowed:
                    Set("Only replies get in: friends whose routers change ports cannot connect.", Color.DarkOrange, true);
                    break;
                case FirewallVerdict.Blocked:
                    Set("Windows Firewall blocks this program (a block rule, e.g. from Cancel on its prompt).", Color.Firebrick, true);
                    break;
                case FirewallVerdict.BlockAll:
                    Set("\"Block all incoming connections\" is on for this network (Windows Security > Firewall): no rule can help.", Color.Firebrick, false);
                    break;
                default:
                    Set("Could not read the Windows Firewall settings (another firewall program?).", SystemColors.GrayText, false);
                    break;
            }
            Changed?.Invoke();
        }

        void Set(string text, Color color, bool showButton)
        {
            Status.Text = text;
            Status.ForeColor = color;
            AllowButton.Visible = showButton;
        }

        void OnUi(Action a)
        {
            if (_owner.IsDisposed || !_owner.IsHandleCreated) return;
            try { _owner.BeginInvoke(a); } catch (Exception) { }
        }

        [DllImport("user32.dll")]
        static extern IntPtr SendMessage(IntPtr hWnd, int msg, IntPtr wParam, IntPtr lParam);
    }
}
