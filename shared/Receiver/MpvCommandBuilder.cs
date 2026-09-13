using System;
using System.Collections.Generic;
using System.Globalization;
using HdrCast.Common;

namespace HdrCast.Receiver
{
    /// <summary>Everything needed to launch the playback pipeline (mpv alone, or ffmpeg | mpv).</summary>
    public sealed class MpvLaunchPlan
    {
        public MpvLaunchPlan(string mpvExe, IReadOnlyList<string> mpvArgs, string pipeName, string url,
            string? ffmpegExe, IReadOnlyList<string>? ffmpegArgs)
        {
            MpvExe = mpvExe;
            MpvArgs = mpvArgs;
            PipeName = pipeName;
            Url = url;
            FfmpegExe = ffmpegExe;
            FfmpegArgs = ffmpegArgs;
        }

        public string MpvExe { get; }
        public IReadOnlyList<string> MpvArgs { get; }
        /// <summary>Bare pipe name (without \\.\pipe\) for NamedPipeClientStream.</summary>
        public string PipeName { get; }
        public string Url { get; }
        /// <summary>Set for the ffmpeg pipe fallback; ffmpeg's stdout feeds mpv's stdin.</summary>
        public string? FfmpegExe { get; }
        public IReadOnlyList<string>? FfmpegArgs { get; }
        public bool UsesFfmpegPipe => FfmpegExe != null && FfmpegArgs != null;

        public string MpvCommandLine => ArgQuote.CommandLine(MpvExe, MpvArgs);
        public string? FfmpegCommandLine => UsesFfmpegPipe ? ArgQuote.CommandLine(FfmpegExe!, FfmpegArgs!) : null;

        /// <summary>Copy-and-paste form ("ffmpeg ... | mpv ..." for the fallback).</summary>
        public string ToCommandLine() => UsesFfmpegPipe ? FfmpegCommandLine + " | " + MpvCommandLine : MpvCommandLine;
    }

    /// <summary>Turns <see cref="ReceiverSettings"/> into the mpv argument list from docs/design.md section 3.1.</summary>
    public static class MpvCommandBuilder
    {
        public const string PipePrefix = @"\\.\pipe\";
        public const string WindowTitle = "HdrCast";
        public const string DemuxerMaxBytes = "256MiB";

        public static string PipeName(int pid) => "hdrcast-mpv-" + pid.ToString(CultureInfo.InvariantCulture);

        public static string BuildUrl(ReceiverSettings s) => SrtUrl.Build(s.ToSrtOptions());

        /// <summary>
        /// Builds the launch plan. <paramref name="pid"/> is the GUI process id (unique IPC pipe per instance).
        /// <paramref name="ffmpegExe"/> is only needed for <see cref="ReceiverFrontend.FfmpegPipe"/>.
        /// </summary>
        public static MpvLaunchPlan Build(ReceiverSettings s, int pid, string mpvExe, string? ffmpegExe = null)
        {
            if (s == null) throw new ArgumentNullException(nameof(s));
            if (string.IsNullOrEmpty(mpvExe)) throw new ArgumentException("mpv path required", nameof(mpvExe));
            string url = BuildUrl(s);
            string pipe = PipeName(pid);

            if (s.Frontend == ReceiverFrontend.FfmpegPipe)
            {
                if (string.IsNullOrEmpty(ffmpegExe)) throw new ArgumentException("ffmpeg path required for the pipe frontend", nameof(ffmpegExe));
                var ffArgs = FfmpegPipeArgs(url);
                var mpvArgs = MpvArgs(s, pipe, "-", pipeInput: true);
                return new MpvLaunchPlan(mpvExe, mpvArgs, pipe, url, ffmpegExe, ffArgs);
            }

            return new MpvLaunchPlan(mpvExe, MpvArgs(s, pipe, url, pipeInput: false), pipe, url, null, null);
        }

