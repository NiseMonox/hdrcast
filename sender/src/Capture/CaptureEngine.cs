using System;
using System.Diagnostics;
using System.Globalization;
using System.Threading;
using HdrCast.Capture;

namespace HdrCast.Sender.Capture
{
    /// <summary>Process exit codes of the capture engine (design.md §2.1).</summary>
    internal static class ExitCodes
    {
        public const int Ok = 0;
        public const int BadArguments = 2;
        public const int DeviceError = 3;
        public const int PipeBroken = 4;
        public const int AccessLost = 5;
    }

    /// <summary>
    /// The capture engine: duplicates one desktop, converts every frame on the GPU and writes a constant-rate
    /// raw stream into the video pipe, plus desktop audio into the audio pipe (design.md §2.2).
    /// </summary>
    internal sealed class CaptureEngine : IDisposable
    {
        const int AccessLostRetryMs = 500;
        const int AccessLostGiveUpMs = 30000;

        readonly CaptureArgs _args;
        readonly Stopwatch _clock = new Stopwatch();
        readonly CancellationTokenSource _cancel = new CancellationTokenSource();

        DesktopDuplicator? _duplicator;
        GpuConverter? _converter;
        FrameSink? _sink;
        FramePool? _pool;
        AudioCapture? _audio;
        FrameBuffer? _latest;

        long _captured, _duplicated, _dropped;
        double _gpuMsAccum;
        int _gpuMsCount;

        public CaptureEngine(CaptureArgs args)
        {
            _args = args ?? throw new ArgumentNullException(nameof(args));
        }

        public void RequestStop() => _cancel.Cancel();

        public int Run()
        {
            _duplicator = new DesktopDuplicator(_args.OutputIndex, _args.Format != CaptureFormat.Nv12);
            try
            {
                _duplicator.Initialize();
            }
            catch (Exception ex)
            {
                Log.Error("capture: " + ex.Message);
                return ExitCodes.DeviceError;
            }

            CaptureFormat format = ResolveFormat(_duplicator);
            int srcW = _duplicator.DesktopWidth, srcH = _duplicator.DesktopHeight;
            int dstW = _args.HasExplicitSize ? _args.Width : Even(srcW);
            int dstH = _args.HasExplicitSize ? _args.Height : Even(srcH);

            Log.Info(string.Format(CultureInfo.InvariantCulture,
                "capture: output {0} ({1} {2}) {3}x{4} rot{5} hdr={6} source={7} -> {8} {9}x{10} @{11}fps",
                _args.OutputIndex, _duplicator.Info.DeviceName, _duplicator.Info.FriendlyName,
                srcW, srcH, _duplicator.Rotation, _duplicator.Info.Hdr,
                _duplicator.SourceIsFloat ? "rgbaf16(scRGB)" : "bgra8(sRGB)",
                CaptureArgs.FormatName(format), dstW, dstH, _args.Fps));

            if (format == CaptureFormat.P010 && !_duplicator.SourceIsFloat)
                Log.Warn("capture: the desktop is not delivering FP16; HDR highlights above SDR white are not available");

            try
            {
                _converter = new GpuConverter(_duplicator.Device, format, dstW, dstH);
            }
            catch (Exception ex)
            {
                Log.Error("capture: GPU converter failed: " + ex.Message);
                return ExitCodes.DeviceError;
            }

            int frameBytes = _converter.FrameBytes;
            _pool = new FramePool(6, frameBytes);
            _sink = _args.DumpPath != null
                ? FrameSink.CreateFile(_args.DumpPath, 4)
                : FrameSink.CreatePipe(_args.VideoPipe, 32 << 20, 4);

            if (_sink.IsPipe)
            {
                Log.Info("capture: waiting for the video pipe client on " + @"\\.\pipe\" + _args.VideoPipe +
                         " (" + _args.PipeTimeoutSeconds + "s)");
                if (!_sink.WaitForClient(TimeSpan.FromSeconds(_args.PipeTimeoutSeconds), _cancel.Token))
                {
                    Log.Error("capture: no pipe client connected" +
                              (_sink.Error != null ? ": " + _sink.Error.Message : " (timeout)"));
                    return ExitCodes.PipeBroken;
                }
                Log.Info("capture: video pipe connected");
            }
            _sink.Start();

            if (_args.Audio && _args.DumpPath == null)
            {
                try
                {
                    _audio = new AudioCapture(_args.AudioPipe, _args.AudioDevice, _clock);
                    _audio.Start();
                }
                catch (Exception ex)
                {
                    Log.Warn("capture: audio disabled (" + ex.Message + ")");
                    _audio?.Dispose();
                    _audio = null;
                }
            }

            return Loop(format, dstW, dstH);
        }

