// Generates tests/vectors/csharp-0.2.4.json from the C# 0.2.4 sources (see Generator.csproj for how to run).
// Every section is a list of { "in": ..., "out": ... } pairs; tests/csharp_vectors.rs feeds each "in" to the
// Rust port and compares.

using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Linq;
using System.Text;
using HdrCast.Capture;
using HdrCast.Common;
using HdrCast.Receiver;
using HdrCast.Sender;
using HdrCast.Sender.Model;

namespace HdrCast.Vectors
{
    static class Program
    {
        static Dictionary<string, object?> D(params object?[] kv)
        {
            var d = new Dictionary<string, object?>(StringComparer.Ordinal);
            for (int i = 0; i < kv.Length; i += 2) d[(string)kv[i]!] = kv[i + 1];
            return d;
        }

        static List<object?> L<T>(IEnumerable<T> items)
        {
            var l = new List<object?>();
            foreach (T x in items) l.Add(x);
            return l;
        }

        static object? Pair(object? input, object? output) => D("in", input, "out", output);

        /// <summary>The message as written in the source: .NET Framework appends "\r\nParameter name: x".</summary>
        static string Message(Exception e)
        {
            string m = e.Message;
            int nl = m.IndexOfAny(new[] { '\r', '\n' });
            if (nl >= 0) m = m.Substring(0, nl);
            int par = m.IndexOf(" (Parameter '", StringComparison.Ordinal);
            if (par >= 0) m = m.Substring(0, par);
            return m;
        }

        static int Main(string[] argv)
        {
            if (argv.Length != 1) { Console.Error.WriteLine("usage: Generator <output.json>"); return 2; }
            var sections = new List<KeyValuePair<string, List<object?>>>
            {
                Section("catalogue", Catalogue()),
                Section("video_args", VideoArgs()),
                Section("ffmpeg_build", FfmpegBuild()),
                Section("probe", Probe()),
                Section("progress", Progress()),
                Section("progress_line", ProgressLine()),
                Section("first_error", FirstError()),
                Section("sender_settings", SenderSettingsVectors()),
                Section("resolve_size", ResolveSize()),
                Section("capture_args", CaptureArgsVectors()),
                Section("receiver_settings", ReceiverSettingsVectors()),
                Section("mpv", Mpv()),
                Section("ipc_message", IpcMessage()),
                Section("ipc_command", IpcCommand()),
                Section("status", Status()),
                Section("log_line", LogLine()),
                Section("log_lines", LogLines()),
            };

            var sb = new StringBuilder();
            sb.Append("{\n");
            sb.Append("\"generator\": ").Append(MiniJson.Serialize(
                "HdrCast 0.2.4 shared/Sender + shared/Receiver on " + System.Runtime.InteropServices.RuntimeInformation.FrameworkDescription)).Append(",\n");
            for (int s = 0; s < sections.Count; s++)
            {
                sb.Append('"').Append(sections[s].Key).Append("\": [\n");
                List<object?> items = sections[s].Value;
                for (int i = 0; i < items.Count; i++)
                    sb.Append(MiniJson.Serialize(items[i])).Append(i + 1 < items.Count ? ",\n" : "\n");
                sb.Append(s + 1 < sections.Count ? "],\n" : "]\n");
            }
            sb.Append("}\n");
            File.WriteAllText(argv[0], sb.ToString(), new UTF8Encoding(false));
            int total = sections.Sum(x => x.Value.Count);
            Console.WriteLine("wrote " + total + " vectors in " + sections.Count + " sections to " + argv[0]);
            return 0;
        }

        static KeyValuePair<string, List<object?>> Section(string name, List<object?> items)
            => new KeyValuePair<string, List<object?>>(name, items);

        // ---- sender -------------------------------------------------------------------------------

        static List<object?> Catalogue()
        {
            var list = new List<object?>();
            foreach (EncoderInfo i in EncoderProfiles.All)
                list.Add(Pair("encoder", D("id", (int)i.Id, "ffmpegName", i.FfmpegName, "displayName", i.DisplayName, "vendor", i.Vendor,
                                           "isHardware", i.IsHardware, "supports10Bit", i.Supports10Bit,
                                           "pixelFormatHdr", EncoderProfiles.PixelFormat(i.Id, true),
                                           "pixelFormatSdr", EncoderProfiles.PixelFormat(i.Id, false))));
            list.Add(Pair("auto_order", L(EncoderProfiles.AutoOrder.Select(e => (int)e))));
            list.Add(Pair("color_tags_hdr", L(EncoderProfiles.ColorTags(true))));
            list.Add(Pair("color_tags_sdr", L(EncoderProfiles.ColorTags(false))));
            foreach (int[] m in new[] { new[] { 1920, 1080, 60 }, new[] { 2560, 1440, 60 }, new[] { 3840, 2160, 60 }, new[] { 2560, 1440, 30 },
                                        new[] { 3840, 2160, 120 }, new[] { 1280, 720, 30 }, new[] { 640, 360, 30 }, new[] { 7680, 4320, 120 },
                                        new[] { 3440, 1440, 144 }, new[] { 1920, 1200, 75 }, new[] { 2560, 1600, 165 }, new[] { 5120, 2880, 60 } })
                list.Add(Pair(D("suggested_mbps", L(m)), EncoderProfiles.SuggestedMbps(m[0], m[1], m[2])));
            list.Add(Pair("latency_presets", L(ReceiverSettings.LatencyPresets.Select(p => (object?)D("name", p.Name, "latencyMs", p.LatencyMs, "text", p.ToString())))));
            list.Add(Pair("tone_mapping_algorithms", L(ReceiverSettings.ToneMappingAlgorithms)));
            list.Add(Pair("hwdec_values", L(new[] { 0, 1, 2, 3 }.Select(m => MpvCommandBuilder.HwDecValue((HwDecMode)m)))));
            list.Add(Pair("colorspace_hint_values", L(new[] { 0, 1, 2 }.Select(m => MpvCommandBuilder.ColorspaceHintValue((DisplayMode)m)))));
            list.Add(Pair("pipe_name_4321", MpvCommandBuilder.PipeName(4321)));
            list.Add(Pair("pipe_prefix", MpvCommandBuilder.PipePrefix));
            list.Add(Pair("ffmpeg_pipe_path_foo", FfmpegCommandBuilder.PipePath("foo")));
            list.Add(Pair("exit_reason_texts", L(new[] { 0, 1, 2, 3, 4, 5, 6 }.Select(r => MpvLogClassifier.Describe((MpvExitReason)r)))));
            list.Add(Pair("observed_properties", L(MpvObservedProperties.All.Select(kv => (object?)L(new object?[] { kv.Key, kv.Value })))));
            list.Add(Pair("observe_commands", L(MpvObservedProperties.ObserveCommands())));
            list.Add(Pair("file_names", L(new[] { SenderSettings.FileName, ReceiverSettings.FileName, ReceiverSettings.DefaultToneMapping,
                                                  MpvCommandBuilder.WindowTitle, MpvCommandBuilder.DemuxerMaxBytes })));
            return list;
        }

