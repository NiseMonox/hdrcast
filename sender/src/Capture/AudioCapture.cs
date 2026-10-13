using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.IO.Pipes;
using System.Threading;
using HdrCast.Capture;
using NAudio.CoreAudioApi;
using NAudio.Wave;

namespace HdrCast.Sender.Capture
{
    /// <summary>
    /// Desktop audio via WASAPI loopback, converted to f32le 48 kHz stereo and written to the audio pipe
    /// (design.md §2.4). With an app to leave out (the voice chat) it is process loopback instead
    /// (<see cref="ProcessLoopbackCapture"/>), following that app as it starts, quits and restarts.
    /// The stream is kept locked to the video clock by <see cref="AudioClockAligner"/>:
    /// silence is inserted when we fall behind, surplus frames are dropped when we run ahead, so ffmpeg can
    /// use ideal timestamps on both inputs.
    /// </summary>
    internal sealed class AudioCapture : IDisposable
    {
        public const int TargetSampleRate = 48000;
        public const int TargetChannels = 2;

        readonly string _pipeName;
        readonly string _deviceId;
        readonly string _excludeApp;
        readonly AudioClockAligner _aligner = new AudioClockAligner(TargetSampleRate);
        readonly object _gate = new object();
        readonly object _switchGate = new object();
        readonly Stopwatch _clock;
        readonly int _selfPid = Process.GetCurrentProcess().Id;

        IWaveIn? _capture;
        ProcessLoopbackCapture? _processCapture;
        Timer? _targetTimer;
        IWavePlayer? _silence;
        MMDevice? _device;
        NamedPipeServerStream? _pipe;
        Thread? _writerThread;
        volatile bool _running;
        volatile bool _clientConnected;
        volatile Exception? _error;

        // Ring of converted target-format samples waiting for the pipe writer.
        float[] _ring = new float[TargetSampleRate * TargetChannels * 4];   // 4 s
        int _ringHead, _ringTail, _ringCount;

        // Resampler state (previous tail sample per channel and fractional position).
        float[] _lastSample = new float[TargetChannels];
        double _resamplePos;
        float[] _scratch = new float[0];

        /// <param name="excludeApp">Program whose audio (with everything it started) is left out, e.g. the voice chat; "" = none.</param>
        public AudioCapture(string pipeName, string deviceId, Stopwatch clock, string excludeApp = "")
        {
            _pipeName = pipeName;
            _deviceId = deviceId ?? "default";
            _clock = clock;
            _excludeApp = excludeApp ?? "";
        }

        public bool Running => _running;
        public Exception? Error => _error;
        public int Underruns => _aligner.Underruns;
        public long WrittenFrames => _aligner.WrittenFrames;
        public string FormatDescription { get; private set; } = "";
        /// <summary>True once ffmpeg opened the audio pipe.</summary>
        public bool ClientConnected => _clientConnected;
        /// <summary>True when the audio endpoint could not be opened and the stream is silence only.</summary>
        public bool Degraded { get; private set; }

        /// <summary>
        /// Creates the pipe first and opens the audio endpoint second, on purpose: ffmpeg was started with an
        /// audio input, so the pipe MUST exist even when the endpoint cannot be opened (unplugged headphones,
        /// a stale device id in sender.json, no render endpoint at all). If WASAPI fails the stream degrades to
        /// silence and the session keeps running, instead of ffmpeg dying on a missing pipe and the GUI blaming
        /// the encoder.
        /// </summary>
        public void Start()
        {
            CreatePipe();
            BeginCapture();
        }

        /// <summary>
        /// Creates the pipe and its writer thread. Done early, before the video pipe waits for ffmpeg: ffmpeg
        /// opens both inputs, so both pipes have to exist or the two sides wait for each other forever.
        /// </summary>
        public void CreatePipe()
        {
            if (_pipe != null) return;
            _pipe = new NamedPipeServerStream(_pipeName, PipeDirection.Out, 1, PipeTransmissionMode.Byte,
                                              PipeOptions.Asynchronous, 0, 8 << 20);
            _running = true;
            _writerThread = new Thread(WriterLoop) { IsBackground = true, Name = "audio-pipe-writer" };
            _writerThread.Start();
        }

