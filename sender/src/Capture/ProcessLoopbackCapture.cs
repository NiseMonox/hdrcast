using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Threading;
using HdrCast.Capture;
using NAudio.CoreAudioApi;
using NAudio.CoreAudioApi.Interfaces;
using NAudio.Wave;

namespace HdrCast.Sender.Capture
{
    /// <summary>
    /// Desktop audio without one process tree (design.md §2.4): Windows' process-loopback capture, the API behind
    /// Parsec's echo cancelling and OBS's application audio capture (Windows 10 21H1+ / 11). The virtual endpoint has
    /// no mix format of its own, so the stream is requested as float 48 kHz stereo and the audio engine converts.
    /// Everything COM happens on one MTA thread: the capture engine's entry point is [STAThread], and the activation
    /// callback would otherwise need a message pump.
    /// </summary>
    internal sealed class ProcessLoopbackCapture : IWaveIn
    {
        const string VirtualDevice = @"VAD\Process_Loopback";
        static readonly Guid IidAudioClient = new Guid("1CB9AD4C-DBFA-4c32-B178-C2F568A703B2");

        readonly int _targetPid;
        readonly bool _includeTree;
        Thread? _thread;
        volatile bool _running;

        /// <param name="includeTree">true = only that process tree; false = everything except it.</param>
        public ProcessLoopbackCapture(int targetPid, bool includeTree)
        {
            _targetPid = targetPid;
            _includeTree = includeTree;
        }

        public WaveFormat WaveFormat { get; set; } = WaveFormat.CreateIeeeFloatWaveFormat(48000, 2);
        public int TargetPid => _targetPid;

        public event EventHandler<WaveInEventArgs>? DataAvailable;
        public event EventHandler<StoppedEventArgs>? RecordingStopped;

        /// <summary>Activates and starts the stream; throws when this Windows cannot (then use plain loopback).</summary>
        public void StartRecording()
        {
            if (_thread != null) throw new InvalidOperationException("already recording");
            var started = new ManualResetEventSlim(false);
            Exception? startError = null;
            _running = true;
            _thread = new Thread(() => Run(started, ex => startError = ex)) { IsBackground = true, Name = "process-loopback" };
            _thread.SetApartmentState(ApartmentState.MTA);
            _thread.Start();
            if (!started.Wait(8000)) startError = new TimeoutException("process loopback did not start");
            if (startError != null)
            {
                _running = false;
                _thread.Join(2000);
                throw startError;
            }
        }

        public void StopRecording()
        {
            _running = false;
            Thread? t = _thread;
            if (t != null && t != Thread.CurrentThread) t.Join(2000);
        }

        public void Dispose() => StopRecording();

        void Run(ManualResetEventSlim started, Action<Exception> failedToStart)
        {
            AudioClient? client = null;
            Exception? error = null;
            bool running = false;
            try
            {
                using (var ready = new AutoResetEvent(false))
                {
                    client = Activate(_targetPid, _includeTree);
                    client.Initialize(AudioClientShareMode.Shared,
                        AudioClientStreamFlags.Loopback | AudioClientStreamFlags.EventCallback |
                        AudioClientStreamFlags.AutoConvertPcm | AudioClientStreamFlags.SrcDefaultQuality,
                        1_000_000, 0, WaveFormat, Guid.Empty);                // 100 ms shared buffer
                    client.SetEventHandle(ready.SafeWaitHandle.DangerousGetHandle());
                    AudioCaptureClient capture = client.AudioCaptureClient;
                    client.Start();
                    running = true;
                    started.Set();
                    Pump(capture, ready);
                    client.Stop();
                }
            }
            catch (Exception ex)
            {
                if (!running) failedToStart(ex);
                else error = ex;
            }
            finally
            {
                try { client?.Dispose(); } catch (Exception) { }
                started.Set();
                if (running) RecordingStopped?.Invoke(this, new StoppedEventArgs(error));
            }
        }

        void Pump(AudioCaptureClient capture, WaitHandle ready)
        {
            int blockAlign = WaveFormat.BlockAlign;
            var buffer = new byte[WaveFormat.AverageBytesPerSecond / 10];
            while (_running)
            {
                ready.WaitOne(100);
                int frames = capture.GetNextPacketSize();
                while (_running && frames > 0)
                {
                    IntPtr data = capture.GetBuffer(out int count, out AudioClientBufferFlags flags);
                    int bytes = count * blockAlign;
                    if (buffer.Length < bytes) buffer = new byte[bytes];
                    if ((flags & AudioClientBufferFlags.Silent) != 0) Array.Clear(buffer, 0, bytes);
                    else Marshal.Copy(data, buffer, 0, bytes);
                    capture.ReleaseBuffer(count);
                    if (bytes > 0) DataAvailable?.Invoke(this, new WaveInEventArgs(buffer, bytes));   // consumed synchronously
                    frames = capture.GetNextPacketSize();
                }
            }
        }

        // ---------------------------------------------------------------- activation (Windows SDK audioclientactivationparams.h)

        [StructLayout(LayoutKind.Sequential)]
        struct ActivationParams
        {
            public int ActivationType;          // AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK = 1
            public int TargetProcessId;
            public int ProcessLoopbackMode;     // 0 = include the tree, 1 = exclude it
        }

        [StructLayout(LayoutKind.Sequential)]
        struct PropVariantBlob
        {
            public ushort Vt;                   // VT_BLOB = 65
            public ushort Reserved1, Reserved2, Reserved3;
            public int CbSize;
            public IntPtr PBlobData;
        }

