using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Globalization;
using System.Threading;
using HdrCast.Capture;
using HdrCast.Common;
using HdrCast.Sender.Model;

namespace HdrCast.Sender
{
    public enum RunnerState
    {
        Idle,
        Starting,
        Streaming,
        Stopping,
        Failed,
    }

    /// <summary>Everything needed for one streaming session; built by the GUI from the settings plus the chosen monitor.</summary>
    public sealed class SendPlan
    {
        public string CaptureExe { get; set; } = "";
        public string[] CaptureArgv { get; set; } = new string[0];
        public string FfmpegExe { get; set; } = "";
        public string[] FfmpegArgv { get; set; } = new string[0];
        public string OutputUrl { get; set; } = "";
        public int Fps { get; set; } = 60;
        public string Summary { get; set; } = "";
    }

    /// <summary>
    /// Owns the capture engine and the ffmpeg child processes for one session (design.md §2.5).
    /// The capture engine is started first (it is the pipe server); ffmpeg connects to the pipes.
    /// If either process exits the other is stopped, and the session is optionally restarted.
    /// </summary>
    public sealed class SenderRunner
    {
        const int RestartDelayMs = 3000;

        readonly object _gate = new object();
        LineProcess? _capture;
        LineProcess? _ffmpeg;
        SendPlan? _plan;
        Timer? _restartTimer;
        int _generation;
        bool _userStopped = true;
        bool _autoRestart;
        int _attempt;
        DateTime _startedUtc;

        public event Action<string>? LogLine;
        public event Action<RunnerState, string>? StateChanged;

        public RunnerState State { get; private set; } = RunnerState.Idle;
        public string StateDetail { get; private set; } = "";
        public CaptureStats? LastCaptureStats { get; private set; }
        public FfmpegProgress? LastProgress { get; private set; }
        public DateTime StartedUtc => _startedUtc;
        public int Attempt => _attempt;
        public bool IsRunning => State == RunnerState.Starting || State == RunnerState.Streaming;

        public void Start(SendPlan plan, bool autoRestart)
        {
            Stop();
            lock (_gate)
            {
                _plan = plan;
                _autoRestart = autoRestart;
                _userStopped = false;
                _attempt = 0;
            }
            Launch();
        }

        public void Stop()
        {
            LineProcess? capture, ffmpeg;
            lock (_gate)
            {
                _userStopped = true;
                _generation++;
                capture = _capture;
                ffmpeg = _ffmpeg;
                _capture = null;
                _ffmpeg = null;
                _restartTimer?.Dispose();
                _restartTimer = null;
            }
            if (capture == null && ffmpeg == null)
            {
                if (State != RunnerState.Idle) SetState(RunnerState.Idle, "Stopped");
                return;
            }
            SetState(RunnerState.Stopping, "Stopping…");
            // ffmpeg first: it flushes the muxer and closes the SRT connection, then the engine sees the pipe close.
            try { ffmpeg?.Stop(); } catch (Exception) { }
            try { capture?.Stop(); } catch (Exception) { }
            try { ffmpeg?.Dispose(); } catch (Exception) { }
            try { capture?.Dispose(); } catch (Exception) { }
            SetState(RunnerState.Idle, "Stopped");
        }