        int Loop(CaptureFormat format, int dstW, int dstH)
        {
            if (_duplicator == null || _converter == null || _sink == null || _pool == null) return ExitCodes.DeviceError;

            var pacer = new FramePacer(_args.Fps, Stopwatch.Frequency);
            using (var timer = new HighResTimer())
            {
                _clock.Start();
                pacer.Start(_clock.ElapsedTicks);

                // Start from a black frame so the encoder has valid input even before the first desktop update.
                FrameBuffer? first = _pool.TryRent();
                if (first == null) return ExitCodes.DeviceError;
                ColorMath.FillBlack(first.Data, format, dstW, dstH);
                _latest = first;

                long lastStatsTick = _clock.ElapsedTicks;
                long accessLostSince = 0;
                var gpuWatch = new Stopwatch();
                int exitCode = ExitCodes.Ok;
                bool freshFrame = false;   // a new desktop image was converted since the last emitted frame

                while (!_cancel.IsCancellationRequested)
                {
                    if (_args.ParentPid > 0 && !IsProcessAlive(_args.ParentPid))
                    {
                        Log.Info("capture: parent process " + _args.ParentPid + " is gone, stopping");
                        break;
                    }
                    if (_sink.Broken)
                    {
                        Log.Error("capture: the pipe consumer went away" +
                                  (_sink.Error != null ? ": " + _sink.Error.Message : ""));
                        exitCode = ExitCodes.PipeBroken;
                        break;
                    }
                    if (_args.DumpSeconds > 0 && _clock.Elapsed.TotalSeconds >= _args.DumpSeconds) break;

                    // Pull whatever the desktop has, without blocking past the next frame deadline.
                    long ticksLeft = pacer.TicksUntilNext(_clock.ElapsedTicks);
                    int waitMs = (int)Math.Max(0, Math.Min(8, ticksLeft * 1000 / Stopwatch.Frequency));
                    AcquireResult acquired;
                    try
                    {
                        acquired = _duplicator.TryAcquire(waitMs, _converter);
                    }
                    catch (Exception ex)
                    {
                        Log.Error("capture: acquire failed: " + ex.Message);
                        acquired = AcquireResult.AccessLost;
                    }

                    if (acquired == AcquireResult.AccessLost)
                    {
                        if (accessLostSince == 0)
                        {
                            accessLostSince = _clock.ElapsedTicks;
                            Log.Warn("capture: desktop duplication lost (mode change or full-screen app); retrying");
                        }
                        else if ((_clock.ElapsedTicks - accessLostSince) * 1000 / Stopwatch.Frequency > AccessLostGiveUpMs)
                        {
                            Log.Error("capture: could not recreate the duplication within " +
                                      (AccessLostGiveUpMs / 1000) + "s");
                            exitCode = ExitCodes.AccessLost;
                            break;
                        }
                        try
                        {
                            Thread.Sleep(AccessLostRetryMs);
                            _duplicator.Recreate();
                            accessLostSince = 0;
                            Log.Info("capture: duplication restored");
                        }
                        catch (Exception ex)
                        {
                            Log.Warn("capture: retry failed: " + ex.Message);
                        }
                    }
                    else
                    {
                        accessLostSince = 0;
                        if (_duplicator.PendingCursorBgra != null)
                        {
                            try
                            {
                                _converter.SetCursor(_duplicator.PendingCursorBgra, _duplicator.CursorWidth, _duplicator.CursorHeight);
                            }
                            catch (Exception ex) { Log.Warn("capture: cursor upload failed: " + ex.Message); }
                            _duplicator.ClearPendingCursor();
                        }
                        bool pointerMoved = acquired == AcquireResult.PointerOnly && _args.Cursor;
                        if ((acquired == AcquireResult.NewFrame || pointerMoved) && _converter.HasSource)
                        {
                            FrameBuffer? target = _pool.TryRent();
                            if (target == null)
                            {
                                _dropped++;   // every buffer is still queued: the consumer is behind
                            }
                            else
                            {
                                try
                                {
                                    gpuWatch.Restart();
                                    _converter.Convert(BuildParams(format, dstW, dstH), target.Data);
                                    gpuWatch.Stop();
                                    _gpuMsAccum += gpuWatch.Elapsed.TotalMilliseconds;
                                    _gpuMsCount++;
                                    _latest?.Release();
                                    _latest = target;
                                    _captured++;
                                    freshFrame = true;
                                }
                                catch (Exception ex)
                                {
                                    target.Release();
                                    Log.Error("capture: conversion failed: " + ex.Message);
                                    exitCode = ExitCodes.DeviceError;
                                    break;
                                }
                            }
                        }
                    }

                    // Emit every frame whose deadline has passed (repeating the last image when idle).
                    long due = pacer.FramesDue(_clock.ElapsedTicks);
                    if (due > 0 && _latest != null)
                    {
                        if (due > _args.Fps)
                        {
                            // More than a second behind (suspend/resume, long stall): jump to now instead of catching up.
                            long skipped = pacer.SkipMissed(_clock.ElapsedTicks);
                            _dropped += Math.Max(0, skipped);
                            due = pacer.FramesDue(_clock.ElapsedTicks);
                        }
                        long emitted = 0;
                        for (long i = 0; i < due; i++)
                        {
                            if (!_sink.TryEnqueue(_latest)) { _dropped++; break; }
                            emitted++;
                            // The first frame after a fresh capture carries new pixels; the rest repeat it.
                            if (freshFrame && i == 0) freshFrame = false;
                            else _duplicated++;
                        }
                        pacer.Consume(emitted);
                        freshFrame = false;
                    }

                    if ((_clock.ElapsedTicks - lastStatsTick) >= Stopwatch.Frequency)
                    {
                        lastStatsTick = _clock.ElapsedTicks;
                        EmitStats();
                    }

                    long wait = pacer.TicksUntilNext(_clock.ElapsedTicks);
                    if (wait > 0) timer.WaitUntil(pacer.NextDeadlineTicks);
                }

                EmitStats();
                return exitCode;
            }
        }

