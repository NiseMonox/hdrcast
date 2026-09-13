using System;
using System.Collections.Generic;
using HdrCast.Common;
using HdrCast.Receiver;
using Xunit;

namespace HdrCast.Tests
{
    public class MpvCommandTests
    {
        static ReceiverSettings Settings(DisplayMode display = DisplayMode.Auto)
        {
            var s = new ReceiverSettings
            {
                Mode = ConnectionMode.Relay,
                Host = "relay.example.net",
                Port = 8890,
                StreamPath = "screen",
                User = "viewer",
                Password = "pw",
                Passphrase = "0123456789abcdef",
                LatencyMs = 1500,
                ExpectedBitrateMbps = 60,
                DisplayMode = display,
            };
            return s.Normalize();
        }

        static string Flag(IReadOnlyList<string> args, string name)
        {
            foreach (string a in args)
                if (a.StartsWith("--" + name + "=", StringComparison.Ordinal))
                    return a.Substring(name.Length + 3);
            return "";
        }

        static bool Has(IReadOnlyList<string> args, string name)
        {
            foreach (string a in args)
                if (a == "--" + name || a.StartsWith("--" + name + "=", StringComparison.Ordinal)) return true;
            return false;
        }

        [Fact]
        public void Hdr_passthrough_needs_gpu_next_and_the_colorspace_hint()
        {
            MpvLaunchPlan plan = MpvCommandBuilder.Build(Settings(DisplayMode.HdrPassthrough), 1234, @"C:\mpv\mpv.exe");
            Assert.Equal("gpu-next", Flag(plan.MpvArgs, "vo"));
            Assert.Equal("d3d11", Flag(plan.MpvArgs, "gpu-api"));
            Assert.Equal("yes", Flag(plan.MpvArgs, "target-colorspace-hint"));
        }

        [Fact]
        public void Sdr_mode_turns_the_hint_off_and_tone_maps()
        {
            ReceiverSettings s = Settings(DisplayMode.SdrToneMap);
            MpvLaunchPlan plan = MpvCommandBuilder.Build(s, 1234, "mpv");
            Assert.Equal("no", Flag(plan.MpvArgs, "target-colorspace-hint"));
            Assert.Equal(ReceiverSettings.DefaultToneMapping, Flag(plan.MpvArgs, "tone-mapping"));
        }

        [Fact]
        public void Auto_mode_lets_mpv_decide()
        {
            MpvLaunchPlan plan = MpvCommandBuilder.Build(Settings(DisplayMode.Auto), 1234, "mpv");
            Assert.Equal("auto", Flag(plan.MpvArgs, "target-colorspace-hint"));
        }

        [Fact]
        public void Url_is_the_last_argument_and_carries_the_srt_options()
        {
            MpvLaunchPlan plan = MpvCommandBuilder.Build(Settings(), 1234, "mpv");
            string url = plan.MpvArgs[plan.MpvArgs.Count - 1];
            Assert.StartsWith("srt://relay.example.net:8890?", url);
            SrtUrlParts p = SrtUrl.Parse(url);
            Assert.Equal("read:screen:viewer:pw", SrtUrl.Decode(p.Get("streamid")!));
            Assert.Equal("1500000", p.Get("latency"));
            // Reading a 60 Mbps stream with 1.5 s of latency needs a bigger receive buffer than libsrt's default.
            Assert.True(p.Has("rcvbuf"));
            Assert.True(p.Has("ffs"));
            Assert.True(p.Has("recv_buffer_size"));
        }

        [Fact]
        public void Ipc_pipe_is_per_process()
        {
            MpvLaunchPlan plan = MpvCommandBuilder.Build(Settings(), 4321, "mpv");
            Assert.Equal("hdrcast-mpv-4321", plan.PipeName);
            Assert.Contains(MpvCommandBuilder.PipePrefix + "hdrcast-mpv-4321", plan.MpvArgs[FindIpc(plan.MpvArgs)]);
        }

        static int FindIpc(IReadOnlyList<string> args)
        {
            for (int i = 0; i < args.Count; i++)
                if (args[i].StartsWith("--input-ipc-server=", StringComparison.Ordinal)) return i;
            throw new Xunit.Sdk.XunitException("no --input-ipc-server argument");
        }

        [Fact]
        public void Cache_settings_follow_the_buffer_seconds()
        {
            ReceiverSettings s = Settings();
            s.BufferSeconds = 2.5;
            MpvLaunchPlan plan = MpvCommandBuilder.Build(s, 1, "mpv");
            Assert.True(Has(plan.MpvArgs, "cache"));
            Assert.True(Has(plan.MpvArgs, "cache-pause-initial"));
            Assert.Contains("2.5", Flag(plan.MpvArgs, "cache-pause-wait"));
        }