        void Launch()
        {
            SendPlan plan;
            int gen;
            lock (_gate)
            {
                if (_plan == null || _userStopped) return;
                plan = _plan;
                gen = ++_generation;
                _attempt++;
            }

            SetState(RunnerState.Starting, _attempt == 1 ? "Starting…" : "Restarting (attempt " + _attempt + ")…");
            LineProcess? capture = null;
            LineProcess? ffmpeg = null;
            try
            {
                capture = new LineProcess(plan.CaptureExe, ArgQuote.Join(plan.CaptureArgv));
                capture.StdoutLine += line => OnCaptureStats(gen, line);
                capture.StderrLine += line => Emit("[capture] " + line);
                capture.Exited += code => OnCaptureExited(gen, code);

                ffmpeg = new LineProcess(plan.FfmpegExe, ArgQuote.Join(plan.FfmpegArgv));
                ffmpeg.StdoutLine += line => OnFfmpegProgress(gen, line);
                ffmpeg.StderrLine += line => Emit("[ffmpeg] " + line);
                ffmpeg.Exited += code => OnFfmpegExited(gen, code);

                Emit("[run] " + capture.CommandLine);
                Emit("[run] " + ffmpeg.CommandLine);
                SenderLog.Write("run", capture.CommandLine);
                SenderLog.Write("run", ffmpeg.CommandLine);

                lock (_gate)
                {
                    if (_userStopped || gen != _generation)
                    {
                        capture.Dispose();
                        ffmpeg.Dispose();
                        return;
                    }
                    _capture = capture;
                    _ffmpeg = ffmpeg;
                    _startedUtc = DateTime.UtcNow;
                }

                capture.Start();
                // Give the engine a moment to create the named pipes before ffmpeg tries to open them.
                Thread.Sleep(400);
                lock (_gate) { if (gen != _generation || _userStopped) return; }
                ffmpeg.Start();
                SetState(RunnerState.Streaming, "Streaming");
            }
            catch (Exception ex)
            {
                Emit("[run] failed to start: " + ex.Message);
                SenderLog.Write("run", "start failed: " + ex);
                lock (_gate) { _capture = null; _ffmpeg = null; }
                try { ffmpeg?.Dispose(); } catch (Exception) { }
                try { capture?.Dispose(); } catch (Exception) { }
                Fail("could not start: " + ex.Message);
            }
        }

        void OnCaptureStats(int gen, string line)
        {
            if (gen != _generation) return;
            CaptureStats? stats = CaptureStats.TryParse(line);
            if (stats != null) LastCaptureStats = stats;
            else Emit("[capture] " + line);
        }

        readonly FfmpegProgressParser _progress = new FfmpegProgressParser();

        void OnFfmpegProgress(int gen, string line)
        {
            if (gen != _generation) return;
            FfmpegProgress? block = _progress.Feed(line);
            if (block != null) LastProgress = block;
        }

        void OnCaptureExited(int gen, int code)
        {
            if (gen != _generation) return;
            string reason = DescribeCaptureExit(code);
            Emit("[capture] exited: " + reason);
            SenderLog.Write("capture", "exited " + code + " (" + reason + ")");
            StopPeerAndMaybeRestart(gen, "capture engine stopped: " + reason);
        }

        void OnFfmpegExited(int gen, int code)
        {
            if (gen != _generation) return;
            Emit("[ffmpeg] exited with code " + code);
            SenderLog.Write("ffmpeg", "exited " + code);
            StopPeerAndMaybeRestart(gen, "ffmpeg stopped (exit code " + code + ")");
        }

        void StopPeerAndMaybeRestart(int gen, string reason)
        {
            LineProcess? capture, ffmpeg;
            bool restart;
            lock (_gate)
            {
                if (gen != _generation || _userStopped) return;
                capture = _capture;
                ffmpeg = _ffmpeg;
                _capture = null;
                _ffmpeg = null;
                restart = _autoRestart;
            }
            try { ffmpeg?.Stop(); } catch (Exception) { }
            try { capture?.Stop(); } catch (Exception) { }
            try { ffmpeg?.Dispose(); } catch (Exception) { }
            try { capture?.Dispose(); } catch (Exception) { }

            if (!restart)
            {
                Fail(reason);
                return;
            }
            SetState(RunnerState.Starting, reason + " — restarting in " + (RestartDelayMs / 1000) + " s");
            lock (_gate)
            {
                _restartTimer?.Dispose();
                _restartTimer = new Timer(_ => Launch(), null, RestartDelayMs, Timeout.Infinite);
            }
        }

        static string DescribeCaptureExit(int code)
        {
            switch (code)
            {
                case 0: return "finished";
                case 2: return "bad arguments";
                case 3: return "GPU / display error";
                case 4: return "the pipe consumer went away (ffmpeg stopped)";
                case 5: return "desktop duplication lost";
                default: return "exit code " + code.ToString(CultureInfo.InvariantCulture);
            }
        }

        void Fail(string detail)
        {
            LastProgress = null;
            SetState(RunnerState.Failed, detail);
        }

        void SetState(RunnerState state, string detail)
        {
            State = state;
            StateDetail = detail;
            StateChanged?.Invoke(state, detail);
        }

        void Emit(string line)
        {
            LogLine?.Invoke(line);
        }
    }
}
