using System;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Threading;

namespace HdrCast.Common
{
    /// <summary>
    /// A child process with redirected stdout/stderr delivered as line events on thread-pool threads,
    /// an Exited event that fires after the output streams drained, and a Stop() that kills the whole
    /// process tree. Stdout can alternatively be exposed as a raw stream (ffmpeg | mpv piping) and
    /// stdin can be redirected.
    /// </summary>
    public sealed class LineProcess : IDisposable
    {
        private Process? _process;
        private int _exitRaised;
        private int _disposed;

        public LineProcess(string fileName, string arguments)
        {
            FileName = fileName ?? throw new ArgumentNullException(nameof(fileName));
            Arguments = arguments ?? "";
        }

        public string FileName { get; }
        public string Arguments { get; }
        public string? WorkingDirectory { get; set; }

        /// <summary>true (default): stdout is read line by line into <see cref="StdoutLine"/>;
        /// false: stdout is available as <see cref="StdoutStream"/> after Start().</summary>
        public bool ReadStdoutLines { get; set; } = true;

        /// <summary>When true, <see cref="StdinStream"/> is available after Start().</summary>
        public bool RedirectStdin { get; set; }

        public event Action<string>? StdoutLine;
        public event Action<string>? StderrLine;
        /// <summary>Exit code; raised once, after stdout/stderr have been drained.</summary>
        public event Action<int>? Exited;

        public int Id { get; private set; } = -1;
        public int ExitCode { get; private set; } = -1;
        public bool HasStarted => _process != null;
        public bool HasExited
        {
            get
            {
                var p = _process;
                if (p == null) return false;
                try { return p.HasExited; } catch (Exception) { return true; }
            }
        }
        public Stream? StdoutStream { get; private set; }
        public Stream? StdinStream { get; private set; }

        public string CommandLine => ArgQuote.Quote(FileName) + (Arguments.Length > 0 ? " " + Arguments : "");

        public void Start()
        {
            if (_process != null) throw new InvalidOperationException("already started");
            var psi = new ProcessStartInfo
            {
                FileName = FileName,
                Arguments = Arguments,
                UseShellExecute = false,
                CreateNoWindow = true,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
                RedirectStandardInput = RedirectStdin,
                StandardErrorEncoding = new UTF8Encoding(false),
            };
            if (ReadStdoutLines) psi.StandardOutputEncoding = new UTF8Encoding(false);
            if (!string.IsNullOrEmpty(WorkingDirectory)) psi.WorkingDirectory = WorkingDirectory;

            var p = new Process { StartInfo = psi, EnableRaisingEvents = true };
            p.ErrorDataReceived += (s, e) => { if (e.Data != null) StderrLine?.Invoke(e.Data); };
            if (ReadStdoutLines) p.OutputDataReceived += (s, e) => { if (e.Data != null) StdoutLine?.Invoke(e.Data); };
            p.Exited += (s, e) => RaiseExited(p);

            _process = p;
            p.Start();
            Id = p.Id;
            p.BeginErrorReadLine();
            if (ReadStdoutLines) p.BeginOutputReadLine();
            else StdoutStream = p.StandardOutput.BaseStream;
            if (RedirectStdin) StdinStream = p.StandardInput.BaseStream;
        }

        private void RaiseExited(Process p)
        {
            if (Interlocked.Exchange(ref _exitRaised, 1) != 0) return;
            int code = -1;
            try
            {
                // WaitForExit() without a timeout also waits for the async line readers to hit EOF.
                p.WaitForExit();
                code = p.ExitCode;
            }
            catch (Exception)
            {
                // process handle already gone
            }
            ExitCode = code;
            Exited?.Invoke(code);
        }

        public bool WaitForExit(int milliseconds)
        {
            var p = _process;
            if (p == null) return true;
            try { return p.WaitForExit(milliseconds); } catch (Exception) { return true; }
        }

        /// <summary>Kills the process tree (no-op when not running). Waits up to 3 s for the exit.</summary>
        public void Stop()
        {
            var p = _process;
            if (p == null) return;
            try
            {
                if (!p.HasExited) KillTree(p);
            }
            catch (Exception)
            {
                // already gone
            }
            try { p.WaitForExit(3000); } catch (Exception) { }
        }

        /// <summary>Kills a process and all of its descendants.</summary>
        public static void KillTree(Process p)
        {
#if NET5_0_OR_GREATER
            try { p.Kill(entireProcessTree: true); } catch (Exception) { }
#else
            if (Environment.OSVersion.Platform == PlatformID.Win32NT)
            {
                try
                {
                    var psi = new ProcessStartInfo
                    {
                        FileName = "taskkill",
                        Arguments = "/PID " + p.Id + " /T /F",
                        UseShellExecute = false,
                        CreateNoWindow = true,
                        RedirectStandardOutput = true,
                        RedirectStandardError = true,
                    };
                    using (var tk = Process.Start(psi))
                    {
                        tk?.WaitForExit(3000);
                    }
                }
                catch (Exception)
                {
                    // taskkill missing: fall back to Kill()
                }
            }
            try { if (!p.HasExited) p.Kill(); } catch (Exception) { }
#endif
        }

        public void Dispose()
        {
            if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
            Stop();
            try { _process?.Dispose(); } catch (Exception) { }
        }
    }
}
