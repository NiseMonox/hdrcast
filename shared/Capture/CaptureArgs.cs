using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;

namespace HdrCast.Capture
{
    /// <summary>Requested output pixel format of the capture engine (design.md §2.1 --format).</summary>
    public enum CaptureFormat
    {
        /// <summary>p010 when the desktop is HDR, nv12 otherwise.</summary>
        Auto,
        /// <summary>HDR10: PQ / BT.2020 / limited range 4:2:0 10 bit (p010le).</summary>
        P010,
        /// <summary>SDR: BT.709 / limited range 4:2:0 8 bit (nv12).</summary>
        Nv12,
    }

    /// <summary>Thrown by <see cref="CaptureArgs.Parse"/> for malformed command lines (exit code 2).</summary>
    public sealed class CaptureArgsException : Exception
    {
        public CaptureArgsException(string message) : base(message) { }
    }

    /// <summary>
    /// Parsed command line of <c>HdrCastSender.exe capture ...</c> (design.md §2.1) plus the debug switches.
    /// Pure logic: no Windows dependencies, unit-tested on Linux.
    /// </summary>
    public sealed class CaptureArgs
    {
        public const int DefaultFps = 60;
        public const float DefaultSdrWhite = 80f;
        public const string DefaultVideoPipe = "hdrcast-video";
        public const string DefaultAudioPipe = "hdrcast-audio";
        public const int DefaultPipeTimeoutSeconds = 30;

        /// <summary>Global DXGI output index (order of EnumAdapters1 → EnumOutputs).</summary>
        public int OutputIndex { get; set; } = -1;
        public int Fps { get; set; } = DefaultFps;
        public CaptureFormat Format { get; set; } = CaptureFormat.Auto;
        /// <summary>Requested output width, or 0 for the display's native size.</summary>
        public int Width { get; set; }
        /// <summary>Requested output height, or 0 for the display's native size.</summary>
        public int Height { get; set; }
        public float SdrWhite { get; set; } = DefaultSdrWhite;
        public bool Cursor { get; set; } = true;
        public string VideoPipe { get; set; } = DefaultVideoPipe;
        public string AudioPipe { get; set; } = DefaultAudioPipe;
        public bool Audio { get; set; } = true;
        /// <summary>"default" or an MMDevice endpoint id.</summary>
        public string AudioDevice { get; set; } = "default";
        /// <summary>Executable whose audio (with everything it started) is left out, e.g. "Discord.exe"; "" = none.</summary>
        public string AudioExclude { get; set; } = "";
        public int PipeTimeoutSeconds { get; set; } = DefaultPipeTimeoutSeconds;
        /// <summary>Process id to watch; the engine exits when that process is gone. 0 = none.</summary>
        public int ParentPid { get; set; }
        /// <summary>Debug: write raw frames to this file instead of the pipe.</summary>
        public string? DumpPath { get; set; }
        /// <summary>Debug: seconds to run in dump mode (0 = until stopped).</summary>
        public int DumpSeconds { get; set; }
        /// <summary>Debug: run the shader on a synthetic gradient and print values.</summary>
        public bool SelfTest { get; set; }

        public bool HasExplicitSize => Width > 0 && Height > 0;

        public static string Usage =>
            "usage: HdrCastSender.exe capture --output <idx> [options]\n" +
            "  --output <idx>            DXGI output index (see list-outputs)\n" +
            "  --fps <n>                 constant output frame rate (default 60)\n" +
            "  --format auto|p010|nv12   auto = p010 when the desktop is HDR, else nv12\n" +
            "  --size WxH                output size (bilinear scaling in the shader); default native\n" +
            "  --sdr-white <nits>        luminance of scRGB 1.0 (default 80)\n" +
            "  --cursor on|off           draw the mouse cursor (default on)\n" +
            "  --video-pipe <name>       named pipe for video (default hdrcast-video)\n" +
            "  --audio-pipe <name>       named pipe for audio (default hdrcast-audio)\n" +
            "  --audio on|off            capture desktop audio (default on)\n" +
            "  --audio-device default|<endpoint id>\n" +
            "  --audio-exclude <app.exe> leave this program's audio out (e.g. the voice chat)\n" +
            "  --pipe-timeout <s>        seconds to wait for the pipe client (default 30)\n" +
            "  --parent-pid <pid>        exit when this process disappears\n" +
            "  --dump <file> [--seconds <n>]  write raw frames to a file instead of the pipe\n" +
            "  --selftest                run the conversion shader on a synthetic gradient\n";

