using System;
using System.Threading;
using System.Windows.Forms;
using HdrCast.Receiver.Gui;

namespace HdrCast.Receiver
{
    internal static class Program
    {
        [STAThread]
        static void Main()
        {
            Application.EnableVisualStyles();
            Application.SetCompatibleTextRenderingDefault(false);
            Application.SetUnhandledExceptionMode(UnhandledExceptionMode.CatchException);
            Application.ThreadException += (s, e) => ReportCrash(e.Exception);
            AppDomain.CurrentDomain.UnhandledException += (s, e) => ReceiverLog.Write("crash", (e.ExceptionObject ?? "?").ToString() ?? "?");

            ReceiverLog.Write("app", "HdrCast Receiver " + typeof(Program).Assembly.GetName().Version + " starting");
            Application.Run(new MainForm());
            ReceiverLog.Write("app", "exit");
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
