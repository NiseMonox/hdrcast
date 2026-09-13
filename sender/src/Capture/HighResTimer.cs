using System;
using System.Diagnostics;
using System.Threading;

namespace HdrCast.Sender.Capture
{
    /// <summary>
    /// Sleeps until an absolute Stopwatch (QueryPerformanceCounter) deadline with sub-millisecond accuracy:
    /// a high-resolution waitable timer (Windows 10 1803+) when available, otherwise Sleep + SpinWait.
    /// timeBeginPeriod(1) is held for the lifetime of the object so the fallback and pipe waits stay tight.
    /// </summary>
    internal sealed class HighResTimer : IDisposable
    {
        IntPtr _timer;
        bool _periodSet;
        readonly long _ticksPerMs = Stopwatch.Frequency / 1000;

        public bool HighResolution => _timer != IntPtr.Zero;

        public HighResTimer()
        {
            try
            {
                _periodSet = NativeMethods.timeBeginPeriod(1) == 0;
            }
            catch (Exception) { _periodSet = false; }
            try
            {
                _timer = NativeMethods.CreateWaitableTimerExW(IntPtr.Zero, null,
                    NativeMethods.CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, NativeMethods.TIMER_ALL_ACCESS);
                if (_timer == NativeMethods.INVALID_HANDLE_VALUE) _timer = IntPtr.Zero;
            }
            catch (Exception) { _timer = IntPtr.Zero; }
        }

        /// <summary>Blocks until Stopwatch.GetTimestamp() >= deadlineTicks (returns immediately if already past).</summary>
        public void WaitUntil(long deadlineTicks)
        {
            long now = Stopwatch.GetTimestamp();
            long remaining = deadlineTicks - now;
            if (remaining <= 0) return;

            if (_timer != IntPtr.Zero)
            {
                // Leave the last ~0.2 ms to a spin so we do not overshoot even if the timer is slightly late.
                long spinTicks = _ticksPerMs / 5;
                long timerTicks = remaining - spinTicks;
                if (timerTicks > 0)
                {
                    long due100ns = -(timerTicks * 10000000L / Stopwatch.Frequency);   // negative = relative
                    if (due100ns < 0 && NativeMethods.SetWaitableTimer(_timer, ref due100ns, 0, IntPtr.Zero, IntPtr.Zero, false))
                    {
                        NativeMethods.WaitForSingleObject(_timer, (uint)(timerTicks / _ticksPerMs + 2));
                    }
                }
            }
            else
            {
                long ms = remaining / _ticksPerMs;
                if (ms > 2) Thread.Sleep((int)(ms - 1));
            }

            var spin = new SpinWait();
            while (Stopwatch.GetTimestamp() < deadlineTicks)
            {
                // SpinWait.SpinOnce() already yields the time slice once NextSpinWillYield turns true.
                spin.SpinOnce();
            }
        }

        public void Dispose()
        {
            if (_timer != IntPtr.Zero)
            {
                try { NativeMethods.CloseHandle(_timer); } catch (Exception) { }
                _timer = IntPtr.Zero;
            }
            if (_periodSet)
            {
                try { NativeMethods.timeEndPeriod(1); } catch (Exception) { }
                _periodSet = false;
            }
        }
    }
}