        static readonly double[][] Rates =
        {
            // kbps, fps, gop seconds, buffer seconds, mastering nits
            new[] { 40000.0, 60, 2.0, 2.0, 1000 },
            new[] { 8000.0, 30, 1.5, 0.5, 0 },
            new[] { 2500.0, 25, 0.5, 3.0, 400 },          // 12.5 frames: Math.Round goes to the even 12
            new[] { 100000.0, 120, 10.0, 10.0, 10000 },
            new[] { 1001.0, 24, 0.3, 0.5, 120 },          // 500.5 kbit buffer -> 500
            new[] { 6000.0, 1, 0.25, 1.25, 199 },
        };

        static EncoderSettings Enc(int encoder, int preset, bool hdr, double[] r, bool b, bool la) => new EncoderSettings
        {
            Encoder = (VideoEncoder)encoder, Preset = (QualityPreset)preset, Hdr = hdr,
            BitrateKbps = (int)r[0], Fps = (int)r[1], GopSeconds = r[2], BufferSeconds = r[3],
            BFrames = b, Lookahead = la, MasteringMaxNits = (int)r[4],
        };

        static Dictionary<string, object?> EncIn(EncoderSettings s) => D(
            "encoder", (int)s.Encoder, "preset", (int)s.Preset, "hdr", s.Hdr, "kbps", s.BitrateKbps, "fps", s.Fps,
            "gop", s.GopSeconds, "buffer", s.BufferSeconds, "b", s.BFrames, "la", s.Lookahead, "nits", s.MasteringMaxNits);

        static List<object?> VideoArgs()
        {
            var list = new List<object?>();
            foreach (int encoder in new[] { 0, 2, 4, 5 })
                foreach (int preset in new[] { 0, 1, 2 })
                    foreach (bool hdr in new[] { false, true })
                        for (int r = 0; r < Rates.Length; r++)
                        {
                            // every switch combination for the first rate set, the two extremes for the others
                            var switches = r == 0 ? new[] { 3, 2, 1, 0 } : new[] { 3, 0 };
                            foreach (int sw in switches)
                            {
                                EncoderSettings s = Enc(encoder, preset, hdr, Rates[r], (sw & 1) != 0, (sw & 2) != 0);
                                list.Add(Pair(EncIn(s), L(EncoderProfiles.BuildVideoArgs(s))));
                            }
                        }
            return list;
        }

        static Dictionary<string, object?> PlanIn(FfmpegPlan p) => D(
            "videoPipe", p.VideoPipeName, "audioPipe", p.AudioPipeName, "format", p.PixelFormat.ToString().ToLowerInvariant(),
            "width", p.Width, "height", p.Height, "fps", p.Fps, "encoder", EncIn(p.Encoder), "audio", (int)p.Audio,
            "audioKbps", p.AudioBitrateKbps, "outputUrl", p.OutputUrl, "progress", p.Progress, "progressUrl", p.ProgressUrl,
            "logLevel", p.LogLevel);

        static object? PlanOut(FfmpegPlan p, bool commandLine)
        {
            try
            {
                // the command line repeats the arguments: only some vectors carry it
                return commandLine
                    ? D("args", L(FfmpegCommandBuilder.Build(p)),
                        "commandLine", FfmpegCommandBuilder.CommandLine(@"C:\Program Files\HdrCast\tools\ffmpeg.exe", p))
                    : D("args", L(FfmpegCommandBuilder.Build(p)));
            }
            catch (ArgumentException e)
            {
                return D("error", Message(e));
            }
        }

        static List<object?> FfmpegBuild()
        {
            var list = new List<object?>();
            void Add(FfmpegPlan p, bool commandLine = false) => list.Add(Pair(PlanIn(p), PlanOut(p, commandLine)));

            foreach (int encoder in new[] { 0, 2, 4, 5 })
                foreach (bool hdr in new[] { true, false })
                    foreach (int audio in new[] { 0, 1, 2 })
                        foreach (bool progress in new[] { true, false })
                            Add(new FfmpegPlan
                            {
                                VideoPipeName = "hdrcast-video-1234",
                                AudioPipeName = audio == 0 ? null : "hdrcast-audio-1234",
                                PixelFormat = hdr ? CaptureFormat.P010 : CaptureFormat.Nv12,
                                Width = hdr ? 3840 : 2560, Height = hdr ? 2160 : 1440, Fps = progress ? 60 : 120,
                                Encoder = Enc(encoder, 1, hdr, Rates[0], true, true),
                                Audio = (AudioCodec)audio, AudioBitrateKbps = audio == 2 ? 128 : 192,
                                Progress = progress,
                            }, commandLine: encoder == 0 && audio == 1);

            // the defaults of a plan, and what the colour regression test uses
            Add(new FfmpegPlan { Width = 1920, Height = 1080, Encoder = Enc(0, 1, true, Rates[0], true, true) });
            Add(new FfmpegPlan
            {
                VideoPipeName = "colour-test", PixelFormat = CaptureFormat.P010, Width = 320, Height = 160, Fps = 30,
                Encoder = new EncoderSettings { Encoder = VideoEncoder.LibX265, Preset = QualityPreset.Speed, Hdr = true, BitrateKbps = 8000, Fps = 30, GopSeconds = 1 },
                Audio = AudioCodec.None, Progress = false, OutputUrl = @"C:\Temp\out file.ts", LogLevel = "error",
            }, commandLine: true);
            // an audio codec without a pipe, a pipe without a codec, an empty pipe name: no audio input
            Add(new FfmpegPlan { Width = 1280, Height = 720, Encoder = Enc(5, 0, true, Rates[1], true, true), Audio = AudioCodec.Opus });
            Add(new FfmpegPlan { Width = 1280, Height = 720, Encoder = Enc(5, 0, true, Rates[1], true, true), Audio = AudioCodec.None, AudioPipeName = "a" });
            Add(new FfmpegPlan { Width = 1280, Height = 720, Encoder = Enc(5, 0, true, Rates[1], true, true), Audio = AudioCodec.Aac, AudioPipeName = "" });
            Add(new FfmpegPlan { Width = 1280, Height = 720, PixelFormat = CaptureFormat.Nv12, Encoder = Enc(2, 2, false, Rates[2], false, true),
                                 AudioPipeName = "aud", ProgressUrl = "pipe:1", OutputUrl = "srt://127.0.0.1:9000?mode=caller&latency=1000000", LogLevel = "info" },
                commandLine: true);

            // refused plans, in the order the checks are made
            FfmpegPlan Good() => new FfmpegPlan { Width = 1920, Height = 1080, Encoder = Enc(0, 1, true, Rates[0], true, true) };
            FfmpegPlan p;
            p = Good(); p.Width = 0; Add(p);
            p = Good(); p.Height = 0; Add(p);
            p = Good(); p.Width = 1921; Add(p);
            p = Good(); p.Height = 1081; Add(p);
            p = Good(); p.Fps = 0; Add(p);
            p = Good(); p.OutputUrl = ""; Add(p);
            p = Good(); p.PixelFormat = CaptureFormat.Auto; Add(p);
            p = Good(); p.PixelFormat = CaptureFormat.Nv12; Add(p);
            p = Good(); p.Encoder.Hdr = false; Add(p);
            p = Good(); p.Width = 0; p.Fps = 0; p.OutputUrl = ""; Add(p);
            p = Good(); p.Width = 1921; p.PixelFormat = CaptureFormat.Auto; Add(p);
            return list;
        }