        /// <summary>ffmpeg side of the fallback: pull SRT, remux to MPEG-TS on stdout without re-encoding.</summary>
        public static List<string> FfmpegPipeArgs(string url)
        {
            return new List<string>
            {
                "-hide_banner", "-loglevel", "warning", "-nostdin",
                "-fflags", "nobuffer", "-flags", "low_delay",
                "-i", url,
                "-c", "copy",
                "-f", "mpegts", "-muxdelay", "0", "-muxpreload", "0",
                "pipe:1",
            };
        }

        /// <summary>mpv arguments; <paramref name="input"/> is the srt:// URL or "-" for stdin.</summary>
        public static List<string> MpvArgs(ReceiverSettings s, string pipeName, string input, bool pipeInput)
        {
            var a = new List<string>
            {
                "--vo=gpu-next",
                "--gpu-api=d3d11",
                "--hwdec=" + HwDecValue(s.HwDec),
                "--target-colorspace-hint=" + ColorspaceHintValue(s.DisplayMode),
            };
            if (s.DisplayMode == DisplayMode.SdrToneMap)
                a.Add("--tone-mapping=" + (string.IsNullOrWhiteSpace(s.ToneMapping) ? ReceiverSettings.DefaultToneMapping : s.ToneMapping.Trim()));
            if (s.TargetPeakNits > 0)
                a.Add("--target-peak=" + s.TargetPeakNits.ToString(CultureInfo.InvariantCulture));
            a.Add("--hdr-compute-peak=auto");

            double buffer = s.BufferSeconds > 0 ? s.BufferSeconds : 1.5;
            double readahead = Math.Max(1.0, buffer * 2.0);
            // --cache-secs overrides --demuxer-readahead-secs whenever the cache is on and its value is larger
            // (default 3600000), so it is the effective read-ahead cap; keep it generous to never throttle the SRT socket.
            double cacheSecs = Math.Max(8.0, buffer * 4.0);
            a.Add("--cache=yes");
            a.Add("--demuxer-readahead-secs=" + Num(readahead));
            a.Add("--cache-secs=" + Num(cacheSecs));
            a.Add("--cache-pause-initial=yes");
            a.Add("--cache-pause-wait=" + Num(buffer));
            a.Add("--demuxer-max-bytes=" + DemuxerMaxBytes);

            a.Add("--video-sync=audio");
            a.Add("--interpolation=no");
            a.Add("--framedrop=vo");
            a.Add("--demuxer-lavf-probesize=5000000");
            a.Add("--demuxer-lavf-analyzeduration=2");
            if (pipeInput) a.Add("--demuxer-lavf-format=mpegts");

            a.Add("--input-ipc-server=" + PipePrefix + pipeName);
            a.Add("--keep-open=no");
            a.Add("--force-window=yes");
            a.Add("--title=" + WindowTitle);
            a.Add("--msg-level=all=info");
            a.Add("--term-status-msg=");
            if (pipeInput) a.Add("--input-terminal=no");

            if (s.Fullscreen)
            {
                a.Add("--fs");
                a.Add("--fs-screen=" + Math.Max(0, s.FullscreenScreen).ToString(CultureInfo.InvariantCulture));
            }

            a.Add(input);
            return a;
        }

        public static string HwDecValue(HwDecMode mode)
        {
            switch (mode)
            {
                case HwDecMode.AutoSafe: return "auto-safe";
                case HwDecMode.D3d11vaCopy: return "d3d11va-copy";
                case HwDecMode.No: return "no";
                default: return "d3d11va";
            }
        }

        public static string ColorspaceHintValue(DisplayMode mode)
        {
            switch (mode)
            {
                case DisplayMode.HdrPassthrough: return "yes";
                case DisplayMode.SdrToneMap: return "no";
                default: return "auto";
            }
        }

        private static string Num(double v) => v.ToString("0.###", CultureInfo.InvariantCulture);
    }
}
