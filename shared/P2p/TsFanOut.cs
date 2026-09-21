using System;
using System.Collections.Generic;
using System.IO;
using System.Threading;

namespace HdrCast.P2p
{
    /// <summary>
    /// Copies one MPEG-TS stream (the encoder's output) to many sinks (each viewer's remux child). Every sink has
    /// its own bounded queue and writer thread, so a viewer whose child stalls — still waiting for its SRT
    /// connection, or on a slow path — only loses its own data: when its queue overflows it is cleared and that
    /// viewer's decoder recovers at the next keyframe. (Tested: a plain `tee` stalls every viewer at once.)
    /// </summary>
    public sealed class TsFanOut : IDisposable
    {
        public const int ReadChunkBytes = 188 * 7 * 32;
        public const long MinQueueBytes = 512 * 1024;

        sealed class Sink
        {
            public Sink(string id, Stream output, long maxBytes)
            {
                Id = id; Output = output; MaxBytes = maxBytes;
            }
            public readonly string Id;
            public readonly Stream Output;
            public readonly long MaxBytes;
            public readonly Queue<byte[]> Queue = new Queue<byte[]>();
            public long QueuedBytes;
            public long Written;
            public int Clears;
            public volatile bool Closed;
            public Thread? Writer;
        }

        readonly Stream _input;
        readonly Dictionary<string, Sink> _sinks = new Dictionary<string, Sink>();
        readonly Thread _reader;
        volatile bool _disposed;
        long _bytesIn;

        public TsFanOut(Stream input)
        {
            _input = input ?? throw new ArgumentNullException(nameof(input));
            _reader = new Thread(ReadLoop) { IsBackground = true, Name = "ts-fanout" };
        }

        /// <summary>The encoder's stream ended (it exited or the pipe broke).</summary>
        public event Action? InputEnded;
        /// <summary>A sink could not be written any more (its child exited); the sink is removed.</summary>
        public event Action<string>? SinkFailed;

        public long BytesIn => Interlocked.Read(ref _bytesIn);

        public void Start() => _reader.Start();

        /// <summary>Queue size for a stream of <paramref name="bitsPerSecond"/>: half a second, at least 512 KB.</summary>
        public static long QueueBytesFor(long bitsPerSecond) => Math.Max(MinQueueBytes, bitsPerSecond / 8 / 2);

        public void AddSink(string id, Stream output, long maxQueueBytes)
        {
            var sink = new Sink(id, output, Math.Max(64 * 1024, maxQueueBytes));
            sink.Writer = new Thread(() => WriteLoop(sink)) { IsBackground = true, Name = "ts-sink-" + id };
            Sink? old;
            lock (_sinks)
            {
                _sinks.TryGetValue(id, out old);
                _sinks[id] = sink;
            }
            if (old != null) Close(old);
            sink.Writer.Start();
        }

        public void RemoveSink(string id)
        {
            Sink? s;
            lock (_sinks)
            {
                if (!_sinks.TryGetValue(id, out s)) return;
                _sinks.Remove(id);
            }
            Close(s);
        }

        /// <summary>(written bytes, times the queue overflowed and was cleared) for a sink, or null.</summary>
        public (long written, int clears)? Stats(string id)
        {
            lock (_sinks)
            {
                if (!_sinks.TryGetValue(id, out Sink? s)) return null;
                lock (s.Queue) return (Interlocked.Read(ref s.Written), s.Clears);
            }
        }

        public int SinkCount
        {
            get { lock (_sinks) return _sinks.Count; }
        }

        void ReadLoop()
        {
            var buf = new byte[ReadChunkBytes];
            try
            {
                while (!_disposed)
                {
                    int n = _input.Read(buf, 0, buf.Length);
                    if (n <= 0) break;
                    Interlocked.Add(ref _bytesIn, n);
                    var chunk = new byte[n];
                    Buffer.BlockCopy(buf, 0, chunk, 0, n);
                    Sink[] sinks;
                    lock (_sinks)
                    {
                        sinks = new Sink[_sinks.Count];
                        _sinks.Values.CopyTo(sinks, 0);
                    }
                    foreach (Sink s in sinks) Enqueue(s, chunk);
                }
            }
            catch (IOException) { }
            catch (ObjectDisposedException) { }
            if (!_disposed) InputEnded?.Invoke();
        }

        static void Enqueue(Sink s, byte[] chunk)
        {
            lock (s.Queue)
            {
                if (s.Closed) return;
                if (s.QueuedBytes + chunk.Length > s.MaxBytes)
                {
                    s.Queue.Clear();
                    s.QueuedBytes = 0;
                    s.Clears++;
                }
                s.Queue.Enqueue(chunk);
                s.QueuedBytes += chunk.Length;
                Monitor.Pulse(s.Queue);
            }
        }

        void WriteLoop(Sink s)
        {
            while (true)
            {
                byte[] chunk;
                lock (s.Queue)
                {
                    while (s.Queue.Count == 0 && !s.Closed) Monitor.Wait(s.Queue);
                    if (s.Closed) return;
                    chunk = s.Queue.Dequeue();
                    s.QueuedBytes -= chunk.Length;
                }
                try
                {
                    s.Output.Write(chunk, 0, chunk.Length);
                    s.Output.Flush();
                    Interlocked.Add(ref s.Written, chunk.Length);
                }
                catch (Exception ex) when (ex is IOException || ex is ObjectDisposedException || ex is InvalidOperationException)
                {
                    bool mine;
                    lock (_sinks)
                    {
                        mine = _sinks.TryGetValue(s.Id, out Sink? current) && current == s;
                        if (mine) _sinks.Remove(s.Id);
                    }
                    Close(s);
                    if (mine && !_disposed) SinkFailed?.Invoke(s.Id);
                    return;
                }
            }
        }

        static void Close(Sink s)
        {
            lock (s.Queue)
            {
                s.Closed = true;
                s.Queue.Clear();
                s.QueuedBytes = 0;
                Monitor.PulseAll(s.Queue);
            }
        }

        public void Dispose()
        {
            if (_disposed) return;
            _disposed = true;
            Sink[] sinks;
            lock (_sinks)
            {
                sinks = new Sink[_sinks.Count];
                _sinks.Values.CopyTo(sinks, 0);
                _sinks.Clear();
            }
            foreach (Sink s in sinks) Close(s);
            try { _input.Dispose(); } catch (Exception) { }
        }
    }
}