        static List<object?> Probe()
        {
            var list = new List<object?>();
            foreach (int encoder in new[] { 0, 2, 4, 5 })
                foreach (bool hdr in new[] { false, true })
                {
                    // what EncoderProbe.ProbeAll passes
                    var s = new EncoderSettings { Encoder = (VideoEncoder)encoder, Hdr = hdr };
                    list.Add(Pair(D("encoder", EncIn(s), "size", null), L(FfmpegCommandBuilder.BuildProbe(s))));
                    foreach (int preset in new[] { 0, 2 })
                    {
                        EncoderSettings t = Enc(encoder, preset, hdr, Rates[2], preset == 0, preset != 0);
                        list.Add(Pair(D("encoder", EncIn(t), "size", L(new[] { 320, 240 })), L(FfmpegCommandBuilder.BuildProbe(t, 320, 240))));
                    }
                }
            return list;
        }

        static object? Block(FfmpegProgress b) => D(
            "frame", b.Frame, "fps", b.Fps, "bitrateKbps", b.BitrateKbps, "totalSizeBytes", b.TotalSizeBytes, "outTimeUs", b.OutTimeUs,
            "dupFrames", b.DupFrames, "dropFrames", b.DropFrames, "speed", b.Speed, "ended", b.Ended);

        static List<object?> Progress()
        {
            var list = new List<object?>();
            string[][] runs =
            {
                new[] { "frame=1234", "fps=59.94", "stream_0_0_q=23.0", "bitrate=39876.5kbits/s", "total_size=12345678", "out_time_us=20566666",
                        "out_time_ms=20566666", "out_time=00:00:20.566666", "dup_frames=3", "drop_frames=1", "speed=1.001x", "progress=continue" },
                new[] { "bitrate=N/A", "speed=N/A", "total_size=N/A", "out_time_us=N/A", "progress=continue" },
                new[] { "frame=10", "progress=end" },
                new[] { "this is not progress output", "", "=nothing", "  frame = 7  ", "frame=N/A", "fps= 30.5 ", "bitrate=  39.8kbits/s", "speed= 1e+03x",
                        "dup_frames=x", "progress=continue", "frame=9", "progress=continue", "fps=12", "progress=end", "progress=continue" },
                new[] { "speed=infx", "bitrate=nankbits/s", "fps=inf", "frame=+12", "drop_frames=-1", "progress=continue" },
                new[] { "bitrate=1500bits/s", "speed=2X", "fps=1,5", "frame=1.0", "bitrate=KBITS/S", "progress=continue",
                        "bitrate=12.5KBits/s", "speed=x", "fps=.5", "frame= 3", "progress= end " },
                new[] { "fps=1e2", "speed=0.5", "bitrate=7kbits/s extra", "total_size=99999999999", "out_time_ms=5", "progress=whatever" },
            };
            foreach (string[] run in runs)
            {
                var parser = new FfmpegProgressParser();
                var blocks = new List<object?>();
                foreach (string line in run)
                {
                    FfmpegProgress? b = parser.Feed(line);
                    blocks.Add(b == null ? null : Block(b));
                }
                list.Add(Pair(L(run), blocks));
            }
            return list;
        }

        static List<object?> ProgressLine()
        {
            var list = new List<object?>();
            foreach (string line in new[]
            {
                "frame=120", "out_time_us=4000000", "stream_0_0_q=23.0", "progress=continue",
                "[hevc_nvenc @ 000001] Lookahead depth set to 20 = too deep", "Last message repeated 3 times", "Some Message=with caps",
                "=nothing", "", "a=", "a_1=b=c", " frame=1", "frame =1", "FRAME=1", "x", "fps=59.9 q=20.0", "ünï=1", "0=1", "_=_",
            })
                list.Add(Pair(line, FfmpegProgressParser.IsProgressLine(line)));
            return list;
        }

        static List<object?> FirstError()
        {
            var list = new List<object?>();
            foreach (string output in new[]
            {
                "[hevc_nvenc @ 0x1] Cannot load nvEncodeAPI64.dll\n[vost#0:0/hevc_nvenc @ 0x2] Could not open encoder before EOF\n[out#0/null @ 0x3] Nothing was written into output file, because at least one of its streams received no packets.\n",
                "something odd happened\n\n",
                "\n\n",
                "",
                "it went wrong\r\nframe=    0 fps=0.0 q=0.0 size=       0KiB\r\n",
                "FRAME=1\n",
                "[hevc_amf @ 000002d7] Error setting option quality to value nonsense.\r\n[vost#0:0/hevc_amf @ 000002d7] Error applying encoder options: Invalid argument\r\nError opening output file -.\r\n",
                "[vost#0:0/hevc_qsv @ 00000209] [enc:hevc_qsv @ 00000209] Could not open encoder before EOF\r\n[vost#0:0/hevc_qsv @ 00000209] Task finished with error code: -22 (Invalid argument)\r\n",
                "Unrecognized option 'bogus_opt'.\r\nError splitting the argument list: Option not found\r\n",
                "first line\nsecond line\n   \n",
                "[hevc_nvenc @ 1] No capable devices found\n",
                "The encoder REQUIRES a newer driver\nsomething else\n",
                "Invalid " + new string('x', 400) + "\n",
                "(timed out after 30000 ms)\n",
            })
                list.Add(Pair(output, EncoderProbe.FirstError(output)));
            return list;
        }