        [Fact]
        public void Fullscreen_targets_the_chosen_screen()
        {
            ReceiverSettings s = Settings();
            s.Fullscreen = true;
            s.FullscreenScreen = 2;
            MpvLaunchPlan plan = MpvCommandBuilder.Build(s, 1, "mpv");
            Assert.True(Has(plan.MpvArgs, "fs"));
            Assert.Equal("2", Flag(plan.MpvArgs, "fs-screen"));
        }

        [Fact]
        public void Exclusive_fullscreen_is_never_used()
        {
            // --d3d11-exclusive-fs breaks HDR passthrough on some drivers; borderless --fs is the supported path.
            MpvLaunchPlan plan = MpvCommandBuilder.Build(Settings(DisplayMode.HdrPassthrough), 1, "mpv");
            Assert.False(Has(plan.MpvArgs, "d3d11-exclusive-fs"));
        }

        [Fact]
        public void Ffmpeg_fallback_pipes_a_copy_into_mpv_stdin()
        {
            ReceiverSettings s = Settings();
            s.Frontend = ReceiverFrontend.FfmpegPipe;
            MpvLaunchPlan plan = MpvCommandBuilder.Build(s, 1, "mpv", @"C:\ff\ffmpeg.exe");
            Assert.True(plan.UsesFfmpegPipe);
            Assert.Contains("-c", plan.FfmpegArgs!);
            Assert.Contains("copy", plan.FfmpegArgs!);
            Assert.Contains("-", plan.MpvArgs);                   // mpv reads stdin
            Assert.Contains("|", plan.ToCommandLine());
        }

        [Fact]
        public void Direct_listen_mode_builds_a_listener_url()
        {
            ReceiverSettings s = Settings();
            s.Mode = ConnectionMode.DirectListen;
            MpvLaunchPlan plan = MpvCommandBuilder.Build(s, 1, "mpv");
            SrtUrlParts p = SrtUrl.Parse(plan.Url);
            Assert.Equal(SrtUrl.ListenerHost, p.Host);
            Assert.Equal("listener", p.Get("mode"));
        }
    }

    public class MiniJsonTests
    {
        [Fact]
        public void Parses_the_shapes_mpv_sends()
        {
            object? v = MiniJson.Parse("{\"event\":\"property-change\",\"id\":3,\"name\":\"demuxer-cache-duration\",\"data\":2.5}");
            Assert.Equal("property-change", MiniJson.GetString(v, "event"));
            Assert.Equal(3, MiniJson.AsLong(MiniJson.Get(v, "id")));
            Assert.Equal(2.5, MiniJson.AsDouble(MiniJson.Get(v, "data"))!.Value, 3);
        }

        [Fact]
        public void Handles_nested_objects_arrays_and_escapes()
        {
            object? v = MiniJson.Parse("{\"a\":[1,2,{\"b\":\"x\\ny\"}],\"c\":true,\"d\":null}");
            List<object?>? arr = MiniJson.AsArray(MiniJson.Get(v, "a"));
            Assert.NotNull(arr);
            Assert.Equal(3, arr!.Count);
            Assert.Equal("x\ny", MiniJson.GetString(arr[2], "b"));
            Assert.True(MiniJson.AsBool(MiniJson.Get(v, "c")));
            Assert.Null(MiniJson.Get(v, "d"));
        }

        [Fact]
        public void Round_trips_through_serialize()
        {
            var obj = new Dictionary<string, object?> { { "command", new List<object?> { "set_property", "fullscreen", true } } };
            object? back = MiniJson.Parse(MiniJson.Serialize(obj));
            List<object?>? cmd = MiniJson.AsArray(MiniJson.Get(back, "command"));
            Assert.Equal("set_property", cmd![0]);
            Assert.Equal(true, cmd[2]);
        }

        [Fact]
        public void Bad_input_does_not_throw_with_tryparse()
        {
            Assert.False(MiniJson.TryParse("{oops", out _));
            Assert.False(MiniJson.TryParse("", out _));
        }

        [Fact]
        public void Escapes_control_characters_when_writing()
        {
            string json = MiniJson.Serialize(new Dictionary<string, object?> { { "k", "a\"b\\c\nd" } });
            Assert.Contains("\\\"", json);
            Assert.Contains("\\\\", json);
            Assert.Contains("\\n", json);
        }
    }

