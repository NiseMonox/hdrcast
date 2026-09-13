using System;
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
    /// (design.md §2.4). The stream is kept locked to the video clock by <see cref="AudioClockAligner"/>:
    /// silence is inserted when we fall behind, surplus frames are dropped when we run ahead, so ffmpeg can
    /// use ideal timestamps on both inputs.
    /// </summary>
    internal sealed class AudioCapture : IDisposable
    {
        public const int TargetSampleRate = 48000;
        public const int TargetChannels = 2;

        readonly string _pipeName;
        readonly string _deviceId;
        readonly AudioClockAligner _aligner = new AudioClockAligner(TargetSampleRate);
        readonly object _gate = new object();
        readonly Stopwatch _clock;

        WasapiLoopbackCapture? _capture;
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

        public AudioCapture(string pipeName, string deviceId, Stopwatch clock)
        {
            _pipeName = pipeName;
            _deviceId = deviceId ?? "default";
            _clock = clock;
        }

        public bool Running => _running;
        public Exception? Error => _error;
        public int Underruns => _aligner.Underruns;
        public long WrittenFrames => _aligner.WrittenFrames;
        public string FormatDescription { get; private set; } = "";
        /// <summary>True once ffmpeg opened the audio pipe.</summary>
        public bool ClientConnected => _clientConnected;

        public void Start()
        {
            _device = ResolveDevice(_deviceId);
            _capture = new WasapiLoopbackCapture(_device);
            WaveFormat f = _capture.WaveFormat;
            FormatDescription = f.Encoding + " " + f.SampleRate + " Hz " + f.Channels + " ch " + f.BitsPerSample + " bit";
            _capture.DataAvailable += OnData;
            _capture.RecordingStopped += OnStopped;

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

            _pipe = new NamedPipeServerStream(_pipeName, PipeDirection.Out, 1, PipeTransmissionMode.Byte,
                                              PipeOptions.Asynchronous, 0, 8 << 20);
            _running = true;
            _writerThread = new Thread(WriterLoop) { IsBackground = true, Name = "audio-pipe-writer" };
            _writerThread.Start();
            _capture.StartRecording();
            Log.Info("audio: capturing " + FormatDescription + " from " + (_device?.FriendlyName ?? "default"));
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
            if (!_running || e.BytesRecorded <= 0 || _capture == null) return;
            try
            {
                int frames = ConvertToTarget(e.Buffer, e.BytesRecorded, _capture.WaveFormat, out float[] converted);
                if (frames <= 0) return;

                long expected = _aligner.ExpectedFrames(_clock.ElapsedTicks, Stopwatch.Frequency);
                AudioAlignment align = _aligner.Align(expected, frames);
                lock (_gate)
                {
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
            try { _capture?.StopRecording(); } catch (Exception) { }
            try { _silence?.Stop(); } catch (Exception) { }
            if (_writerThread != null && _writerThread.IsAlive) _writerThread.Join(1000);
            try { _pipe?.Dispose(); } catch (Exception) { }
            try { _capture?.Dispose(); } catch (Exception) { }
            try { _silence?.Dispose(); } catch (Exception) { }
            try { _device?.Dispose(); } catch (Exception) { }
            _pipe = null; _capture = null; _silence = null; _device = null;
        }
    }
}