        /// <summary>
        /// Opens the audio endpoint and starts recording. Called once the video consumer is connected so the
        /// audio timeline starts with the video one; starting it earlier would leave the first seconds of audio
        /// sitting in the ring buffer and the two streams offset by the connect delay.
        /// </summary>
        public void BeginCapture()
        {
            if (_capture != null || Degraded) return;
            try
            {
                _device = ResolveDevice(_deviceId);
                lock (_switchGate)
                {
                    if (_excludeApp.Length > 0)
                    {
                        int target = ExcludeTarget(out int root);
                        if (StartProcessCapture(target, root))
                            _targetTimer = new Timer(_ => FollowExcludedApp(), null, 1000, 1000);
                        else
                            Log.Warn("audio: cannot leave out " + _excludeApp + " on this Windows (needs Windows 10 21H1 or newer); sending all desktop audio");
                    }
                    if (_capture == null) StartDeviceCapture();
                }
                WaveFormat f = _capture!.WaveFormat;

                // A completely idle render endpoint delivers no loopback data at all, so play silence into it.
                try
                {
                    var silenceOut = new WasapiOut(_device, AudioClientShareMode.Shared, false, 100);
                    silenceOut.Init(new SilenceProvider(new WaveFormat(f.SampleRate, 16, Math.Max(1, f.Channels))));
                    silenceOut.Play();
                    _silence = silenceOut;
                }
                catch (Exception ex)
                {
                    Log.Warn("audio: could not start the silence keep-alive: " + ex.Message);
                }

                Log.Info("audio: capturing " + FormatDescription + " from " + (_device?.FriendlyName ?? "default"));
            }
            catch (Exception ex)
            {
                Degraded = true;
                FormatDescription = "silence (" + ex.Message + ")";
                Log.Warn("audio: no desktop audio (" + ex.Message + "); sending silence so the stream keeps its audio track");
                lock (_switchGate)
                {
                    try { _targetTimer?.Dispose(); } catch (Exception) { }
                    try { _capture?.Dispose(); } catch (Exception) { }
                    _targetTimer = null;
                    _capture = null;
                    _processCapture = null;
                }
                try { _device?.Dispose(); } catch (Exception) { }
                _device = null;
            }
        }

        static string Describe(WaveFormat f) => f.Encoding + " " + f.SampleRate + " Hz " + f.Channels + " ch " + f.BitsPerSample + " bit";

        /// <summary>Plain loopback of the whole endpoint. Call under _switchGate.</summary>
        void StartDeviceCapture()
        {
            var loopback = new WasapiLoopbackCapture(_device);
            loopback.DataAvailable += OnData;
            loopback.RecordingStopped += OnStopped;
            FormatDescription = Describe(loopback.WaveFormat);
            _capture = loopback;
            loopback.StartRecording();
        }

        /// <summary>
        /// Everything except <paramref name="target"/>'s process tree (the excluded app, or this engine while that app is
        /// not running). False when this Windows has no process loopback. Call under _switchGate.
        /// </summary>
        bool StartProcessCapture(int target, int root)
        {
            var capture = new ProcessLoopbackCapture(target, includeTree: false);
            capture.DataAvailable += OnData;
            capture.RecordingStopped += OnStopped;
            try
            {
                capture.StartRecording();
            }
            catch (Exception ex)
            {
                capture.DataAvailable -= OnData;
                capture.RecordingStopped -= OnStopped;
                capture.Dispose();
                Log.Warn("audio: process loopback unavailable: " + ex.Message);
                return false;
            }
            _processCapture = capture;
            _capture = capture;
            FormatDescription = Describe(capture.WaveFormat) + ", without " + _excludeApp;
            Log.Info(root > 0
                ? "audio: leaving out " + _excludeApp + " (pid " + root + " and everything it started)"
                : "audio: " + _excludeApp + " is not running; sending all desktop audio until it starts");
            return true;
        }

        /// <summary>The process tree to exclude: the app's topmost process, or this engine (i.e. nothing) while it is not running.</summary>
        int ExcludeTarget(out int root)
        {
            root = 0;
            try { root = AppAudioTarget.FindRoot(ProcessSnapshot.Take(), _excludeApp); }
            catch (Exception ex) { Log.Warn("audio: cannot list processes: " + ex.Message); }
            return root > 0 ? root : _selfPid;
        }