    public class MpvIpcTests
    {
        [Fact]
        public void Property_change_is_recognised()
        {
            MpvIpcMessage? m = MpvIpcMessage.TryParseLine(
                "{\"event\":\"property-change\",\"id\":1,\"name\":\"paused-for-cache\",\"data\":true}");
            Assert.NotNull(m);
            Assert.True(m!.IsPropertyChange);
            Assert.Equal("paused-for-cache", m.Name);
            Assert.Equal(1, m.Id);
            Assert.Equal(true, m.Data);
        }

        [Fact]
        public void Reply_success_and_error_are_distinguished()
        {
            MpvIpcMessage? ok = MpvIpcMessage.TryParseLine("{\"error\":\"success\",\"data\":1.5,\"request_id\":7}");
            Assert.True(ok!.IsSuccess);
            Assert.Equal(7, ok.RequestId);

            MpvIpcMessage? bad = MpvIpcMessage.TryParseLine("{\"error\":\"property unavailable\",\"request_id\":8}");
            Assert.False(bad!.IsSuccess);
            Assert.Equal("property unavailable", bad.Error);
        }

        [Fact]
        public void Garbage_lines_are_ignored()
        {
            Assert.Null(MpvIpcMessage.TryParseLine("not json"));
            Assert.Null(MpvIpcMessage.TryParseLine(""));
            Assert.Null(MpvIpcMessage.TryParseLine(null));
        }

        [Fact]
        public void Commands_serialise_to_one_line()
        {
            string cmd = MpvIpcCommands.ObserveProperty(3, "demuxer-cache-duration");
            Assert.DoesNotContain("\n", cmd);
            Assert.Contains("observe_property", cmd);
            Assert.Contains("demuxer-cache-duration", cmd);

            string set = MpvIpcCommands.SetProperty("fullscreen", true, 9);
            Assert.Contains("set_property", set);
            Assert.Contains("\"request_id\":9", set.Replace(" ", ""));
        }
    }

    public class MpvLogClassifierTests
    {
        [Fact]
        public void Recognises_a_relay_without_a_publisher()
        {
            Assert.True(MpvLogClassifier.IsStreamUnavailable(
                "[ffmpeg/demuxer] srt: no stream is available on path 'screen'"));
        }

        [Fact]
        public void Classifies_authentication_and_passphrase_problems()
        {
            MpvExitReason auth = MpvLogClassifier.Classify(new[]
            {
                "[ffmpeg] srt: Connection setup failure: authentication failed",
            });
            Assert.NotEqual(MpvExitReason.Unknown, auth);
            Assert.False(string.IsNullOrEmpty(MpvLogClassifier.Describe(auth)));
        }

        [Fact]
        public void Unknown_lines_stay_unknown()
        {
            Assert.Equal(MpvExitReason.Unknown, MpvLogClassifier.ClassifyLine("Playing: srt://..."));
        }
    }

    public class ReceiverSettingsTests
    {
        [Fact]
        public void Defaults_point_at_a_relay_on_8890()
        {
            var s = new ReceiverSettings();
            Assert.Equal(ConnectionMode.Relay, s.Mode);
            Assert.Equal(8890, s.Port);
            Assert.Equal("screen", s.StreamPath);
        }

        [Fact]
        public void Validation_reports_the_usual_mistakes()
        {
            var s = new ReceiverSettings { Host = "", Passphrase = "abc", LatencyMs = 5, ExpectedBitrateMbps = 0 };
            List<string> problems = s.Validate();
            Assert.Contains(problems, p => p.Contains("Host"));
            Assert.Contains(problems, p => p.Contains("Passphrase"));
            Assert.Contains(problems, p => p.Contains("Latency"));
        }

        [Fact]
        public void Read_role_options_include_the_receive_buffer()
        {
            var s = new ReceiverSettings { Host = "h" };
            SrtUrlOptions o = s.ToSrtOptions();
            Assert.Equal(SrtRole.Read, o.Role);
            Assert.True(o.IncludeRecvSocketBuffer);
        }

        [Fact]
        public void Region_presets_cover_japan_australia_and_china()
        {
            var names = new List<string>();
            foreach (LatencyPreset p in ReceiverSettings.LatencyPresets) names.Add(p.Name);
            Assert.Contains("Japan", names);
            Assert.Contains("Australia", names);
            Assert.Contains("China", names);
            foreach (LatencyPreset p in ReceiverSettings.LatencyPresets) Assert.True(p.LatencyMs >= 1000);
        }
    }
}
