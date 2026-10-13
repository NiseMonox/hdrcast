using System;
using System.Collections.Generic;
using System.Threading;
using System.Windows.Forms;
using HdrCast.Capture;
using HdrCast.Common;
using HdrCast.Sender.Capture;

namespace HdrCast.Sender
{
    /// <summary>
    /// One executable, three modes (design.md §2.1):
    ///   HdrCastSender.exe                     → the GUI
    ///   HdrCastSender.exe list-outputs [--json]
    ///   HdrCastSender.exe capture &lt;options&gt;    → the capture engine driven by the GUI (or by hand)
    /// </summary>
    internal static class Program
    {
        [STAThread]
        static int Main(string[] args)
        {
            if (args.Length == 0) return RunGui(null);
            // "HdrCastSender.exe hdrcast:…" (e.g. from a shortcut or a script): open the GUI and offer to join.
            if (args[0].StartsWith("hdrcast:", StringComparison.OrdinalIgnoreCase)) return RunGui(string.Join(" ", args));

            string verb = args[0].ToLowerInvariant();
            var rest = new string[args.Length - 1];
            Array.Copy(args, 1, rest, 0, rest.Length);

            // The GUI's Allow… starts this elevated (Windows shows its UAC prompt): add the rule, no window, exit.
            if (verb == WindowsFirewall.AllowVerb) return AllowThroughFirewall();

            NativeMethods.EnsureConsole();
            switch (verb)
            {
                case "list-outputs": return ListOutputs(rest);
                case "audio-devices": return ListAudioDevices();
                case "capture": return RunCapture(rest);
                case "--help":
                case "-h":
                case "/?":
                case "help":
                    PrintUsage();
                    return ExitCodes.Ok;
                default:
                    Console.Error.WriteLine("unknown command: " + args[0]);
                    PrintUsage();
                    return ExitCodes.BadArguments;
            }
        }

        static int RunGui(string? invite)
        {
            Application.EnableVisualStyles();
            Application.SetCompatibleTextRenderingDefault(false);
            Application.SetUnhandledExceptionMode(UnhandledExceptionMode.CatchException);
            Application.ThreadException += (s, e) => ReportCrash(e.Exception);
            AppDomain.CurrentDomain.UnhandledException +=
                (s, e) => SenderLog.Write("crash", (e.ExceptionObject ?? "?").ToString() ?? "?");

            SenderLog.Write("app", "HdrCast Sender " + typeof(Program).Assembly.GetName().Version + " starting");
            Application.Run(new Gui.MainForm(invite));
            SenderLog.Write("app", "exit");
            return ExitCodes.Ok;
        }

        static int AllowThroughFirewall()
        {
            string exe = Application.ExecutablePath;
            string? error = WindowsFirewall.AllowProgram(exe, "HdrCast Sender", HdrCast.Gui.FirewallRow.RuleDescription);
            SenderLog.Write("app", error == null ? "firewall: inbound UDP allowed for " + exe : "firewall: could not add the rule for " + exe + ": " + error);
            return error == null ? ExitCodes.Ok : ExitCodes.DeviceError;
        }

        static void ReportCrash(Exception ex)
        {
            SenderLog.Write("crash", ex.ToString());
            try
            {
                MessageBox.Show("Unexpected error:\n\n" + ex.Message + "\n\nDetails were written to the log folder.",
                    "HdrCast Sender", MessageBoxButtons.OK, MessageBoxIcon.Error);
            }
            catch (Exception)
            {
                // nothing else we can do
            }
        }

        static void PrintUsage()
        {
            Console.Out.WriteLine("HdrCast sender " + typeof(Program).Assembly.GetName().Version);
            Console.Out.WriteLine("  HdrCastSender.exe                       start the GUI");
            Console.Out.WriteLine("  HdrCastSender.exe list-outputs [--json] list the DXGI outputs (index, HDR state)");
            Console.Out.WriteLine("  HdrCastSender.exe audio-devices         list the WASAPI render endpoints");
            Console.Out.WriteLine();
            Console.Out.Write(CaptureArgs.Usage);
        }

        static int ListOutputs(string[] args)
        {
            bool json = false;
            foreach (string a in args)
            {
                if (a == "--json") json = true;
                else { Console.Error.WriteLine("unknown option: " + a); return ExitCodes.BadArguments; }
            }
            try
            {
                List<OutputInfo> outputs = OutputEnumerator.Enumerate();
                Console.Out.Write(json ? OutputInfo.ToJson(outputs) : OutputInfo.ToTable(outputs));
                if (json) Console.Out.WriteLine();
                return ExitCodes.Ok;
            }
            catch (Exception ex)
            {
                Console.Error.WriteLine("list-outputs failed: " + ex.Message);
                return ExitCodes.DeviceError;
            }
        }

        static int ListAudioDevices()
        {
            foreach (string line in AudioCapture.ListDevices()) Console.Out.WriteLine(line);
            return ExitCodes.Ok;
        }

        static int RunCapture(string[] args)
        {
            CaptureArgs parsed;
            try
            {
                parsed = CaptureArgs.Parse(args);
            }
            catch (CaptureArgsException ex)
            {
                Console.Error.WriteLine(ex.Message);
                Console.Error.Write(CaptureArgs.Usage);
                return ExitCodes.BadArguments;
            }

            if (parsed.SelfTest) return SelfTest.Run(parsed.SdrWhite);

            using (var engine = new CaptureEngine(parsed))
            {
                // Ctrl+C / console close / the GUI killing the process tree: stop cleanly so the pipe is flushed.
                NativeMethods.ConsoleCtrlHandler handler = type =>
                {
                    engine.RequestStop();
                    Thread.Sleep(300);
                    return true;
                };
                NativeMethods.SetConsoleCtrlHandler(handler, true);
                Console.CancelKeyPress += (s, e) => { e.Cancel = true; engine.RequestStop(); };
                try
                {
                    return engine.Run();
                }
                catch (Exception ex)
                {
                    Log.Error("capture: unhandled error: " + ex);
                    return ExitCodes.DeviceError;
                }
                finally
                {
                    GC.KeepAlive(handler);
                    NativeMethods.SetConsoleCtrlHandler(null, false);
                }
            }
        }
    }
}
