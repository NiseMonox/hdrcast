using System;
using System.Threading;

namespace HdrCast.Sender.Capture
{
    /// <summary>A raw video frame with a reference count: the queue and the "latest frame" slot can share one buffer.</summary>
    internal sealed class FrameBuffer
    {
        readonly FramePool _pool;
        int _refs;

        public byte[] Data { get; }
        public int Length { get; }
        /// <summary>Stopwatch tick when the frame was enqueued to the sink (for queue-age stats).</summary>
        public long EnqueuedTicks;

        internal FrameBuffer(FramePool pool, int length)
        {
            _pool = pool;
            Length = length;
            Data = new byte[length];
        }

        public bool InUse => Volatile.Read(ref _refs) > 0;

        internal void Rent() => Interlocked.Exchange(ref _refs, 1);
        public void AddRef() => Interlocked.Increment(ref _refs);
        public void Release()
        {
            if (Interlocked.Decrement(ref _refs) == 0) _pool.Return(this);
        }
    }

    /// <summary>Fixed set of pre-allocated frame buffers (no per-frame GC churn at 25 MB per 4K P010 frame).</summary>
    internal sealed class FramePool
    {
        readonly FrameBuffer[] _buffers;
        readonly object _gate = new object();
        readonly bool[] _free;

        public FramePool(int count, int frameBytes)
        {
            if (count < 2) throw new ArgumentOutOfRangeException(nameof(count));
            _buffers = new FrameBuffer[count];
            _free = new bool[count];
            for (int i = 0; i < count; i++)
            {
                _buffers[i] = new FrameBuffer(this, frameBytes);
                _free[i] = true;
            }
        }

        public int Count => _buffers.Length;
        public int FrameBytes => _buffers[0].Length;

        /// <summary>Returns a buffer with reference count 1, or null when every buffer is in use.</summary>
        public FrameBuffer? TryRent()
        {
            lock (_gate)
            {
                for (int i = 0; i < _buffers.Length; i++)
                {
                    if (_free[i])
                    {
                        _free[i] = false;
                        _buffers[i].Rent();
                        return _buffers[i];
                    }
                }
            }
            return null;
        }

        internal void Return(FrameBuffer b)
        {
            lock (_gate)
            {
                int idx = Array.IndexOf(_buffers, b);
                if (idx >= 0) _free[idx] = true;
            }
        }
    }
}
