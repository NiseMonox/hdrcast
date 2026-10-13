using System;
using System.Threading;
using System.Windows.Forms;
using HdrCast.Common;
using HdrCast.Receiver.Gui;

namespace HdrCast.Receiver
{
    internal static class Program
    {
        [STAThread]
        static int Main(string[] args)
        {
            // The GUI's Allow… starts this elevated (Windows shows its UAC prompt): add the rule, no window, exit.
            if (args.Length == 1 && args[0] == WindowsFirewall.AllowVerb)
            {
                string exe = Application.ExecutablePath;
                string? error = WindowsFirewall.AllowProgram(exe, "HdrCast Receiver", HdrCast.Gui.FirewallRow.RuleDescription);
                ReceiverLog.Write("app", error == null ? "firewall: inbound UDP allowed for " + exe : "firewall: could not add the rule for " + exe + ": " + error);
                return error == null ? 0 : 1;
            }

            Application.EnableVisualStyles();
            Application.SetCompatibleTextRenderingDefault(false);
            Application.SetUnhandledExceptionMode(UnhandledExceptionMode.CatchException);
            Application.ThreadException += (s, e) => ReportCrash(e.Exception);
            AppDomain.CurrentDomain.UnhandledException += (s, e) => ReceiverLog.Write("crash", (e.ExceptionObject ?? "?").ToString() ?? "?");

            ReceiverLog.Write("app", "HdrCast Receiver " + typeof(Program).Assembly.GetName().Version + " starting");
            // "HdrCastReceiver.exe hdrcast:…" (a shortcut or a script): open and offer to join that group.
            string? invite = args.Length > 0 && args[0].StartsWith("hdrcast:", StringComparison.OrdinalIgnoreCase) ? string.Join(" ", args) : null;
            Application.Run(new MainForm(invite));
            ReceiverLog.Write("app", "exit");
            return 0;
        }

        private static void ReportCrash(Exception ex)
        {
            ReceiverLog.Write("crash", ex.ToString());
            try
            {
                MessageBox.Show("Unexpected error:\n\n" + ex.Message + "\n\nDetails were written to the log folder.",
                    "HdrCast Receiver", MessageBoxButtons.OK, MessageBoxIcon.Error);
            }
            catch (Exception)
            {
                // nothing else we can do
            }
        }
    }
}