        static readonly string[] SenderFiles =
        {
            "{}",
            "{\"encoder\":1,\"bitrateMbps\":50,\"mode\":0,\"host\":\"192.168.1.201\",\"port\":8890,\"path\":\"screen\",\"user\":\"sender\",\"password\":\"pw\",\"autoRestart\":true}",
            "{\"audio\":true}",
            "{\"outputIndex\":1,\"outputDeviceName\":\"\\\\\\\\.\\\\DISPLAY2\",\"outputFriendlyName\":\"AW3225QF\",\"fps\":120,\"scale\":1,\"hdrMode\":1,\"cursor\":false,\"sdrWhite\":200,\"encoder\":5,\"quality\":2,\"bitrateMbps\":80,\"gopSeconds\":1.5,\"bufferSeconds\":2,\"bFrames\":false,\"lookahead\":true,\"audio\":true,\"audioDevice\":\"{0.0.0.00000000}.{6f1b7a3c-0000-4000-8000-000000000001}\",\"audioCodec\":2,\"audioBitrateKbps\":128,\"audioExcludeApp\":\"Discord.exe\",\"maxViewers\":6,\"uploadMbps\":1000,\"ffmpegPath\":\"D:\\\\HdrCast-0.2.4\\\\tools\\\\ffmpeg.exe\"}",
            "{\"outputIndex\":-3,\"fps\":0,\"sdrWhite\":0.5,\"bitrateMbps\":-4,\"gopSeconds\":0,\"bufferSeconds\":0.1,\"audioBitrateKbps\":8,\"audioDevice\":\"  \",\"maxViewers\":0,\"uploadMbps\":-1}",
            "{\"fps\":481,\"sdrWhite\":20000,\"bitrateMbps\":9999,\"gopSeconds\":99,\"bufferSeconds\":60,\"audioBitrateKbps\":9000,\"maxViewers\":99,\"uploadMbps\":2000000}",
            "{\"fps\":480,\"sdrWhite\":10000,\"bitrateMbps\":500,\"gopSeconds\":10,\"bufferSeconds\":10,\"audioBitrateKbps\":512,\"maxViewers\":16,\"uploadMbps\":100000}",
            "{\"fps\":1,\"sdrWhite\":1,\"bitrateMbps\":1,\"gopSeconds\":0.25,\"bufferSeconds\":0.5,\"audioBitrateKbps\":32,\"maxViewers\":1}",
            "{\"audioExcludeApp\":\"  Discord \",\"encoder\":2}",
            "{\"audioExcludeApp\":\"C:\\\\Apps\\\\Voice Chat\\\\TeamSpeak.EXE\",\"encoder\":4}",
            "{\"audioExcludeApp\":\"bad|name\",\"encoder\":0}",
            "{\"encoder\":3,\"fps\":30}",
            "{\"encoder\":7,\"fps\":30}",
            "{\"encoder\":6,\"fps\":30}",
            "{\"encoder\":-1,\"fps\":30}",
            "{\"scale\":2,\"hdrMode\":0,\"quality\":0,\"audioCodec\":0,\"audio\":false,\"cursor\":true}",
            "{\"scale\":3,\"quality\":1,\"audioCodec\":1,\"bFrames\":true,\"lookahead\":false}",
            "{\"outputDeviceName\":null,\"outputFriendlyName\":null,\"audioDevice\":null,\"audioExcludeApp\":null,\"ffmpegPath\":null,\"fps\":30}",
            "{\"fps\":30,\"bitrateMbps\":25,\"gopSeconds\":1,\"bufferSeconds\":3,\"unknown\":{\"nested\":[1,2,{\"x\":null}]},\"another\":\"x\"}",
            "{\"sdrWhite\":203.5,\"gopSeconds\":2.5,\"bufferSeconds\":1.25,\"outputFriendlyName\":\"显示器 ディスプレイ\"}",
            // not what the serializer wrote: does it still read?
            "{\"fps\":\"sixty\"}",
            "{\"fps\":60.5}",
            "{\"fps\":6e1}",
            "{\"fps\":\"60\"}",
            "{\"fps\":null}",
            "{\"cursor\":1}",
            "{\"cursor\":\"true\"}",
            "{\"encoder\":\"hevc_nvenc\"}",
            "{\"encoder\":2.0}",
            "{\"encoder\":\"2\"}",
            "{\"fps\":60.0}",
            "{\"fps\":\" 60 \"}",
            "{\"cursor\":0}",
            "{\"cursor\":2}",
            "{\"cursor\":\"yes\"}",
            "{\"cursor\":null}",
            "{\"gopSeconds\":null}",
            "{\"gopSeconds\":true}",
            "{\"audioDevice\":5}",
            "{\"audioDevice\":true}",
            " \r\n\t{\"fps\":30}\r\n",
            "{\"gopSeconds\":\"2.5\"}",
            "{\"gopSeconds\":3}",
            "{\"bitrateMbps\":4294967296}",
            "{\"fps\":30,}",
            "{\"fps\":30} trailing",
            "[1,2,3]",
            "null",
            "\"sender\"",
            "",
            "{\"fps\": 30,",
            "{\"fps\":30,\"fps\":50}",
            "{\"FPS\":30,\"Fps\":31}",
        };

        static List<object?> SenderSettingsVectors()
        {
            var list = new List<object?>();
            foreach (string file in SenderFiles)
            {
                SenderSettings? s;
                try { s = Json.Deserialize<SenderSettings>(file); }
                catch (Exception) { list.Add(Pair(file, D("unreadable", true))); continue; }
                if (s == null) { list.Add(Pair(file, D("unreadable", true))); continue; }
                var validateBefore = L(s.Validate());
                s.Normalize();
                list.Add(Pair(file, D(
                    "validateBeforeNormalize", validateBefore,
                    "normalized", Json.Serialize(s),
                    "validate", L(s.Validate()),
                    "text", s.ToString(),
                    "bitrateBps", s.BitrateBps,
                    "scaleHeight", s.ScaleHeight,
                    "formatOnHdrMonitor", s.ResolveFormat(true).ToString().ToLowerInvariant(),
                    "formatOnSdrMonitor", s.ResolveFormat(false).ToString().ToLowerInvariant(),
                    "videoArgsHdr1015", L(EncoderProfiles.BuildVideoArgs(s.ToEncoderSettings(true, 1015))),
                    "videoArgsSdr", L(EncoderProfiles.BuildVideoArgs(s.ToEncoderSettings(false, 0))))));
            }
            return list;
        }

