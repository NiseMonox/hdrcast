using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.IO.Pipes;
using System.Threading;

namespace HdrCast.Sender.Capture
{
    /// <summary>
    /// Writes raw frames to the video named pipe (server side, design.md §2.2) or to a dump file on its own thread.
    /// The capture loop enqueues reference-counted frames; when the consumer (ffmpeg) falls behind the queue fills up
    /// and the loop drops frames instead of stalling the GPU/pacer.
    /// </summary>
    internal sealed class FrameSink : IDisposable
    {
        readonly BlockingCollection<FrameBuffer> _queue;
        readonly Stream _stream;
        readonly NamedPipeServerStream? _pipe;
        readonly Thread _thread;
        readonly int _capacity;
        readonly object _gate = new object();
        int _queued;
        long _written;
        volatile bool _broken;
        volatile Exception? _error;
        volatile bool _closing;

        FrameSink(Stream stream, NamedPipeServerStream? pipe, int queueCapacity)
        {
            _stream = stream;
            _pipe = pipe;
            _capacity = queueCapacity;
            _queue = new BlockingCollection<FrameBuffer>(new ConcurrentQueue<FrameBuffer>());
            _thread = new Thread(WriterLoop) { IsBackground = true, Name = "video-pipe-writer" };
        }

        public static FrameSink CreatePipe(string name, int outBufferBytes, int queueCapacity)
        {
            // Asynchronous option is needed so BeginWaitForConnection can be abandoned on timeout; Write() still blocks.
            var pipe = new NamedPipeServerStream(name, PipeDirection.Out, 1, PipeTransmissionMode.Byte,
                PipeOptions.Asynchronous, 0, outBufferBytes);
            return new FrameSink(pipe, pipe, queueCapacity);
        }

        public static FrameSink CreateFile(string path, int queueCapacity)
        {
            var fs = new FileStream(path, FileMode.Create, FileAccess.Write, FileShare.Read, 1 << 20, FileOptions.None);
            return new FrameSink(fs, null, queueCapacity);
        }

        public bool IsPipe => _pipe != null;
        public bool Broken => _broken;
        public Exception? Error => _error;
        public long FramesWritten => Interlocked.Read(ref _written);
        public int QueuedFrames => Volatile.Read(ref _queued);

        /// <summary>Waits for the pipe client; returns true when connected (always true for files).</summary>
        public bool WaitForClient(TimeSpan timeout, CancellationToken cancel)
        {
            if (_pipe == null) return true;
            IAsyncResult ar = _pipe.BeginWaitForConnection(null, null);
            var handles = new WaitHandle[] { ar.AsyncWaitHandle, cancel.WaitHandle };
            int idx = WaitHandle.WaitAny(handles, timeout);
            if (idx == 0)
            {
                try { _pipe.EndWaitForConnection(ar); }
                catch (Exception ex) { _error = ex; _broken = true; return false; }
                return true;
            }
            // Timed out or cancelled: dispose the pipe so the pending overlapped wait is completed and its
            // handle released, then drain EndWaitForConnection to observe the cancellation.
            try { _pipe.Dispose(); } catch (Exception) { }
            try { _pipe.EndWaitForConnection(ar); } catch (Exception) { }
            _broken = true;
            return false;
        }

        public void Start() => _thread.Start();

        /// <summary>Queues a frame (takes its own reference). Returns false when the queue is full or the sink is broken.</summary>
        public bool TryEnqueue(FrameBuffer frame)
        {
            if (_broken || _closing) return false;
            lock (_gate)
            {
                if (_queued >= _capacity) return false;
                _queued++;
            }
            frame.AddRef();
            frame.EnqueuedTicks = Stopwatch.GetTimestamp();
            _queue.Add(frame);
            return true;
        }

        void WriterLoop()
        {
            try
            {
                foreach (FrameBuffer f in _queue.GetConsumingEnumerable())
                {
                    try
                    {
                        if (!_broken)
                        {
                            _stream.Write(f.Data, 0, f.Length);
                            Interlocked.Increment(ref _written);
                        }
                    }
                    catch (Exception ex)
                    {
                        // IOException "pipe is being closed" / "broken pipe" = consumer went away (exit code 4).
                        _error = ex;
                        _broken = true;
                    }
                    finally
                    {
                        lock (_gate) { _queued--; }
                        f.Release();
                    }
                }
                if (!_broken)
                {
                    try { _stream.Flush(); } catch (Exception) { }
                }
            }
            catch (Exception ex)
            {
                _error = ex;
                _broken = true;
            }
        }

        /// <summary>Stops accepting frames, drains the queue (bounded wait) and closes the stream.</summary>
        public void Dispose()
        {
            _closing = true;
            try { _queue.CompleteAdding(); } catch (Exception) { }
            if (_thread.IsAlive)
            {
                if (!_thread.Join(3000))
                {
                    _broken = true;
                    try { _stream.Dispose(); } catch (Exception) { }
                    _thread.Join(1000);
                }
            }
            try { _stream.Dispose(); } catch (Exception) { }
        }
    }
}