        /// <summary>Parses the arguments that follow the <c>capture</c> verb.</summary>
        public static CaptureArgs Parse(string[] args)
        {
            if (args == null) throw new ArgumentNullException(nameof(args));
            var a = new CaptureArgs();
            for (int i = 0; i < args.Length; i++)
            {
                string opt = args[i];
                string Next()
                {
                    if (i + 1 >= args.Length) throw new CaptureArgsException("missing value after " + opt);
                    return args[++i];
                }
                switch (opt.ToLowerInvariant())
                {
                    case "--output": a.OutputIndex = ParseInt(opt, Next(), 0, 63); break;
                    case "--fps": a.Fps = ParseInt(opt, Next(), 1, 480); break;
                    case "--format": a.Format = ParseFormat(Next()); break;
                    case "--size":
                    {
                        ParseSize(Next(), out int w, out int h);
                        a.Width = w; a.Height = h;
                        break;
                    }
                    case "--sdr-white": a.SdrWhite = ParseFloat(opt, Next(), 1f, 10000f); break;
                    case "--cursor": a.Cursor = ParseOnOff(opt, Next()); break;
                    case "--video-pipe": a.VideoPipe = ParsePipeName(opt, Next()); break;
                    case "--audio-pipe": a.AudioPipe = ParsePipeName(opt, Next()); break;
                    case "--audio": a.Audio = ParseOnOff(opt, Next()); break;
                    case "--audio-device":
                    {
                        string v = Next();
                        if (v.Length == 0) throw new CaptureArgsException("--audio-device needs a value");
                        a.AudioDevice = v;
                        break;
                    }
                    case "--audio-exclude":
                    {
                        string v = Next();
                        a.AudioExclude = AppAudioTarget.NormalizeExeName(v);
                        if (a.AudioExclude.Length == 0) throw new CaptureArgsException("--audio-exclude: expected a program name like Discord.exe, got " + v);
                        break;
                    }
                    case "--pipe-timeout": a.PipeTimeoutSeconds = ParseInt(opt, Next(), 1, 3600); break;
                    case "--parent-pid": a.ParentPid = ParseInt(opt, Next(), 1, int.MaxValue); break;
                    case "--dump":
                    {
                        string v = Next();
                        if (v.Length == 0) throw new CaptureArgsException("--dump needs a file path");
                        a.DumpPath = v;
                        break;
                    }
                    case "--seconds": a.DumpSeconds = ParseInt(opt, Next(), 1, 86400); break;
                    case "--selftest": a.SelfTest = true; break;
                    default:
                        throw new CaptureArgsException("unknown option: " + opt);
                }
            }
            if (!a.SelfTest && a.OutputIndex < 0)
                throw new CaptureArgsException("--output <idx> is required");
            if (a.DumpSeconds > 0 && a.DumpPath == null)
                throw new CaptureArgsException("--seconds requires --dump <file>");
            return a;
        }

        static int ParseInt(string opt, string value, int min, int max)
        {
            if (!int.TryParse(value, NumberStyles.Integer, CultureInfo.InvariantCulture, out int n))
                throw new CaptureArgsException(opt + ": not an integer: " + value);
            if (n < min || n > max)
                throw new CaptureArgsException(opt + ": " + n + " is out of range [" + min + ".." + max + "]");
            return n;
        }

        static float ParseFloat(string opt, string value, float min, float max)
        {
            if (!float.TryParse(value, NumberStyles.Float, CultureInfo.InvariantCulture, out float f))
                throw new CaptureArgsException(opt + ": not a number: " + value);
            if (f < min || f > max)
                throw new CaptureArgsException(opt + ": " + value + " is out of range [" + min + ".." + max + "]");
            return f;
        }

        static bool ParseOnOff(string opt, string value)
        {
            switch (value.ToLowerInvariant())
            {
                case "on": case "1": case "true": case "yes": return true;
                case "off": case "0": case "false": case "no": return false;
            }
            throw new CaptureArgsException(opt + ": expected on|off, got " + value);
        }

        static CaptureFormat ParseFormat(string value)
        {
            switch (value.ToLowerInvariant())
            {
                case "auto": return CaptureFormat.Auto;
                case "p010": return CaptureFormat.P010;
                case "nv12": return CaptureFormat.Nv12;
            }
            throw new CaptureArgsException("--format: expected auto|p010|nv12, got " + value);
        }