        static List<object?> ResolveSize()
        {
            var list = new List<object?>();
            int[][] sources =
            {
                new[] { 3840, 2160 }, new[] { 3440, 1440 }, new[] { 2560, 1080 }, new[] { 1366, 769 }, new[] { 1920, 1200 }, new[] { 5120, 1440 },
                new[] { 1280, 720 }, new[] { 1, 1 }, new[] { 0, 0 }, new[] { 7681, 4321 }, new[] { 2160, 3840 }, new[] { 1081, 1441 },
                new[] { 1707, 1081 }, new[] { 2561, 1439 }, new[] { 3, 1443 }, new[] { -5, 2000 }, new[] { 1920, -1 }, new[] { 2559, 1601 },
            };
            foreach (int scale in new[] { 0, 1, 2, 3 })
                foreach (int[] src in sources)
                {
                    var s = new SenderSettings { Scale = (ScaleMode)scale };
                    s.ResolveSize(src[0], src[1], out int w, out int h);
                    list.Add(Pair(D("scale", scale, "source", L(src)), L(new[] { w, h })));
                }
            return list;
        }

        static List<object?> CaptureArgsVectors()
        {
            var list = new List<object?>();
            void Add(string settingsJson, int outputIndex, int width, int height, CaptureFormat format, string videoPipe, string audioPipe, int parentPid)
            {
                SenderSettings s = Json.Deserialize<SenderSettings>(settingsJson)!.Normalize();
                CaptureArgs a = s.ToCaptureArgs(new OutputInfo { Index = outputIndex }, width, height, format, videoPipe, audioPipe, parentPid);
                list.Add(Pair(D("settings", settingsJson, "outputIndex", outputIndex, "width", width, "height", height,
                                "format", format.ToString().ToLowerInvariant(), "videoPipe", videoPipe, "audioPipe", audioPipe, "parentPid", parentPid),
                              L(a.ToArgv())));
            }
            Add("{}", 0, 3840, 2160, CaptureFormat.P010, "hdrcast-video-4321", "hdrcast-audio-4321", 4321);
            Add("{\"fps\":120,\"cursor\":false,\"sdrWhite\":120,\"audio\":true}", 2, 2560, 1440, CaptureFormat.P010, "vid", "aud", 4321);
            Add("{\"fps\":30,\"audio\":false,\"sdrWhite\":203.5}", 1, 1920, 1080, CaptureFormat.Nv12, "v", "a", 1);
            Add("{\"audioExcludeApp\":\"  Discord \",\"audioDevice\":\"{0.0.0.00000000}.{6f1b7a3c}\"}", 0, 1920, 1080, CaptureFormat.P010, "v", "a", 99999);
            Add("{\"sdrWhite\":80.25,\"audioExcludeApp\":\"C:\\\\Apps\\\\TeamSpeak.EXE\"}", 3, 1280, 720, CaptureFormat.Auto, "v-1", "a-1", 12);
            Add("{\"sdrWhite\":123.4567}", 0, 0, 0, CaptureFormat.Auto, "v", "a", 0);
            return list;
        }

        // ---- receiver -----------------------------------------------------------------------------

        static readonly string[] ReceiverFiles =
        {
            "{}",
            "{\"mode\":0,\"host\":\"relay.example.net\",\"port\":8890,\"path\":\"screen\",\"user\":\"viewer\",\"password\":\"pw\",\"passphrase\":\"0123456789abcdef\",\"latencyMs\":3500,\"expectedBitrateMbps\":80}",
            "{\"latencyMs\":3500,\"expectedBitrateMbps\":80,\"displayMode\":2,\"toneMapping\":\"spline\",\"hwDec\":2,\"bufferSeconds\":2.5,\"fullscreen\":true,\"fullscreenScreen\":1,\"frontend\":1,\"mpvPath\":\"D:\\\\HdrCast-0.2.4\\\\tools\\\\mpv.exe\",\"ffmpegPath\":\"\",\"autoCatchUp\":false,\"targetPeakNits\":400,\"lastHost\":\"q83vEjRWeJA_dzSLwjkZeg\"}",
            "{\"latencyMs\":5,\"expectedBitrateMbps\":0,\"bufferSeconds\":0,\"fullscreenScreen\":-1,\"toneMapping\":\"  \",\"lastHost\":\"  abc  \"}",
            "{\"latencyMs\":99999,\"expectedBitrateMbps\":5000,\"bufferSeconds\":99,\"targetPeakNits\":99999}",
            "{\"latencyMs\":20,\"bufferSeconds\":0.05,\"targetPeakNits\":9}",
            "{\"latencyMs\":19,\"bufferSeconds\":0.1,\"targetPeakNits\":10}",
            "{\"latencyMs\":30000,\"expectedBitrateMbps\":1000,\"bufferSeconds\":30,\"targetPeakNits\":10000}",
            "{\"latencyMs\":2999}",
            "{\"latencyMs\":3000}",
            "{\"latencyMs\":300}",
            "{\"targetPeakNits\":5}",
            "{\"targetPeakNits\":-3}",
            "{\"displayMode\":9,\"hwDec\":42,\"frontend\":2,\"latencyMs\":2000}",
            "{\"displayMode\":1,\"hwDec\":0,\"frontend\":0}",
            "{\"displayMode\":0,\"hwDec\":3,\"toneMapping\":\" hable \"}",
            "{\"toneMapping\":null,\"mpvPath\":null,\"ffmpegPath\":null,\"lastHost\":null,\"latencyMs\":2000}",
            "{\"bufferSeconds\":-1}",
            "{\"bufferSeconds\":1.23456}",
            "{\"latencyMs\":\"x\"}",
            "{\"fullscreen\":\"yes\"}",
            "[1,2,3]",
            "",
        };

        static List<object?> ReceiverSettingsVectors()
        {
            var list = new List<object?>();
            foreach (string file in ReceiverFiles)
            {
                ReceiverSettings? s;
                try { s = Json.Deserialize<ReceiverSettings>(file); }
                catch (Exception) { list.Add(Pair(file, D("unreadable", true))); continue; }
                if (s == null) { list.Add(Pair(file, D("unreadable", true))); continue; }
                var validateBefore = L(s.Validate());
                int overheadBefore = s.OverheadPercent;
                s.Normalize();
                string? url;
                try { url = MpvCommandBuilder.BuildUrl(s, 40123, "0123456789abcdef"); } catch (Exception) { url = null; }
                list.Add(Pair(file, D(
                    "validateBeforeNormalize", validateBefore,
                    "overheadBeforeNormalize", overheadBefore,
                    "normalized", Json.Serialize(s),
                    "validate", L(s.Validate()),
                    "overheadPercent", s.OverheadPercent,
                    "url", url)));
            }
            return list;
        }

