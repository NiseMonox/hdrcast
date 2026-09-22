using System;
using System.Collections.Generic;
using System.IO;
using System.Threading;
using HdrCast.Common;

namespace HdrCast.Receiver
{
    internal enum RunnerState
    {
        Idle = 0,
        Starting = 1,
        WaitingForStream = 2,
        Playing = 3,
        Stopping = 4,
    }

    /// <summary>
    /// Owns the mpv (and optional ffmpeg) child processes: launches them from an <see cref="MpvLaunchPlan"/>,
    /// attaches the IPC client, retries every 3 s while the path to the host is not up yet, and kills the tree
    /// on Stop()/Dispose(). All events fire on worker threads; the form marshals to the UI thread.
    /// </summary>
    internal sealed class MpvRunner : IDisposable
    {
        public const int RetryDelayMs = 3000;
        private const int RecentLineCount = 40;

        private readonly object _gate = new object();
        private readonly List<string> _recentLines = new List<string>();
        private int _generation;
        private bool _userStopped = true;
        private int _attempt;
        private LineProcess? _mpv;
        private LineProcess? _ffmpeg;
        private MpvIpcClient? _ipc;
        private Timer? _retryTimer;
        private MpvLaunchPlan? _plan;
        private ReceiverSettings? _settings;
        private bool _catchingUp;
        private DateTime _startedUtc;

        public MpvStatusModel Status { get; } = new MpvStatusModel();
        public RunnerState State { get; private set; } = RunnerState.Idle;
        public string StateText { get; private set; } = "Idle";
        public bool IsRunning => !_userStopped;
        public int Attempt => _attempt;
        public TimeSpan Uptime => _userStopped ? TimeSpan.Zero : DateTime.UtcNow - _startedUtc;

        /// <summary>Raw child-process / IPC log lines (already prefixed with [mpv] / [ffmpeg] / [ipc]).</summary>
        public event Action<string>? LogLine;
        public event Action<RunnerState, string>? StateChanged;

        /// <summary>Job object the children are assigned to, so mpv cannot outlive the GUI.</summary>
        public ProcessJob? Job { get; set; }
        /// <summary>Raised after every property-change applied to <see cref="Status"/>.</summary>
        public event Action? StatusChanged;

        public void Start(ReceiverSettings settings, MpvLaunchPlan plan)
        {
            lock (_gate)
            {
                if (!_userStopped) throw new InvalidOperationException("already running");
                _settings = settings ?? throw new ArgumentNullException(nameof(settings));
                _plan = plan ?? throw new ArgumentNullException(nameof(plan));
                _userStopped = false;
                _attempt = 0;
                _startedUtc = DateTime.UtcNow;
                Status.Reset();
            }
            Launch();
        }

        public void Stop()
        {
            int gen;
            LineProcess? mpv;
            LineProcess? ffmpeg;
            MpvIpcClient? ipc;
            lock (_gate)
            {
                if (_userStopped && _mpv == null) return;
                _userStopped = true;
                gen = ++_generation;
                _retryTimer?.Dispose();
                _retryTimer = null;
                mpv = _mpv;
                ffmpeg = _ffmpeg;
                ipc = _ipc;
                _mpv = null;
                _ffmpeg = null;
                _ipc = null;
            }
            SetState(RunnerState.Stopping, "Stopping…");
            try
            {
                // Ask mpv politely first so it tears down the swapchain cleanly, then kill whatever is left.
                if (ipc != null && ipc.IsConnected)
                {
                    ipc.Quit();
                    mpv?.WaitForExit(1500);
                }
            }
            catch (Exception) { }
            ffmpeg?.Stop();
            mpv?.Stop();
            ipc?.Dispose();
            ffmpeg?.Dispose();
            mpv?.Dispose();

            // Killing mpv takes seconds. If the user pressed Play again in the meantime a newer generation owns
            // the state now, and resetting it here would blank the status of a session that is actually running.
            lock (_gate) { if (gen != _generation) return; }
            Status.Reset();
            StatusChanged?.Invoke();
            SetState(RunnerState.Idle, "Idle");
        }

        public void ToggleFullscreen()
        {
            var ipc = _ipc;
            if (ipc == null || !ipc.IsConnected)
            {
                Emit("[ipc] not connected: cannot toggle fullscreen");
                return;
            }
            ipc.CycleProperty("fullscreen");
        }

        public void SetFullscreen(bool on)
        {
            var ipc = _ipc;
            if (ipc == null || !ipc.IsConnected) return;
            ipc.SetProperty("fullscreen", on);
        }