        [DllImport("Mmdevapi.dll", ExactSpelling = true, PreserveSig = true)]
        static extern int ActivateAudioInterfaceAsync([MarshalAs(UnmanagedType.LPWStr)] string deviceInterfacePath,
                                                      [MarshalAs(UnmanagedType.LPStruct)] Guid riid, IntPtr activationParams,
                                                      IActivateAudioInterfaceCompletionHandler completionHandler,
                                                      out IActivateAudioInterfaceAsyncOperation activationOperation);

        static AudioClient Activate(int targetPid, bool includeTree)
        {
            var p = new ActivationParams { ActivationType = 1, TargetProcessId = targetPid, ProcessLoopbackMode = includeTree ? 0 : 1 };
            int paramsSize = Marshal.SizeOf(typeof(ActivationParams));
            IntPtr paramsPtr = Marshal.AllocHGlobal(paramsSize);
            IntPtr propPtr = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(PropVariantBlob)));
            try
            {
                Marshal.StructureToPtr(p, paramsPtr, false);
                Marshal.StructureToPtr(new PropVariantBlob { Vt = 65, CbSize = paramsSize, PBlobData = paramsPtr }, propPtr, false);
                var handler = new AudioActivationHandler();
                int hr = ActivateAudioInterfaceAsync(VirtualDevice, IidAudioClient, propPtr, handler, out IActivateAudioInterfaceAsyncOperation operation);
                Marshal.ThrowExceptionForHR(hr);
                // The blob has to stay valid until the activation completes, so wait here before freeing it.
                if (!handler.Done.Wait(5000)) throw new TimeoutException("process loopback activation did not complete");
                GC.KeepAlive(operation);
                Marshal.ThrowExceptionForHR(handler.Result);
                if (!(handler.Interface is IAudioClient audioClient)) throw new InvalidOperationException("process loopback returned no audio client");
                return new AudioClient(audioClient);
            }
            finally
            {
                Marshal.FreeHGlobal(propPtr);
                Marshal.FreeHGlobal(paramsPtr);
            }
        }
    }

    // The COM types of ActivateAudioInterfaceAsync. Public and COM-visible on purpose: Windows calls back into the
    // handler and queries it for these interfaces, which the CLR only answers for types visible to COM.

    [ComImport, Guid("72A22D78-CDE4-431D-B8CC-843A71199B6D"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IActivateAudioInterfaceAsyncOperation
    {
        void GetActivateResult(out int activateResult, [MarshalAs(UnmanagedType.IUnknown)] out object activatedInterface);
    }

    [ComImport, Guid("41D949AB-9862-444A-80F6-C261334DA5EB"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IActivateAudioInterfaceCompletionHandler
    {
        void ActivateCompleted(IActivateAudioInterfaceAsyncOperation activateOperation);
    }

    /// <summary>Marker: the completion handler must be agile, or the callback waits for an apartment nobody pumps.</summary>
    [ComImport, Guid("94EA2B94-E9CC-49E0-C0FF-EE64CA8F5B90"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IAgileObject
    {
    }

    [ComVisible(true), ClassInterface(ClassInterfaceType.None)]
    public sealed class AudioActivationHandler : IActivateAudioInterfaceCompletionHandler, IAgileObject
    {
        public readonly ManualResetEventSlim Done = new ManualResetEventSlim(false);
        public int Result = unchecked((int)0x80004005);
        public object? Interface;

        public void ActivateCompleted(IActivateAudioInterfaceAsyncOperation activateOperation)
        {
            try
            {
                activateOperation.GetActivateResult(out Result, out Interface);
            }
            catch (Exception ex)
            {
                Result = Marshal.GetHRForException(ex);
            }
            finally
            {
                Done.Set();
            }
        }
    }

    /// <summary>Process id, parent id and exe name of every running process (Toolhelp32: fast, no WMI).</summary>
    internal static class ProcessSnapshot
    {
        const uint Th32csSnapProcess = 0x00000002;

        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        struct ProcessEntry32W
        {
            public uint DwSize;
            public uint CntUsage;
            public uint Th32ProcessId;
            public IntPtr Th32DefaultHeapId;
            public uint Th32ModuleId;
            public uint CntThreads;
            public uint Th32ParentProcessId;
            public int PcPriClassBase;
            public uint DwFlags;
            [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)] public string SzExeFile;
        }

        [DllImport("kernel32.dll", SetLastError = true)]
        static extern IntPtr CreateToolhelp32Snapshot(uint flags, uint processId);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern bool Process32FirstW(IntPtr snapshot, ref ProcessEntry32W entry);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern bool Process32NextW(IntPtr snapshot, ref ProcessEntry32W entry);

        [DllImport("kernel32.dll", SetLastError = true)]
        static extern bool CloseHandle(IntPtr handle);

        public static List<ProcessEntry> Take()
        {
            var list = new List<ProcessEntry>();
            IntPtr snap = CreateToolhelp32Snapshot(Th32csSnapProcess, 0);
            if (snap == IntPtr.Zero || snap == new IntPtr(-1)) return list;
            try
            {
                var e = new ProcessEntry32W { DwSize = (uint)Marshal.SizeOf(typeof(ProcessEntry32W)) };
                for (bool ok = Process32FirstW(snap, ref e); ok; ok = Process32NextW(snap, ref e))
                    list.Add(new ProcessEntry((int)e.Th32ProcessId, (int)e.Th32ParentProcessId, e.SzExeFile ?? ""));
            }
            finally
            {
                CloseHandle(snap);
            }
            return list;
        }
    }
}