        static object? MpvOut(ReceiverSettings s, int port, string passphrase, int pid, string mpv, string? ffmpeg, bool commandLines)
        {
            try
            {
                MpvLaunchPlan plan = MpvCommandBuilder.Build(s, port, passphrase, pid, mpv, ffmpeg);
                Dictionary<string, object?> o = D("mpvArgs", L(plan.MpvArgs), "pipeName", plan.PipeName, "url", plan.Url,
                                                  "ffmpegArgs", plan.FfmpegArgs == null ? null : L(plan.FfmpegArgs),
                                                  "usesFfmpegPipe", plan.UsesFfmpegPipe);
                // the command lines repeat the arguments: only some vectors carry them
                if (commandLines)
                {
                    o["mpvCommandLine"] = plan.MpvCommandLine;
                    o["ffmpegCommandLine"] = plan.FfmpegCommandLine;
                    o["commandLine"] = plan.ToCommandLine();
                }
                return o;
            }
            catch (ArgumentException e)
            {
                return D("error", Message(e));
            }
        }

        static List<object?> Mpv()
        {
            var list = new List<object?>();
            void Add(ReceiverSettings s, bool normalize, int port = 40123, string passphrase = "0123456789abcdef", int pid = 4321,
                     string mpv = @"C:\Program Files\HdrCast\tools\mpv.exe", string? ffmpeg = @"C:\Program Files\HdrCast\tools\ffmpeg.exe",
                     bool commandLines = false)
            {
                string settings = Json.Serialize(s);
                if (normalize) s.Normalize();
                list.Add(Pair(D("settings", settings, "normalize", normalize, "port", port, "passphrase", passphrase, "pid", pid, "mpv", mpv, "ffmpeg", ffmpeg),
                              MpvOut(s, port, passphrase, pid, mpv, ffmpeg, commandLines)));
            }

            foreach (int display in new[] { 0, 1, 2 })
                foreach (int hwdec in new[] { 0, 1, 2, 3 })
                    foreach (bool fullscreen in new[] { false, true })
                        foreach (int frontend in new[] { 0, 1 })
                            Add(new ReceiverSettings
                            {
                                LatencyMs = 1500, ExpectedBitrateMbps = 60, DisplayMode = (DisplayMode)display, HwDec = (HwDecMode)hwdec,
                                Fullscreen = fullscreen, FullscreenScreen = 1, Frontend = (ReceiverFrontend)frontend,
                                TargetPeakNits = display == 2 ? 400 : 0,
                            }, true, commandLines: display == 2 && hwdec == 1);

            // numbers as "0.###" writes them
            foreach (double buffer in new[] { 1.5, 0.3, 2.5, 1.0, 2.0, 1.0005, 1.23456, 7.77777, 30, 0.1, 0.25, 0.125, 0.0625, 3.3335, 2.00049, 0.4999, 12.3455, 0.05, 0, -1, 45 })
                Add(new ReceiverSettings { BufferSeconds = buffer }, buffer > 0 && buffer <= 30);
            foreach (int latency in new[] { 300, 20, 1500, 2000, 3500, 30000 })
                foreach (int mbps in new[] { 1, 40, 100, 1000 })
                    Add(new ReceiverSettings { LatencyMs = latency, ExpectedBitrateMbps = mbps }, true);
            foreach (string tone in ReceiverSettings.ToneMappingAlgorithms.Concat(new[] { " hable ", "", "  " }))
                Add(new ReceiverSettings { DisplayMode = DisplayMode.SdrToneMap, ToneMapping = tone }, false);
            foreach (int peak in new[] { 0, 5, 9, 10, 400, 10000, 99999, -3 })
            {
                Add(new ReceiverSettings { DisplayMode = DisplayMode.SdrToneMap, TargetPeakNits = peak }, true);
                Add(new ReceiverSettings { DisplayMode = DisplayMode.HdrPassthrough, TargetPeakNits = peak }, false);
            }
            foreach (int screen in new[] { -2, 0, 3, 32 })
                Add(new ReceiverSettings { Fullscreen = true, FullscreenScreen = screen }, false);

            // paths with spaces, another pid and port, the group passphrase of the test vectors, no passphrase
            Add(new ReceiverSettings(), true, 51234, "a77MWmBi3_603E3kH2abfOI5O-dIU5dn", 1, "mpv", null, commandLines: true);
            Add(new ReceiverSettings { Frontend = ReceiverFrontend.FfmpegPipe }, true, 1, "", 99999, @"D:\my tools\mpv.exe", @"D:\my tools\ffmpeg.exe", commandLines: true);
            Add(new ReceiverSettings(), true, 65535, "", 7, "mpv", null, commandLines: true);
            // refused
            Add(new ReceiverSettings(), true, 40123, "0123456789abcdef", 1, "", null);
            Add(new ReceiverSettings { Frontend = ReceiverFrontend.FfmpegPipe }, true, 40123, "0123456789abcdef", 1, "mpv", null);
            Add(new ReceiverSettings { Frontend = ReceiverFrontend.FfmpegPipe }, true, 40123, "0123456789abcdef", 1, "mpv", "");
            return list;
        }

        static readonly string[] IpcLines =
        {
            "{\"event\":\"property-change\",\"id\":1,\"name\":\"paused-for-cache\",\"data\":true}",
            "{\"error\":\"success\",\"data\":1.5,\"request_id\":7}",
            "{\"error\":\"property unavailable\",\"request_id\":8}",
            "not json",
            "",
            "   \t",
            "[1,2]",
            "42",
            "\"text\"",
            "null",
            "{\"event\":",
            "{\"event\":\"end-file\",\"reason\":\"error\",\"playlist_entry_id\":1,\"file_error\":\"loading failed\"}",
            " {\"something\":1} trailing",
            "{\"event\":5,\"error\":null}",
            "{\"event\":\"property-change\",\"id\":\"12\",\"name\":\"fullscreen\",\"data\":\"yes\"}",
            "{\"event\":\"property-change\",\"id\":2.6,\"name\":7,\"data\":null}",
            "{\"event\":\"property-change\",\"id\":3.5,\"name\":\"speed\"}",
            "{\"request_id\":9}",
            "{\"request_id\":\"10\",\"error\":7}",
            "{\"event\":\"idle\"}",
            "{\"event\":\"property-change\",\"id\":7,\"name\":\"video-params\",\"data\":{\"pixelformat\":\"p010\",\"w\":3840,\"h\":2160}}",
            "{\"event\":\"property-change\",\"id\":13,\"name\":\"mouse-pos\",\"data\":{\"x\":640,\"y\":360,\"hover\":true}}",
            "{\"data\":[1,\"two\",3.5,null,false],\"error\":\"success\",\"request_id\":9223372036854775807}",
            "{}",
            "{\"event\":\"log-message\",\"prefix\":\"cplayer\",\"level\":\"info\",\"text\":\"Playing: srt://127.0.0.1:40123\\n\"}",
        };

