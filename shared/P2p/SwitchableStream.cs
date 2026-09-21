using System;
using System.IO;
using System.Threading;

namespace HdrCast.P2p
{
    /// <summary>
    /// A read-only stream that reads from whichever source is attached. When the current source ends (the
    /// encoder exited) it waits for the next one (the restarted encoder), so the fan-out and every viewer's
    /// session survive an encoder restart. Read returns 0 only after <see cref="Dispose(bool)"/>.
    /// </summary>
    public sealed class SwitchableStream : Stream
    {
        readonly object _lock = new object();
        Stream? _current;
        bool _closed;

        public void Attach(Stream source)
        {
            lock (_lock)
            {
                if (_closed) return;
                _current = source;
                Monitor.PulseAll(_lock);
            }
        }

        public override int Read(byte[] buffer, int offset, int count)
        {
            while (true)
            {
                Stream? s;
                lock (_lock)
                {
                    while (_current == null && !_closed) Monitor.Wait(_lock);
                    if (_closed) return 0;
                    s = _current!;
                }
                int n;
                try
                {
                    n = s.Read(buffer, offset, count);
                }
                catch (Exception ex) when (ex is IOException || ex is ObjectDisposedException || ex is InvalidOperationException)
                {
                    n = 0;
                }
                if (n > 0) return n;
                lock (_lock)
                    if (_current == s) _current = null;       // wait for the next source
            }
        }

        protected override void Dispose(bool disposing)
        {
            lock (_lock)
            {
                _closed = true;
                _current = null;
                Monitor.PulseAll(_lock);
            }
            base.Dispose(disposing);
        }

        public override bool CanRead => true;
        public override bool CanSeek => false;
        public override bool CanWrite => false;
        public override long Length => throw new NotSupportedException();
        public override long Position { get => throw new NotSupportedException(); set => throw new NotSupportedException(); }
        public override void Flush() { }
        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();
        public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();
    }
}
