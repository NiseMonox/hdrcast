using System;
using System.Globalization;

namespace HdrCast.Sender.Model
{
    /// <summary>
    /// One block of ffmpeg's <c>-progress pipe:1</c> output. ffmpeg writes "key=value" lines and closes each
    /// block with "progress=continue" (or "progress=end"); values can be "N/A".
    /// </summary>
    public sealed class FfmpegProgress
    {
        public long Frame { get; set; }
        public double Fps { get; set; }
        /// <summary>Instantaneous bitrate in kbit/s (-1 when ffmpeg reports N/A).</summary>
        public double BitrateKbps { get; set; } = -1;
        public long TotalSizeBytes { get; set; } = -1;
        public long OutTimeUs { get; set; } = -1;
        public long DupFrames { get; set; }
        public long DropFrames { get; set; }
        public double Speed { get; set; } = -1;
        public bool Ended { get; set; }

        public TimeSpan OutTime => OutTimeUs >= 0 ? TimeSpan.FromMilliseconds(OutTimeUs / 1000.0) : TimeSpan.Zero;
    }

    /// <summary>Accumulates <c>-progress</c> lines and raises a snapshot at the end of every block.</summary>
    public sealed class FfmpegProgressParser
    {
        FfmpegProgress _current = new FfmpegProgress();

        /// <summary>Raised once per complete block.</summary>
        public event Action<FfmpegProgress>? Block;

        /// <summary>Feeds one stdout line. Returns the finished block when the line closed one, else null.</summary>
        public FfmpegProgress? Feed(string? line)
        {
            if (line == null) return null;
            line = line.Trim();
            int eq = line.IndexOf('=');
            if (eq <= 0) return null;
            string key = line.Substring(0, eq).Trim();
            string value = line.Substring(eq + 1).Trim();

            switch (key)
            {
                case "frame": _current.Frame = ParseLong(value, _current.Frame); break;
                case "fps": _current.Fps = ParseDouble(value, _current.Fps); break;
                case "bitrate": _current.BitrateKbps = ParseBitrate(value); break;
                case "total_size": _current.TotalSizeBytes = ParseLong(value, -1); break;
                case "out_time_us":
                case "out_time_ms":
                    // Note: ffmpeg's out_time_ms is microseconds too (a long-standing naming quirk).
                    _current.OutTimeUs = ParseLong(value, -1);
                    break;
                case "dup_frames": _current.DupFrames = ParseLong(value, _current.DupFrames); break;
                case "drop_frames": _current.DropFrames = ParseLong(value, _current.DropFrames); break;
                case "speed": _current.Speed = ParseSpeed(value); break;
                case "progress":
                {
                    FfmpegProgress done = _current;
                    done.Ended = value == "end";
                    _current = new FfmpegProgress
                    {
                        // carry forward monotonic counters so a block without them is not reported as zero
                        Frame = done.Frame,
                        DupFrames = done.DupFrames,
                        DropFrames = done.DropFrames,
                        TotalSizeBytes = done.TotalSizeBytes,
                        OutTimeUs = done.OutTimeUs,
                    };
                    Block?.Invoke(done);
                    return done;
                }
                default: break;
            }
            return null;
        }

        static long ParseLong(string v, long fallback)
            => long.TryParse(v, NumberStyles.Integer, CultureInfo.InvariantCulture, out long n) ? n : fallback;

        static double ParseDouble(string v, double fallback)
            => double.TryParse(v, NumberStyles.Float, CultureInfo.InvariantCulture, out double d) ? d : fallback;

        /// <summary>"4520.1kbits/s" or "N/A".</summary>
        static double ParseBitrate(string v)
        {
            v = v.Trim();
            int idx = v.IndexOf("kbits/s", StringComparison.OrdinalIgnoreCase);
            if (idx > 0) v = v.Substring(0, idx);
            else if (v.EndsWith("bits/s", StringComparison.OrdinalIgnoreCase)) v = v.Substring(0, v.Length - 6);
            return double.TryParse(v, NumberStyles.Float, CultureInfo.InvariantCulture, out double d) ? d : -1;
        }

        /// <summary>"1.01x" or "N/A".</summary>
        static double ParseSpeed(string v)
        {
            v = v.Trim();
            if (v.EndsWith("x", StringComparison.OrdinalIgnoreCase)) v = v.Substring(0, v.Length - 1);
            return double.TryParse(v, NumberStyles.Float, CultureInfo.InvariantCulture, out double d) ? d : -1;
        }
    }
}