        static List<object?> IpcMessage()
        {
            var list = new List<object?>();
            foreach (string line in IpcLines)
            {
                MpvIpcMessage? m = MpvIpcMessage.TryParseLine(line);
                list.Add(Pair(line, m == null ? null : D(
                    "kind", (int)m.Kind, "event", m.Event, "name", m.Name, "id", m.Id, "requestId", m.RequestId, "error", m.Error,
                    "reason", m.Reason, "data", MiniJson.Serialize(m.Data), "isPropertyChange", m.IsPropertyChange, "isSuccess", m.IsSuccess,
                    "rawKeys", L(m.Raw.Keys))));
            }
            return list;
        }

        static List<object?> IpcCommand()
        {
            var list = new List<object?>();
            list.Add(Pair("observe_property 3 demuxer-cache-duration", MpvIpcCommands.ObserveProperty(3, "demuxer-cache-duration")));
            list.Add(Pair("get_property mouse-pos 4", MpvIpcCommands.GetProperty("mouse-pos", 4)));
            list.Add(Pair("set_property fullscreen true 9", MpvIpcCommands.SetProperty("fullscreen", true, 9)));
            list.Add(Pair("set_property fullscreen false", MpvIpcCommands.SetProperty("fullscreen", false)));
            list.Add(Pair("set_property speed 1.01", MpvIpcCommands.SetProperty("speed", 1.01)));
            list.Add(Pair("set_property volume 50 2", MpvIpcCommands.SetProperty("volume", 50L, 2)));
            list.Add(Pair("set_property title text", MpvIpcCommands.SetProperty("title", "a \"quoted\"\nline / ünï \\ tab\t", 3)));
            list.Add(Pair("set_property aid null", MpvIpcCommands.SetProperty("aid", null)));
            list.Add(Pair("cycle pause 5", MpvIpcCommands.CycleProperty("pause", 5)));
            list.Add(Pair("cycle pause", MpvIpcCommands.CycleProperty("pause")));
            list.Add(Pair("quit", MpvIpcCommands.Quit()));
            list.Add(Pair("quit 6", MpvIpcCommands.Quit(6)));
            list.Add(Pair("command keypress MBTN_LEFT 21", MpvIpcCommands.Command(21, "keypress", "MBTN_LEFT")));
            list.Add(Pair("command show-text text 1500", MpvIpcCommands.Command(null, "show-text", "hello", 1500L)));
            list.Add(Pair("command (empty)", MpvIpcCommands.Command(null)));
            return list;
        }

        static object? Params(MpvVideoParams? p) => p == null ? null : D(
            "describe", p.Describe(), "isHdr", p.IsHdr, "pixelFormat", p.PixelFormat, "width", p.Width, "height", p.Height,
            "colorMatrix", p.ColorMatrix, "primaries", p.Primaries, "gamma", p.Gamma, "colorLevels", p.ColorLevels,
            "maxLuma", p.MaxLuma, "minLuma", p.MinLuma, "sigPeak", p.SigPeak);

        static object? StatusOut(MpvStatusModel st, List<object?> applied) => D(
            "applied", applied, "cacheSeconds", st.CacheSeconds, "pausedForCache", st.PausedForCache, "bufferingPercent", st.BufferingPercent,
            "frameDrops", st.FrameDrops, "decoderFrameDrops", st.DecoderFrameDrops, "fps", st.Fps, "hwDec", st.HwDec,
            "source", Params(st.Source), "target", Params(st.Target), "videoBitrateBps", st.VideoBitrateBps, "speed", st.Speed,
            "fullscreen", st.Fullscreen, "updated", st.LastUpdateUtc.HasValue,
            "describeBitrate", st.DescribeBitrate(), "describeCache", st.DescribeCache());

        static string Change(int id, string name, string data) =>
            "{\"event\":\"property-change\",\"id\":" + id + ",\"name\":\"" + name + "\",\"data\":" + data + "}";

