using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Text;
using System.Threading;
using HdrCast.Common;
using HdrCast.Sender.Model;

namespace HdrCast.Sender
{
    /// <summary>Result of probing one encoder with the real option string.</summary>
    public sealed class EncoderProbeResult
    {
        public VideoEncoder Encoder { get; set; }
        public bool Listed { get; set; }
        /// <summary>The probe encode succeeded (the encoder exists, the GPU supports it and the options parse).</summary>
        public bool Works { get; set; }
        public string Message { get; set; } = "";
    }

    /// <summary>
    /// Asks ffmpeg which encoders it has, then actually encodes three frames with the exact option string the
    /// sender would use. That catches unsupported hardware, wrong driver versions and option-name changes
    /// between ffmpeg builds before a stream is started (design.md §2.5).
    /// </summary>
    public static class EncoderProbe
    {
        /// <summary>Runs ffmpeg and returns stdout+stderr; null when it could not be started.</summary>
        public static string? RunFfmpeg(string ffmpegExe, IEnumerable<string> args, int timeoutMs, out int exitCode)
        {
            exitCode = -1;
            var psi = new ProcessStartInfo
            {
                FileName = ffmpegExe,
                Arguments = ArgQuote.Join(args),
                UseShellExecute = false,
                CreateNoWindow = true,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
            };
            try
            {
                using (var p = new Process { StartInfo = psi })
                {
                    var sb = new StringBuilder();
                    p.OutputDataReceived += (s, e) => { if (e.Data != null) lock (sb) sb.AppendLine(e.Data); };
                    p.ErrorDataReceived += (s, e) => { if (e.Data != null) lock (sb) sb.AppendLine(e.Data); };
                    p.Start();
                    p.BeginOutputReadLine();
                    p.BeginErrorReadLine();
                    if (!p.WaitForExit(timeoutMs))
                    {
                        try { LineProcess.KillTree(p); } catch (Exception) { }
                        lock (sb) sb.AppendLine("(timed out after " + timeoutMs + " ms)");
                    }
                    else
                    {
                        exitCode = p.ExitCode;
                    }
                    lock (sb) return sb.ToString();
                }
            }
            catch (Exception)
            {
                return null;   // ffmpeg.exe missing or not executable
            }
        }

        /// <summary>Encoder names ffmpeg reports in <c>-encoders</c>.</summary>
        public static HashSet<string> ListEncoders(string ffmpegExe)
        {
            var found = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
            string? output = RunFfmpeg(ffmpegExe, new[] { "-hide_banner", "-encoders" }, 15000, out _);
            if (output == null) return found;
            foreach (string line in output.Split('\n'))
            {
                // " V....D hevc_nvenc           NVIDIA NVENC hevc encoder (codec hevc)"
                string t = line.TrimEnd();
                if (t.Length < 9 || t[1] != 'V') continue;
                string[] parts = t.Substring(7).Trim().Split(new[] { ' ' }, 2, StringSplitOptions.RemoveEmptyEntries);
                if (parts.Length > 0 && parts[0].Length > 0) found.Add(parts[0]);
            }
            return found;
        }

        /// <summary>Probes every catalogued encoder; <paramref name="progress"/> is called with each result.</summary>
        public static List<EncoderProbeResult> ProbeAll(string ffmpegExe, bool hdr, Action<EncoderProbeResult>? progress = null,
                                                        CancellationToken cancel = default(CancellationToken))
        {
            var results = new List<EncoderProbeResult>();
            HashSet<string> listed = ListEncoders(ffmpegExe);
            foreach (EncoderInfo info in EncoderProfiles.All)
            {
                if (cancel.IsCancellationRequested) break;
                var r = new EncoderProbeResult { Encoder = info.Id, Listed = listed.Contains(info.FfmpegName) };
                if (!r.Listed)
                {
                    r.Message = "not in this ffmpeg build";
                }
                else
                {
                    var settings = new EncoderSettings { Encoder = info.Id, Hdr = hdr && info.Supports10Bit };
                    string? output = RunFfmpeg(ffmpegExe, FfmpegCommandBuilder.BuildProbe(settings), 30000, out int exit);
                    if (output == null) r.Message = "ffmpeg could not be started";
                    else if (exit == 0) { r.Works = true; r.Message = "ok"; }
                    else r.Message = FirstError(output);
                }
                results.Add(r);
                progress?.Invoke(r);
            }
            return results;
        }

        static string FirstError(string output)
        {
            string[] lines = output.Split('\n');
            for (int i = lines.Length - 1; i >= 0; i--)
            {
                string t = lines[i].Trim();
                if (t.Length == 0) continue;
                if (t.StartsWith("frame=", StringComparison.OrdinalIgnoreCase)) continue;
                return t.Length > 200 ? t.Substring(0, 200) : t;
            }
            return "failed";
        }

        /// <summary>First working encoder in <see cref="EncoderProfiles.AutoOrder"/>, or null.</summary>
        public static VideoEncoder? PickBest(IEnumerable<EncoderProbeResult> results)
        {
            var works = new HashSet<VideoEncoder>();
            foreach (EncoderProbeResult r in results) if (r.Works) works.Add(r.Encoder);
            foreach (VideoEncoder e in EncoderProfiles.AutoOrder) if (works.Contains(e)) return e;
            return null;
        }
    }
}