        // ---- launching ------------------------------------------------------------------------

        private void Launch()
        {
            int gen;
            MpvLaunchPlan plan;
            lock (_gate)
            {
                if (_userStopped || _plan == null) return;
                gen = ++_generation;
                plan = _plan;
                _attempt++;
                _catchingUp = false;
                lock (_recentLines) _recentLines.Clear();
            }
            SetState(RunnerState.Starting, _attempt == 1 ? "Starting mpv…" : "Reconnecting (attempt " + _attempt + ")…");

            LineProcess? ffmpeg = null;
            LineProcess? mpv = null;
            try
            {
                mpv = new LineProcess(plan.MpvExe, ArgQuote.Join(plan.MpvArgs))
                {
                    RedirectStdin = plan.UsesFfmpegPipe,
                    Job = Job,
                };
                mpv.StdoutLine += l => OnMpvLine(gen, l);
                mpv.StderrLine += l => OnMpvLine(gen, l);
                mpv.Exited += code => OnMpvExited(gen, code);

                if (plan.UsesFfmpegPipe)
                {
                    ffmpeg = new LineProcess(plan.FfmpegExe!, ArgQuote.Join(plan.FfmpegArgs!))
                    {
                        ReadStdoutLines = false,
                        Job = Job,
                    };
                    ffmpeg.StderrLine += l => Emit("[ffmpeg] " + l);
                    ffmpeg.Exited += code => OnFfmpegExited(gen, code);
                }

                // The log folder is meant to be shareable, so the passphrase and the stream-id password are masked.
                Emit("[run] " + mpv.SafeCommandLine);
                if (ffmpeg != null) Emit("[run] " + ffmpeg.SafeCommandLine);

                lock (_gate)
                {
                    if (_userStopped || gen != _generation)
                    {
                        mpv.Dispose();
                        ffmpeg?.Dispose();
                        return;
                    }
                    _mpv = mpv;
                    _ffmpeg = ffmpeg;
                }

                SetState(RunnerState.WaitingForStream, "Connecting…");
                mpv.Start();
                ffmpeg?.Start();
                if (ffmpeg != null) StartPump(gen, ffmpeg, mpv);
            }
            catch (Exception ex)
            {
                Emit("[run] failed to start: " + ex.Message);
                ReceiverLog.Write("run", "start failed: " + ex);
                lock (_gate)
                {
                    _mpv = null;
                    _ffmpeg = null;
                }
                ffmpeg?.Dispose();
                mpv?.Dispose();
                ScheduleRetry(gen, "could not start: " + ex.Message);
                return;
            }

            var ipc = new MpvIpcClient(plan.PipeName);
            ipc.Log += l => Emit("[ipc] " + l);
            ipc.MessageReceived += m => OnIpcMessage(gen, m);
            ipc.Connected += () => Emit("[ipc] observing mpv properties");
            lock (_gate)
            {
                if (gen != _generation || mpv.HasExited)
                {
                    ipc.Dispose();
                    return;
                }
                _ipc = ipc;
            }
            var mpvRef = mpv;
            ipc.Start(() => gen == _generation && !mpvRef.HasExited);
        }

        private void StartPump(int gen, LineProcess ffmpeg, LineProcess mpv)
        {
            var t = new Thread(() =>
            {
                var src = ffmpeg.StdoutStream;
                var dst = mpv.StdinStream;
                if (src == null || dst == null) return;
                var buf = new byte[65536];
                try
                {
                    int n;
                    while (gen == _generation && (n = src.Read(buf, 0, buf.Length)) > 0)
                    {
                        dst.Write(buf, 0, n);
                    }
                }
                catch (Exception ex)
                {
                    if (gen == _generation) Emit("[pipe] ended: " + ex.Message);
                }
                finally
                {
                    try { dst.Close(); } catch (Exception) { }
                }
            }) { IsBackground = true, Name = "ffmpeg-to-mpv" };
            t.Start();
        }

        // ---- child events ----------------------------------------------------------------------

        private void OnMpvLine(int gen, string line)
        {
            if (gen != _generation) return;
            lock (_recentLines)
            {
                _recentLines.Add(line);
                if (_recentLines.Count > RecentLineCount) _recentLines.RemoveAt(0);
            }
            Emit("[mpv] " + line);
        }

        private void OnFfmpegExited(int gen, int code)
        {
            if (gen != _generation) return;
            Emit("[ffmpeg] exited with code " + code);
            // mpv sees EOF on stdin and exits by itself; OnMpvExited schedules the retry.
        }