        static string ParsePipeName(string opt, string value)
        {
            if (value.Length == 0 || value.IndexOf('\\') >= 0 || value.IndexOf('/') >= 0)
                throw new CaptureArgsException(opt + ": give only the pipe name (without \\\\.\\pipe\\), got " + value);
            return value;
        }

        /// <summary>Parses "WxH"; both must be even and at least 2 (the shader writes 2x2 blocks).</summary>
        public static void ParseSize(string value, out int width, out int height)
        {
            int x = value.IndexOfAny(new[] { 'x', 'X' });
            if (x <= 0 || x == value.Length - 1 ||
                !int.TryParse(value.Substring(0, x), NumberStyles.Integer, CultureInfo.InvariantCulture, out width) ||
                !int.TryParse(value.Substring(x + 1), NumberStyles.Integer, CultureInfo.InvariantCulture, out height))
                throw new CaptureArgsException("--size: expected WxH, got " + value);
            if (width < 2 || height < 2 || width > 16384 || height > 16384)
                throw new CaptureArgsException("--size: " + value + " is out of range");
            if ((width & 1) != 0 || (height & 1) != 0)
                throw new CaptureArgsException("--size: width and height must be even (4:2:0 output), got " + value);
        }

        /// <summary>Builds the argument vector (after the <c>capture</c> verb) for this configuration — used by the GUI to launch the engine.</summary>
        public string[] ToArgv()
        {
            var list = new List<string>();
            list.Add("--output"); list.Add(OutputIndex.ToString(CultureInfo.InvariantCulture));
            list.Add("--fps"); list.Add(Fps.ToString(CultureInfo.InvariantCulture));
            list.Add("--format"); list.Add(FormatName(Format));
            if (HasExplicitSize) { list.Add("--size"); list.Add(Width.ToString(CultureInfo.InvariantCulture) + "x" + Height.ToString(CultureInfo.InvariantCulture)); }
            list.Add("--sdr-white"); list.Add(SdrWhite.ToString("0.###", CultureInfo.InvariantCulture));
            list.Add("--cursor"); list.Add(Cursor ? "on" : "off");
            list.Add("--video-pipe"); list.Add(VideoPipe);
            list.Add("--audio"); list.Add(Audio ? "on" : "off");
            if (Audio)
            {
                list.Add("--audio-pipe"); list.Add(AudioPipe);
                if (AudioDevice != "default") { list.Add("--audio-device"); list.Add(AudioDevice); }
                if (AudioExclude.Length > 0) { list.Add("--audio-exclude"); list.Add(AudioExclude); }
            }
            if (PipeTimeoutSeconds != DefaultPipeTimeoutSeconds) { list.Add("--pipe-timeout"); list.Add(PipeTimeoutSeconds.ToString(CultureInfo.InvariantCulture)); }
            if (ParentPid > 0) { list.Add("--parent-pid"); list.Add(ParentPid.ToString(CultureInfo.InvariantCulture)); }
            if (DumpPath != null)
            {
                list.Add("--dump"); list.Add(DumpPath);
                if (DumpSeconds > 0) { list.Add("--seconds"); list.Add(DumpSeconds.ToString(CultureInfo.InvariantCulture)); }
            }
            if (SelfTest) list.Add("--selftest");
            return list.ToArray();
        }

        public static string FormatName(CaptureFormat f)
        {
            switch (f)
            {
                case CaptureFormat.P010: return "p010";
                case CaptureFormat.Nv12: return "nv12";
                default: return "auto";
            }
        }

        /// <summary>Quotes an argv for display / for ProcessStartInfo.Arguments (Windows CommandLineToArgvW rules).</summary>
        public static string QuoteArgv(IEnumerable<string> argv)
        {
            var sb = new StringBuilder();
            foreach (string s in argv)
            {
                if (sb.Length > 0) sb.Append(' ');
                if (s.Length > 0 && s.IndexOfAny(new[] { ' ', '\t', '"' }) < 0) { sb.Append(s); continue; }
                sb.Append('"');
                int backslashes = 0;
                foreach (char c in s)
                {
                    if (c == '\\') { backslashes++; continue; }
                    if (c == '"') { sb.Append('\\', backslashes * 2 + 1); sb.Append('"'); backslashes = 0; continue; }
                    sb.Append('\\', backslashes); backslashes = 0;
                    sb.Append(c);
                }
                sb.Append('\\', backslashes * 2);
                sb.Append('"');
            }
            return sb.ToString();
        }
    }
}
