using System;

namespace HdrCast.Capture
{
    /// <summary>Result of one alignment step: how many frames to drop from the front of the chunk and how many silent frames to append.</summary>
    public struct AudioAlignment
    {
        public int DropFrames;
        public int PadFrames;
        public bool IsIdentity => DropFrames == 0 && PadFrames == 0;
    }

    /// <summary>
    /// Keeps the audio stream locked to the video clock (design.md §2.4). The caller measures elapsed time on the
    /// same QPC clock as the video pacer and reports every captured chunk; the aligner compares "frames that should
    /// have been written by now" (elapsed × sampleRate) with the frames actually written: if we are behind by more
    /// than the tolerance it asks for silence padding, if we are ahead it asks to drop the oldest frames.
    /// Pure logic; unit-tested.
    /// </summary>
    public sealed class AudioClockAligner
    {
        readonly int _sampleRate;
        readonly int _toleranceFrames;
        long _writtenFrames;

        public AudioClockAligner(int sampleRate = 48000, int toleranceMs = 20)
        {
            if (sampleRate <= 0) throw new ArgumentOutOfRangeException(nameof(sampleRate));
            if (toleranceMs < 0) throw new ArgumentOutOfRangeException(nameof(toleranceMs));
            _sampleRate = sampleRate;
            _toleranceFrames = (int)((long)sampleRate * toleranceMs / 1000);
        }

        public int SampleRate => _sampleRate;
        public int ToleranceFrames => _toleranceFrames;
        /// <summary>Frames (per channel) written so far, after padding / dropping.</summary>
        public long WrittenFrames => _writtenFrames;
        public long TotalPaddedFrames { get; private set; }
        public long TotalDroppedFrames { get; private set; }
        /// <summary>Number of times silence had to be inserted (reported as "audio_underrun").</summary>
        public int Underruns { get; private set; }
        public int Overruns { get; private set; }

        /// <summary>Frames that should have been written after <paramref name="elapsedTicks"/> on a clock of <paramref name="ticksPerSecond"/>.</summary>
        public long ExpectedFrames(long elapsedTicks, long ticksPerSecond)
        {
            if (elapsedTicks <= 0) return 0;
            if (elapsedTicks < 1L << 40) return elapsedTicks * _sampleRate / ticksPerSecond;
            return (long)((decimal)elapsedTicks * _sampleRate / ticksPerSecond);
        }

        /// <summary>
        /// Decides what to do with a chunk of <paramref name="incomingFrames"/> frames that arrived when
        /// <paramref name="expectedFrames"/> frames should have been written in total (see <see cref="ExpectedFrames"/>).
        /// Updates the written counter as if the caller applied the result (drop first, then chunk, then pad).
        /// </summary>
        public AudioAlignment Align(long expectedFrames, int incomingFrames)
        {
            if (incomingFrames < 0) throw new ArgumentOutOfRangeException(nameof(incomingFrames));
            var r = new AudioAlignment();
            long afterChunk = _writtenFrames + incomingFrames;
            long delta = afterChunk - expectedFrames;   // > 0: ahead of the clock, < 0: behind
            if (delta > _toleranceFrames)
            {
                // Ahead: drop the oldest frames of this chunk (never more than the chunk itself).
                long drop = Math.Min(delta, incomingFrames);
                r.DropFrames = (int)drop;
                TotalDroppedFrames += drop;
                if (drop > 0) Overruns++;
            }
            else if (-delta > _toleranceFrames)
            {
                // Behind: append silence to catch up exactly.
                long pad = -delta;
                r.PadFrames = (int)Math.Min(pad, int.MaxValue);
                TotalPaddedFrames += r.PadFrames;
                Underruns++;
            }
            _writtenFrames = afterChunk - r.DropFrames + r.PadFrames;
            return r;
        }

        /// <summary>
        /// Called periodically when no audio arrived (e.g. the loopback endpoint stopped delivering):
        /// returns the number of silent frames to write now, or 0.
        /// </summary>
        public int PadIfBehind(long expectedFrames)
        {
            var r = Align(expectedFrames, 0);
            return r.PadFrames;
        }

        public void Reset()
        {
            _writtenFrames = 0;
        }
    }
}
