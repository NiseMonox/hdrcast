using System;

namespace HdrCast.Capture
{
    /// <summary>
    /// Constant-frame-rate scheduler (design.md §2.2). Deadlines are computed from the start tick with
    /// integer arithmetic (deadline_n = start + n * ticksPerSecond / fps) so there is no accumulated drift.
    /// Pure logic; the caller supplies the clock (Stopwatch ticks / QueryPerformanceCounter).
    /// </summary>
    public sealed class FramePacer
    {
        readonly long _ticksPerSecond;
        readonly int _fps;
        long _start;
        long _nextFrame;   // index of the next frame whose deadline has not been consumed yet

        public FramePacer(int fps, long ticksPerSecond)
        {
            if (fps <= 0) throw new ArgumentOutOfRangeException(nameof(fps));
            if (ticksPerSecond <= 0) throw new ArgumentOutOfRangeException(nameof(ticksPerSecond));
            _fps = fps;
            _ticksPerSecond = ticksPerSecond;
        }

        public int Fps => _fps;
        public long TicksPerSecond => _ticksPerSecond;
        /// <summary>Nominal frame interval in ticks (rounded).</summary>
        public long FrameIntervalTicks => (_ticksPerSecond + _fps / 2) / _fps;
        public double FrameIntervalMs => 1000.0 / _fps;
        /// <summary>Index of the next frame to be emitted (0-based). Equals the number of frames consumed so far.</summary>
        public long NextFrameIndex => _nextFrame;

        /// <summary>Starts the timeline; frame 0 is due immediately at <paramref name="nowTicks"/>.</summary>
        public void Start(long nowTicks)
        {
            _start = nowTicks;
            _nextFrame = 0;
        }

        /// <summary>Absolute tick at which frame <paramref name="frameIndex"/> is due.</summary>
        public long DeadlineTicks(long frameIndex)
        {
            if (frameIndex < 0) throw new ArgumentOutOfRangeException(nameof(frameIndex));
            // (frameIndex * ticksPerSecond) fits in a long for any realistic session (10 MHz * 60 fps * 24 h ~ 5e13).
            return _start + frameIndex * _ticksPerSecond / _fps;
        }

        /// <summary>Deadline of the next frame that has not been consumed.</summary>
        public long NextDeadlineTicks => DeadlineTicks(_nextFrame);

        /// <summary>
        /// Number of frames whose deadline is at or before <paramref name="nowTicks"/> and that have not been consumed.
        /// 0 means "not yet time"; 1 is the normal case; more than 1 means the loop fell behind.
        /// Does not consume anything.
        /// </summary>
        public long FramesDue(long nowTicks)
        {
            long elapsed = nowTicks - _start;
            if (elapsed < 0) return 0;
            // Largest n with DeadlineTicks(n) <= now. DeadlineTicks truncates (n*tps/fps), so the inverse must
            // use the same rounding: floor(n*tps/fps) <= elapsed  <=>  n <= ((elapsed+1)*fps - 1)/tps.
            // Deriving it from elapsed*fps/tps instead would miss the frame that is due on its exact deadline tick.
            long lastDue = MulDiv(elapsed + 1, _fps, _ticksPerSecond, -1);
            long due = lastDue + 1 - _nextFrame;
            return due < 0 ? 0 : due;
        }

        /// <summary>Marks <paramref name="count"/> frames as emitted.</summary>
        public void Consume(long count)
        {
            if (count < 0) throw new ArgumentOutOfRangeException(nameof(count));
            _nextFrame += count;
        }

        /// <summary>
        /// Skips frames so that the next deadline is in the future (used after a long stall when the
        /// caller decided to drop rather than catch up). Returns the number of frames skipped.
        /// </summary>
        public long SkipMissed(long nowTicks)
        {
            long due = FramesDue(nowTicks);
            if (due <= 1) return 0;
            long skipped = due - 1;
            _nextFrame += skipped;
            return skipped;
        }

        /// <summary>Ticks to wait from <paramref name="nowTicks"/> until the next deadline (0 if already due).</summary>
        public long TicksUntilNext(long nowTicks)
        {
            long wait = NextDeadlineTicks - nowTicks;
            return wait < 0 ? 0 : wait;
        }

        /// <summary>(a * b + bias) / c with the multiplication kept exact.</summary>
        static long MulDiv(long a, long b, long c, long bias = 0)
        {
            // a * b may overflow a long only after ~30 years at 10 MHz; use decimal to be safe on the hot path anyway.
            if (a < 1L << 40) return (a * b + bias) / c;
            return (long)(((decimal)a * b + bias) / c);
        }
    }
}