        /// <summary>
        /// Every second (a process snapshot takes a millisecond or two): when the app started, quit or restarted the excluded
        /// tree changed, so restart the capture on it. Until then up to a second of a freshly started app can slip through.
        /// </summary>
        void FollowExcludedApp()
        {
            lock (_switchGate)
            {
                ProcessLoopbackCapture? current = _processCapture;
                if (!_running || current == null) return;
                int target = ExcludeTarget(out int root);
                if (target == current.TargetPid) return;
                current.DataAvailable -= OnData;
                current.RecordingStopped -= OnStopped;
                current.Dispose();
                _processCapture = null;
                _capture = null;
                if (StartProcessCapture(target, root)) return;
                Log.Warn("audio: sending all desktop audio for the rest of this session");
                try { _targetTimer?.Dispose(); } catch (Exception) { }
                _targetTimer = null;
                try { StartDeviceCapture(); }
                catch (Exception ex) { Log.Error("audio: cannot restart the capture: " + ex.Message); }
            }
        }

        /// <summary>Programs that have an audio session on the endpoint, as exe names, for the "Leave out" list.</summary>
        public static string[] ListAudioApps(string deviceId)
        {
            var names = new SortedSet<string>(StringComparer.OrdinalIgnoreCase);
            try
            {
                using (MMDevice device = ResolveDevice(deviceId))
                {
                    AudioSessionManager manager = device.AudioSessionManager;
                    manager.RefreshSessions();
                    SessionCollection sessions = manager.Sessions;
                    for (int i = 0; i < sessions.Count; i++)
                    {
                        AudioSessionControl s = sessions[i];
                        int pid = (int)s.GetProcessID;
                        if (s.IsSystemSoundsSession || pid <= 0) continue;
                        try
                        {
                            using (Process p = Process.GetProcessById(pid)) names.Add(p.ProcessName + ".exe");
                        }
                        catch (Exception)
                        {
                            // gone meanwhile, or not ours to inspect
                        }
                    }
                }
            }
            catch (Exception ex)
            {
                Log.Warn("audio: cannot list audio sessions: " + ex.Message);
            }
            names.Remove(Process.GetCurrentProcess().ProcessName + ".exe");
            return new List<string>(names).ToArray();
        }

        static MMDevice ResolveDevice(string id)
        {
            var enumerator = new MMDeviceEnumerator();
            if (string.IsNullOrEmpty(id) || id == "default")
                return enumerator.GetDefaultAudioEndpoint(DataFlow.Render, Role.Console);
            foreach (MMDevice d in enumerator.EnumerateAudioEndPoints(DataFlow.Render, DeviceState.Active))
            {
                if (string.Equals(d.ID, id, StringComparison.OrdinalIgnoreCase)) return d;
            }
            throw new InvalidOperationException("audio device not found: " + id);
        }

        /// <summary>Lists the active render endpoints as "id\tname" for the GUI.</summary>
        public static string[] ListDevices()
        {
            var list = new System.Collections.Generic.List<string>();
            try
            {
                var enumerator = new MMDeviceEnumerator();
                MMDevice def = enumerator.GetDefaultAudioEndpoint(DataFlow.Render, Role.Console);
                foreach (MMDevice d in enumerator.EnumerateAudioEndPoints(DataFlow.Render, DeviceState.Active))
                    list.Add(d.ID + "\t" + d.FriendlyName + (d.ID == def.ID ? " (default)" : ""));
            }
            catch (Exception ex)
            {
                Log.Warn("audio: cannot enumerate devices: " + ex.Message);
            }
            return list.ToArray();
        }