        static List<object?> Status()
        {
            var list = new List<object?>();
            void Add(params string[] lines)
            {
                var st = new MpvStatusModel();
                var applied = new List<object?>();
                foreach (string line in lines)
                {
                    MpvIpcMessage? m = MpvIpcMessage.TryParseLine(line);
                    applied.Add(m != null && st.Apply(m));
                }
                list.Add(Pair(L(lines), StatusOut(st, applied)));
            }

            Add();
            Add(Change(1, "demuxer-cache-duration", "2.5"), Change(2, "paused-for-cache", "true"), Change(10, "cache-buffering-state", "27"),
                Change(3, "frame-drop-count", "4"), Change(4, "decoder-frame-drop-count", "1"), Change(5, "estimated-vf-fps", "59.94"),
                Change(6, "hwdec-current", "\"d3d11va\""), Change(9, "video-bitrate", "39876543"), Change(11, "speed", "1.05"),
                Change(12, "fullscreen", "true"),
                Change(7, "video-params", "{\"pixelformat\":\"p010\",\"w\":3840,\"h\":2160,\"colormatrix\":\"bt.2020-ncl\",\"primaries\":\"bt.2020\",\"gamma\":\"pq\",\"colorlevels\":\"limited\",\"max-luma\":1000.0,\"min-luma\":0.0001,\"sig-peak\":4.926}"),
                Change(8, "video-target-params", "{\"primaries\":\"bt.709\",\"gamma\":\"bt.1886\",\"sig-peak\":1.0}"),
                "{\"event\":\"file-loaded\"}", Change(99, "volume", "50"), "{\"error\":\"success\",\"request_id\":1}", "garbage");
            Add(Change(1, "demuxer-cache-duration", "null"), Change(7, "video-params", "null"), Change(2, "paused-for-cache", "\"no\""),
                Change(11, "speed", "null"), Change(6, "hwdec-current", "null"), Change(10, "cache-buffering-state", "null"));
            Add(Change(2, "paused-for-cache", "\"yes\""), Change(1, "demuxer-cache-duration", "0.4"));
            Add(Change(2, "paused-for-cache", "true"), Change(10, "cache-buffering-state", "99.5"), Change(1, "demuxer-cache-duration", "\"1.005\""),
                Change(11, "speed", "\"1.0011\""), Change(3, "frame-drop-count", "\"12\""), Change(4, "decoder-frame-drop-count", "2.5"),
                Change(12, "fullscreen", "\"yes\""), Change(6, "hwdec-current", "5"));
            // "0.00", "0.0", "0" and "0.##" on halves and near-halves
            foreach (string cache in new[] { "0.125", "0.375", "2.675", "1.005", "0.004", "0.005", "0.015", "9.995", "123.456", "0", "-0.001", "-1.5", "1e-7", "100" })
                Add(Change(1, "demuxer-cache-duration", cache));
            foreach (string speed in new[] { "1.0005", "1.001", "1.0011", "0.999", "0.9989", "1.005", "1.015", "1.125", "2", "0.5", "100", "0" })
                Add(Change(11, "speed", speed), Change(1, "demuxer-cache-duration", "1"));
            foreach (string bitrate in new[] { "39876543", "39950000", "50000", "49999", "150000", "250000", "350000", "0", "1000000000", "40000000.0", "1234567.89", "\"8000000\"" })
                Add(Change(9, "video-bitrate", bitrate));
            foreach (string luma in new[] { "1000.0", "0.5", "1.5", "2.5", "203", "999.5", "10000", "0.4999" })
                Add(Change(7, "video-params", "{\"max-luma\":" + luma + ",\"sig-peak\":2.0}"));
            foreach (string peak in new[] { "4.926", "1.0", "1.005", "1.125", "0.004", "0.005", "12.345", "3", "0.995" })
                Add(Change(8, "video-target-params", "{\"gamma\":\"hlg\",\"sig-peak\":" + peak + "}"));
            Add(Change(7, "video-params", "{}"), Change(8, "video-target-params", "\"x\""));
            Add(Change(7, "video-params", "{\"gamma\":\"hlg\",\"w\":1920,\"sig-peak\":3.456}"),
                Change(8, "video-target-params", "{\"w\":\"1920\",\"h\":1080.4,\"pixelformat\":5,\"primaries\":\"bt.709\",\"max-luma\":\"203\"}"));
            return list;
        }

        static readonly string[] LogLinesSingle =
        {
            "", "Playing: srt://...", "[ffmpeg/demuxer] srt: no stream is available on path 'screen'",
            "[ffmpeg] srt: Connection setup failure: authentication failed",
            "[ffmpeg] srt: Connection to srt://127.0.0.1:8890?streamid=read:screen:viewer:pw&passphrase=abcdefghij1234&latency=1500000 failed: Input/output error",
            "Failed to open srt://127.0.0.1:8890?streamid=read:screen:viewer:pw&passphrase=abcdefghij1234&latency=1500000.",
            "[ffmpeg/demuxer] srt://h:8890?passphrase=abcdefghij: no stream is available on path 'screen'",
            "[ffmpeg] srt: closed: invalid passphrase", "[ffmpeg] srt: closed: passphrase not provided by client",
            "[ffmpeg] srt: failed to authenticate: authentication failed", "Failed to open srt://h:8890?passphrase=x now.",
            "Exiting... (Quit)", "Exiting... (End of file)", "Exiting... (Errors when loading file)", "exiting... (quit)",
            "[ffmpeg] SRT: CONNECTION TIMED OUT", "srt: km_badsecret", "[ffmpeg] tcp: Connection refused", "[ffmpeg] Protocol not found",
            "srt protocol not on whitelist 'file,crypto'!", "Unsupported protocol", "Unrecognized URL", "no one is publishing", "bad password",
            "badsecret", "wrong password", "wrong passphrase", "unauthorized", "Connection setup failure", "connection rejected", "Connection reset by peer",
            "Failed to recognize file format.", "No route to host", "Name or service not known", "getaddrinfo failed", "the request timed out",
            "passphrase", "streamid=read:screen latency=1", "no url here", "srt://h:1?passphrase=x", "a srt://h:1?p=1 b http://x/y c",
            "open(rist+udp://h:1) failed", "://", "x :// y", "连接 srt://主机:1?passphrase=口令\t失败", "a://b://c d", "连接srt://h:1 timed out",
            "[cplayer] Opening failed or was aborted: srt://127.0.0.1:40123?mode=caller&passphrase=secretsecret&latency=1500000",
            "Failed to open srt://h:1?x=Connection refused", "url at end srt://", "tab\tsrt://a\tb", "wrong password for http://unauthorized/",
        };

        static List<object?> LogLine()
        {
            var list = new List<object?>();
            foreach (string line in LogLinesSingle)
                list.Add(Pair(line, D("reason", (int)MpvLogClassifier.ClassifyLine(line), "stripped", MpvLogClassifier.StripUrls(line),
                                      "unavailable", MpvLogClassifier.IsStreamUnavailable(line))));
            return list;
        }

        static List<object?> LogLines()
        {
            var list = new List<object?>();
            string[][] sets =
            {
                new string[0],
                new[] { "", "nothing special" },
                new[] { "Exiting... (Quit)", "[ffmpeg] srt: Connection refused", "[ffmpeg] srt: invalid passphrase", "Exiting... (End of file)" },
                new[] { "Exiting... (End of file)", "[ffmpeg] srt: invalid passphrase", "[ffmpeg] srt: Connection refused", "Exiting... (Quit)" },
                new[] { "Protocol not found", "unauthorized" },
                new[] { "unauthorized", "Protocol not found" },
                new[] { "Exiting... (Quit)", "Exiting... (End of file)" },
                new[] { "Exiting... (End of file)", "Exiting... (Quit)" },
                new[] { "no stream is available", "Connection refused" },
                new[] { "Connection refused", "no stream is available", "Exiting... (Errors when loading file)" },
                new[] { "[ffmpeg] srt: Connection to srt://127.0.0.1:8890?streamid=read:screen:viewer:pw&passphrase=abcdefghij1234&latency=1500000 failed: Input/output error",
                        "Failed to open srt://127.0.0.1:8890?streamid=read:screen:viewer:pw&passphrase=abcdefghij1234&latency=1500000." },
            };
            foreach (string[] set in sets)
                list.Add(Pair(L(set), (int)MpvLogClassifier.Classify(set)));
            return list;
        }
    }
}
