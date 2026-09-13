using System;
using System.IO;
using System.IO.Pipes;
using System.Text;
using System.Threading;

namespace HdrCast.Receiver
{
    /// <summary>
    /// JSON IPC client for a running mpv (--input-ipc-server=\\.\pipe\&lt;name&gt;). Connects in the background
    /// (retrying until mpv has created the pipe or <c>keepTrying</c> says stop), observes the status
    /// properties, raises one event per received message and offers thread-safe command sending.
    /// </summary>
    internal sealed class MpvIpcClient : IDisposable
    {
        private readonly string _pipeName;
        private readonly object _writeGate = new object();
        private NamedPipeClientStream? _pipe;
        private StreamWriter? _writer;
        private Thread? _thread;
        private volatile bool _disposed;
        private volatile bool _connected;
        private long _requestId;

        public MpvIpcClient(string pipeName)
        {
            _pipeName = pipeName ?? throw new ArgumentNullException(nameof(pipeName));
        }

        public event Action<MpvIpcMessage>? MessageReceived;
        public event Action? Connected;
        public event Action? Disconnected;
        public event Action<string>? Log;

        public bool IsConnected => _connected;

        /// <summary>Starts the connect/read loop. <paramref name="keepTrying"/> is polled between connect attempts.</summary>
        public void Start(Func<bool> keepTrying)
        {
            if (_thread != null) throw new InvalidOperationException("already started");
            _thread = new Thread(() => Run(keepTrying)) { IsBackground = true, Name = "mpv-ipc" };
            _thread.Start();
        }

        private void Run(Func<bool> keepTrying)
        {
            NamedPipeClientStream? pipe = null;
            while (!_disposed)
            {
                if (!keepTrying())
                {
                    Log?.Invoke("IPC: giving up (mpv is not running)");
                    return;
                }
                try
                {
                    pipe = new NamedPipeClientStream(".", _pipeName, PipeDirection.InOut, PipeOptions.Asynchronous);
                    pipe.Connect(500);
                    break;
                }
                catch (TimeoutException)
                {
                    pipe?.Dispose();
                    pipe = null;
                }
                catch (IOException ex)
                {
                    pipe?.Dispose();
                    pipe = null;
                    Log?.Invoke("IPC: connect failed: " + ex.Message);
                    Thread.Sleep(500);
                }
                catch (Exception ex)
                {
                    pipe?.Dispose();
                    pipe = null;
                    Log?.Invoke("IPC: connect error: " + ex.Message);
                    Thread.Sleep(500);
                }
            }
            if (pipe == null || _disposed)
            {
                pipe?.Dispose();
                return;
            }

            _pipe = pipe;
            var utf8 = new UTF8Encoding(false);
            _writer = new StreamWriter(pipe, utf8) { AutoFlush = true, NewLine = "\n" };
            _connected = true;
            Log?.Invoke("IPC: connected to " + _pipeName);
            Connected?.Invoke();

            foreach (string cmd in MpvObservedProperties.ObserveCommands()) Send(cmd);

            try
            {
                using (var reader = new StreamReader(pipe, utf8, false, 16384, leaveOpen: true))
                {
                    string? line;
                    while (!_disposed && (line = reader.ReadLine()) != null)
                    {
                        var msg = MpvIpcMessage.TryParseLine(line);
                        if (msg == null) continue;
                        try
                        {
                            MessageReceived?.Invoke(msg);
                        }
                        catch (Exception ex)
                        {
                            Log?.Invoke("IPC: handler error: " + ex.Message);
                        }
                    }
                }
            }
            catch (Exception ex)
            {
                if (!_disposed) Log?.Invoke("IPC: read loop ended: " + ex.Message);
            }
            finally
            {
                _connected = false;
                Disconnected?.Invoke();
            }
        }

        /// <summary>Sends one JSON line; returns false when not connected or the write failed.</summary>
        public bool Send(string jsonLine)
        {
            var w = _writer;
            if (w == null || !_connected) return false;
            try
            {
                lock (_writeGate)
                {
                    w.Write(jsonLine);
                    w.Write('\n');
                    w.Flush();
                }
                return true;
            }
            catch (Exception ex)
            {
                Log?.Invoke("IPC: send failed: " + ex.Message);
                return false;
            }
        }

        public long NextRequestId() => Interlocked.Increment(ref _requestId);

        public bool SetProperty(string name, object? value) => Send(MpvIpcCommands.SetProperty(name, value, NextRequestId()));
        public bool CycleProperty(string name) => Send(MpvIpcCommands.CycleProperty(name, NextRequestId()));
        public bool Quit() => Send(MpvIpcCommands.Quit(NextRequestId()));

        public void Dispose()
        {
            if (_disposed) return;
            _disposed = true;
            _connected = false;
            try { _pipe?.Dispose(); } catch (Exception) { }
        }
    }
}