        void OnData(object? sender, WaveInEventArgs e)
        {
            // The format comes from the capture that raised this: a switch (the excluded app restarted) swaps them.
            if (!_running || e.BytesRecorded <= 0 || !(sender is IWaveIn source)) return;
            try
            {
                int frames = ConvertToTarget(e.Buffer, e.BytesRecorded, source.WaveFormat, out float[] converted);
                if (frames <= 0) return;

                lock (_gate)
                {
                    long expected = _aligner.ExpectedFrames(_clock.ElapsedTicks, Stopwatch.Frequency);
                    AudioAlignment align = _aligner.Align(expected, frames);
                    int offset = Math.Min(align.DropFrames, frames) * TargetChannels;
                    int count = (frames - Math.Min(align.DropFrames, frames)) * TargetChannels;
                    if (count > 0) RingWrite(converted, offset, count);
                    if (align.PadFrames > 0) RingWriteSilence(align.PadFrames * TargetChannels);
                    Monitor.Pulse(_gate);
                }
            }
            catch (Exception ex)
            {
                _error = ex;
                Log.Error("audio: conversion failed: " + ex.Message);
            }
        }

        void OnStopped(object? sender, StoppedEventArgs e)
        {
            if (e.Exception != null)
            {
                _error = e.Exception;
                Log.Error("audio: recording stopped: " + e.Exception.Message);
            }
        }

        /// <summary>Converts one WASAPI buffer to interleaved 48 kHz stereo float. Returns the number of frames.</summary>
        int ConvertToTarget(byte[] buffer, int bytes, WaveFormat format, out float[] output)
        {
            int channels = Math.Max(1, format.Channels);
            int bytesPerSample = Math.Max(1, format.BitsPerSample / 8);
            int srcFrames = bytes / (bytesPerSample * channels);
            if (srcFrames <= 0) { output = _scratch; return 0; }

            // 1. decode to stereo float at the source rate
            int needed = srcFrames * TargetChannels;
            if (_scratch.Length < needed) _scratch = new float[needed * 2];
            for (int i = 0; i < srcFrames; i++)
            {
                float left = 0f, right = 0f;
                for (int c = 0; c < channels; c++)
                {
                    int idx = (i * channels + c) * bytesPerSample;
                    float v = ReadSample(buffer, idx, format, bytesPerSample);
                    if (channels == 1) { left = v; right = v; }
                    else if (c == 0) left = v;
                    else if (c == 1) right = v;
                    // extra channels (centre / surround) are ignored: a simple front downmix
                }
                _scratch[i * 2] = left;
                _scratch[i * 2 + 1] = right;
            }

            if (format.SampleRate == TargetSampleRate)
            {
                output = _scratch;
                return srcFrames;
            }

            // 2. linear resample to 48 kHz (desktop audio is 44.1/96 kHz only on unusual setups)
            double ratio = (double)TargetSampleRate / format.SampleRate;
            int outFrames = (int)Math.Floor((srcFrames - _resamplePos) * ratio) + 1;
            if (outFrames < 0) outFrames = 0;
            var result = new float[Math.Max(1, outFrames) * TargetChannels];
            int produced = 0;
            double pos = _resamplePos;
            while (true)
            {
                int i0 = (int)Math.Floor(pos);
                if (i0 >= srcFrames) break;
                double frac = pos - i0;
                for (int c = 0; c < TargetChannels; c++)
                {
                    float a = i0 < 0 ? _lastSample[c] : _scratch[i0 * TargetChannels + c];
                    int i1 = i0 + 1;
                    float b = i1 < srcFrames ? _scratch[i1 * TargetChannels + c] : a;
                    if (produced * TargetChannels + c < result.Length)
                        result[produced * TargetChannels + c] = (float)(a + (b - a) * frac);
                }
                produced++;
                if (produced * TargetChannels >= result.Length) break;
                pos += 1.0 / ratio;
            }
            _resamplePos = pos - srcFrames;
            if (_resamplePos < 0) _resamplePos = 0;
            for (int c = 0; c < TargetChannels; c++)
                _lastSample[c] = _scratch[(srcFrames - 1) * TargetChannels + c];
            output = result;
            return produced;
        }

        static float ReadSample(byte[] buffer, int index, WaveFormat format, int bytesPerSample)
        {
            if (IsFloat(format)) return BitConverter.ToSingle(buffer, index);
            switch (format.BitsPerSample)
            {
                case 16: return BitConverter.ToInt16(buffer, index) / 32768f;
                case 24:
                {
                    int v = buffer[index] | (buffer[index + 1] << 8) | ((sbyte)buffer[index + 2] << 16);
                    return v / 8388608f;
                }
                case 32: return BitConverter.ToInt32(buffer, index) / 2147483648f;
                case 8: return (buffer[index] - 128) / 128f;
                default: return 0f;
            }
        }

