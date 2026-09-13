using System;
using System.Globalization;
using System.Text;

namespace HdrCast.Capture
{
    /// <summary>
    /// The once-per-second statistics line the capture engine prints to stdout (design.md §2.2), e.g.
    /// {"t":12.0,"captured":700,"duplicated":20,"dropped":0,"fps":59.9,"gpu_ms":0.8,"audio_underrun":0,"pipe_queue_ms":16.7}
    /// Hand-written JSON (tiny, invariant culture) so both the engine and the GUI parser stay dependency-free.
    /// </summary>
    public sealed class CaptureStats
    {
        public double T { get; set; }
        public long Captured { get; set; }
        public long Duplicated { get; set; }
        public long Dropped { get; set; }
        public double Fps { get; set; }
        public double GpuMs { get; set; }
        public long AudioUnderrun { get; set; }
        public double PipeQueueMs { get; set; }

        public string ToJsonLine()
        {
            var sb = new StringBuilder(160);
            sb.Append("{\"t\":").Append(T.ToString("0.0", CultureInfo.InvariantCulture));
            sb.Append(",\"captured\":").Append(Captured.ToString(CultureInfo.InvariantCulture));
            sb.Append(",\"duplicated\":").Append(Duplicated.ToString(CultureInfo.InvariantCulture));
            sb.Append(",\"dropped\":").Append(Dropped.ToString(CultureInfo.InvariantCulture));
            sb.Append(",\"fps\":").Append(Fps.ToString("0.0", CultureInfo.InvariantCulture));
            sb.Append(",\"gpu_ms\":").Append(GpuMs.ToString("0.00", CultureInfo.InvariantCulture));
            sb.Append(",\"audio_underrun\":").Append(AudioUnderrun.ToString(CultureInfo.InvariantCulture));
            sb.Append(",\"pipe_queue_ms\":").Append(PipeQueueMs.ToString("0.0", CultureInfo.InvariantCulture));
            sb.Append('}');
            return sb.ToString();
        }

        /// <summary>Parses a stats line; returns null for anything that is not a flat JSON object of numbers.</summary>
        public static CaptureStats? TryParse(string? line)
        {
            if (line == null) return null;
            line = line.Trim();
            if (line.Length < 2 || line[0] != '{' || line[line.Length - 1] != '}') return null;
            var s = new CaptureStats();
            bool any = false;
            foreach (string part in line.Substring(1, line.Length - 2).Split(','))
            {
                int colon = part.IndexOf(':');
                if (colon <= 0) continue;
                string key = part.Substring(0, colon).Trim().Trim('"');
                string val = part.Substring(colon + 1).Trim();
                if (!double.TryParse(val, NumberStyles.Float, CultureInfo.InvariantCulture, out double d)) continue;
                any = true;
                switch (key)
                {
                    case "t": s.T = d; break;
                    case "captured": s.Captured = (long)d; break;
                    case "duplicated": s.Duplicated = (long)d; break;
                    case "dropped": s.Dropped = (long)d; break;
                    case "fps": s.Fps = d; break;
                    case "gpu_ms": s.GpuMs = d; break;
                    case "audio_underrun": s.AudioUnderrun = (long)d; break;
                    case "pipe_queue_ms": s.PipeQueueMs = d; break;
                    default: break;
                }
            }
            return any ? s : null;
        }
    }
}