        private void OnMpvExited(int gen, int code)
        {
            bool stopped;
            LineProcess? ffmpeg;
            MpvIpcClient? ipc;
            lock (_gate)
            {
                stopped = _userStopped || gen != _generation;
                ffmpeg = _ffmpeg;
                ipc = _ipc;
            }
            Emit("[mpv] exited with code " + code);
            if (stopped) return;

            MpvExitReason reason;
            lock (_recentLines) reason = MpvLogClassifier.Classify(_recentLines);
            string why = MpvLogClassifier.Describe(reason);
            ReceiverLog.Write("run", "mpv exited (" + code + "): " + why);

            ffmpeg?.Stop();
            ipc?.Dispose();
            lock (_gate)
            {
                if (gen == _generation)
                {
                    _mpv?.Dispose();
                    _ffmpeg?.Dispose();
                    _mpv = null;
                    _ffmpeg = null;
                    _ipc = null;
                }
            }
            Status.Reset();
            StatusChanged?.Invoke();
            ScheduleRetry(gen, why);
        }

        private void ScheduleRetry(int gen, string why)
        {
            lock (_gate)
            {
                if (_userStopped || gen != _generation) return;
                _retryTimer?.Dispose();
                _retryTimer = new Timer(_ => OnRetryTimer(gen), null, RetryDelayMs, Timeout.Infinite);
            }
            SetState(RunnerState.WaitingForStream, "Waiting for stream… (" + why + "; retry in " + (RetryDelayMs / 1000) + " s, attempt " + _attempt + ")");
        }

        private void OnRetryTimer(int gen)
        {
            lock (_gate)
            {
                if (_userStopped || gen != _generation) return;
                _retryTimer?.Dispose();
                _retryTimer = null;
            }
            Launch();
        }

        private void OnIpcMessage(int gen, MpvIpcMessage msg)
        {
            if (gen != _generation) return;
            if (msg.Kind == MpvIpcMessageKind.Event)
            {
                switch (msg.Event)
                {
                    case "property-change":
                        if (Status.Apply(msg))
                        {
                            if (msg.Name == MpvObservedProperties.DemuxerCacheDuration) MaybeCatchUp();
                            if (msg.Name == MpvObservedProperties.PausedForCache && State == RunnerState.Playing)
                                SetState(RunnerState.Playing, Status.PausedForCache ? "Playing (buffering…)" : "Playing");
                            StatusChanged?.Invoke();
                        }
                        break;
                    case "file-loaded":
                        SetState(RunnerState.Playing, "Playing");
                        break;
                    case "end-file":
                        Emit("[ipc] end-file: " + (msg.Reason ?? "?"));
                        break;
                    case "shutdown":
                        Emit("[ipc] mpv shutdown");
                        break;
                }
            }
            else if (msg.Kind == MpvIpcMessageKind.Reply && !msg.IsSuccess)
            {
                Emit("[ipc] command failed: " + msg.Error);
            }
        }

        /// <summary>Live catch-up: play at 1.05x while the demuxer cache exceeds buffer + 1.5 s, back to 1x at the target.</summary>
        private void MaybeCatchUp()
        {
            var s = _settings;
            var ipc = _ipc;
            if (s == null || !s.AutoCatchUp || ipc == null || !ipc.IsConnected || !Status.CacheSeconds.HasValue) return;
            if (Status.PausedForCache) return;
            double cache = Status.CacheSeconds.Value;
            double target = s.BufferSeconds;
            if (!_catchingUp && cache > target + 1.5)
            {
                _catchingUp = true;
                ipc.SetProperty("speed", 1.05);
                Emit("[ipc] cache " + cache.ToString("0.0", System.Globalization.CultureInfo.InvariantCulture) + " s > target: catching up at 1.05x");
            }
            else if (_catchingUp && cache <= target)
            {
                _catchingUp = false;
                ipc.SetProperty("speed", 1.0);
                Emit("[ipc] back at 1.0x");
            }
        }

        // ---- helpers ---------------------------------------------------------------------------

        private void SetState(RunnerState state, string text)
        {
            State = state;
            StateText = text;
            StateChanged?.Invoke(state, text);
        }

        private void Emit(string line)
        {
            // Every path into the log goes through here, so redact once: mpv prints the srt:// URL (passphrase
            // and stream-id password included) whenever a connection fails.
            line = SecretRedactor.Redact(line);
            ReceiverLog.Write(line);
            LogLine?.Invoke(line);
        }

        public void Dispose()
        {
            Stop();
        }
    }
}