        /// <summary>The WASAPI shared-mode mix format is IEEE float; WAVE_FORMAT_EXTENSIBLE wraps the same thing.</summary>
        static bool IsFloat(WaveFormat format)
        {
            if (format.Encoding == WaveFormatEncoding.IeeeFloat) return true;
            var ext = format as WaveFormatExtensible;
            if (ext != null && format.BitsPerSample == 32)
            {
                // WaveFormatExtensible.ToStandardWaveFormat() resolves the sub-format to IeeeFloat / Pcm.
                try { return ext.ToStandardWaveFormat().Encoding == WaveFormatEncoding.IeeeFloat; }
                catch (Exception) { return false; }
            }
            return false;
        }

        void RingWrite(float[] data, int offset, int count)
        {
            if (count <= 0) return;
            if (_ringCount + count > _ring.Length)
            {
                // Overflow: the consumer is far behind (ffmpeg not reading). Drop the oldest samples.
                int drop = _ringCount + count - _ring.Length;
                _ringHead = (_ringHead + drop) % _ring.Length;
                _ringCount -= drop;
            }
            for (int i = 0; i < count; i++)
            {
                _ring[_ringTail] = data[offset + i];
                _ringTail = (_ringTail + 1) % _ring.Length;
            }
            _ringCount += count;
        }

        void RingWriteSilence(int count)
        {
            if (count <= 0) return;
            if (_ringCount + count > _ring.Length) count = Math.Max(0, _ring.Length - _ringCount);
            for (int i = 0; i < count; i++)
            {
                _ring[_ringTail] = 0f;
                _ringTail = (_ringTail + 1) % _ring.Length;
            }
            _ringCount += count;
        }

        void WriterLoop()
        {
            try
            {
                if (_pipe == null) return;
                _pipe.WaitForConnection();
                _clientConnected = true;
                Log.Info("audio: pipe client connected");
                var bytes = new byte[16384];
                var samples = new float[bytes.Length / 4];
                while (_running)
                {
                    int take;
                    lock (_gate)
                    {
                        if (_ringCount == 0) Monitor.Wait(_gate, 20);
                        if (_ringCount == 0)
                        {
                            // Nothing captured: either the endpoint is silent/idle or it never opened. Top the
                            // stream up from the same clock the video uses so ffmpeg's audio track keeps moving
                            // and the muxer does not stall waiting for samples.
                            long expected = _aligner.ExpectedFrames(_clock.ElapsedTicks, Stopwatch.Frequency);
                            int pad = _aligner.PadIfBehind(expected);
                            if (pad > 0) RingWriteSilence(Math.Min(pad, _ring.Length / 2) * TargetChannels);
                        }
                        take = Math.Min(_ringCount, samples.Length);
                        for (int i = 0; i < take; i++)
                        {
                            samples[i] = _ring[_ringHead];
                            _ringHead = (_ringHead + 1) % _ring.Length;
                        }
                        _ringCount -= take;
                    }
                    if (take == 0) continue;
                    Buffer.BlockCopy(samples, 0, bytes, 0, take * 4);
                    _pipe.Write(bytes, 0, take * 4);
                }
            }
            catch (Exception ex)
            {
                if (_running) { _error = ex; Log.Warn("audio: pipe writer stopped: " + ex.Message); }
            }
        }

        public void Dispose()
        {
            _running = false;
            lock (_gate) Monitor.PulseAll(_gate);
            lock (_switchGate)
            {
                try { _targetTimer?.Dispose(); } catch (Exception) { }
                _targetTimer = null;
                try { _capture?.StopRecording(); } catch (Exception) { }
            }
            try { _silence?.Stop(); } catch (Exception) { }
            if (_writerThread != null && _writerThread.IsAlive) _writerThread.Join(1000);
            try { _pipe?.Dispose(); } catch (Exception) { }
            lock (_switchGate)
            {
                try { _capture?.Dispose(); } catch (Exception) { }
                _capture = null;
                _processCapture = null;
            }
            try { _silence?.Dispose(); } catch (Exception) { }
            try { _device?.Dispose(); } catch (Exception) { }
            _pipe = null; _silence = null; _device = null;
        }
    }
}