        ShaderParams BuildParams(CaptureFormat format, int dstW, int dstH)
        {
            DesktopDuplicator d = _duplicator!;
            uint flags = 0;
            if (d.SourceIsFloat) flags |= ConvertShaderSource.FlagSourceLinear;
            if (_args.Cursor && d.CursorVisible && d.CursorWidth > 0) flags |= ConvertShaderSource.FlagCursorVisible;
            bool exact = d.Rotation == 0 && dstW == d.DesktopWidth && dstH == d.DesktopHeight;
            if (exact) flags |= ConvertShaderSource.FlagExact;
            if (format == CaptureFormat.Nv12 && d.SourceIsFloat) flags |= ConvertShaderSource.FlagSourceIsHdrForcedSdr;

            return new ShaderParams
            {
                SrcWidth = (uint)d.SourceWidth,
                SrcHeight = (uint)d.SourceHeight,
                DeskWidth = (uint)d.DesktopWidth,
                DeskHeight = (uint)d.DesktopHeight,
                DstWidth = (uint)dstW,
                DstHeight = (uint)dstH,
                Rotation = (uint)d.Rotation,
                Flags = flags,
                SdrWhite = _args.SdrWhite,
                CursorX = d.CursorX,
                CursorY = d.CursorY,
                CursorW = (uint)Math.Max(1, d.CursorWidth),
                CursorH = (uint)Math.Max(1, d.CursorHeight),
            };
        }

        CaptureFormat ResolveFormat(DesktopDuplicator d)
        {
            if (_args.Format == CaptureFormat.P010) return CaptureFormat.P010;
            if (_args.Format == CaptureFormat.Nv12) return CaptureFormat.Nv12;
            return d.Info.Hdr && d.SourceIsFloat ? CaptureFormat.P010 : CaptureFormat.Nv12;
        }

        void EmitStats()
        {
            var stats = new CaptureStats
            {
                T = _clock.Elapsed.TotalSeconds,
                Captured = _captured,
                Duplicated = _duplicated,
                Dropped = _dropped,
                Fps = _clock.Elapsed.TotalSeconds > 0 ? (_sink?.FramesWritten ?? 0) / _clock.Elapsed.TotalSeconds : 0,
                GpuMs = _gpuMsCount > 0 ? _gpuMsAccum / _gpuMsCount : 0,
                AudioUnderrun = _audio?.Underruns ?? 0,
                PipeQueueMs = (_sink?.QueuedFrames ?? 0) * (1000.0 / Math.Max(1, _args.Fps)),
            };
            Console.Out.WriteLine(stats.ToJsonLine());
            Console.Out.Flush();
            _gpuMsAccum = 0;
            _gpuMsCount = 0;
        }

        static bool IsProcessAlive(int pid)
        {
            try
            {
                using (Process p = Process.GetProcessById(pid)) return !p.HasExited;
            }
            catch (Exception) { return false; }
        }

        static int Even(int v) => v < 2 ? 2 : (v & 1) == 0 ? v : v - 1;

        public void Dispose()
        {
            try { _cancel.Cancel(); } catch (Exception) { }
            _audio?.Dispose();
            _sink?.Dispose();
            _latest?.Release();
            _latest = null;
            _converter?.Dispose();
            _duplicator?.Dispose();
            _cancel.Dispose();
        }
    }
}
