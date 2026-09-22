using System;
using System.Collections.Generic;
using HdrCast.Capture;
using HdrCast.Common;
using HdrCast.Sender.Model;
using Xunit;

namespace HdrCast.Tests
{
    public class EncoderProfileTests
    {
        static EncoderSettings Hevc(QualityPreset preset, bool hdr) => new EncoderSettings
        {
            Encoder = VideoEncoder.HevcNvenc,
            Preset = preset,
            Hdr = hdr,
            BitrateKbps = 40000,
            Fps = 60,
            GopSeconds = 2,
            BufferSeconds = 2,
        };

        static string Value(List<string> args, string key)
        {
            int i = args.IndexOf(key);
            return i >= 0 && i + 1 < args.Count ? args[i + 1] : "";
        }

        [Fact]
        public void Nvenc_hdr_uses_main10_and_pq_bt2020_tags()
        {
            List<string> a = EncoderProfiles.BuildVideoArgs(Hevc(QualityPreset.Balanced, true));
            Assert.Equal("hevc_nvenc", Value(a, "-c:v"));
            Assert.Equal("main10", Value(a, "-profile:v"));
            Assert.Equal("bt2020", Value(a, "-color_primaries"));
            Assert.Equal("smpte2084", Value(a, "-color_trc"));
            Assert.Equal("bt2020nc", Value(a, "-colorspace"));
            Assert.Equal("tv", Value(a, "-color_range"));
        }

        [Fact]
        public void Nvenc_sdr_uses_bt709_tags()
        {
            List<string> a = EncoderProfiles.BuildVideoArgs(Hevc(QualityPreset.Balanced, false));
            Assert.Equal("main", Value(a, "-profile:v"));
            Assert.Equal("bt709", Value(a, "-color_primaries"));
            Assert.Equal("bt709", Value(a, "-color_trc"));
            Assert.Equal("bt709", Value(a, "-colorspace"));
        }

        [Fact]
        public void Gop_is_fps_times_seconds()
        {
            EncoderSettings s = Hevc(QualityPreset.Balanced, true);
            s.Fps = 60; s.GopSeconds = 2;
            Assert.Equal("120", Value(EncoderProfiles.BuildVideoArgs(s), "-g"));
            s.Fps = 30; s.GopSeconds = 1.5;
            Assert.Equal("45", Value(EncoderProfiles.BuildVideoArgs(s), "-g"));
        }

        [Fact]
        public void Cbr_sets_maxrate_and_a_buffer_of_buffer_seconds()
        {
            EncoderSettings s = Hevc(QualityPreset.Balanced, true);
            s.BitrateKbps = 40000;
            s.BufferSeconds = 2;
            List<string> a = EncoderProfiles.BuildVideoArgs(s);
            Assert.Equal("cbr", Value(a, "-rc"));
            Assert.Equal("40000k", Value(a, "-b:v"));
            Assert.Equal("40000k", Value(a, "-maxrate"));
            Assert.Equal("80000k", Value(a, "-bufsize"));
        }

        [Fact]
        public void Quality_preset_spends_more_on_picture_than_speed()
        {
            List<string> speed = EncoderProfiles.BuildVideoArgs(Hevc(QualityPreset.Speed, true));
            List<string> quality = EncoderProfiles.BuildVideoArgs(Hevc(QualityPreset.Quality, true));
            Assert.Equal("p3", Value(speed, "-preset"));
            Assert.Equal("p7", Value(quality, "-preset"));
            Assert.Equal("disabled", Value(speed, "-multipass"));
            Assert.Equal("fullres", Value(quality, "-multipass"));
            Assert.Equal("0", Value(speed, "-bf"));
            Assert.Equal("3", Value(quality, "-bf"));
            Assert.Equal("0", Value(speed, "-rc-lookahead"));
            Assert.Equal("32", Value(quality, "-rc-lookahead"));
        }

        [Fact]
        public void This_is_not_a_low_latency_tune()
        {
            // A buffered livestream keeps -tune hq; ll/ull would trade picture quality for latency we do not need.
            List<string> a = EncoderProfiles.BuildVideoArgs(Hevc(QualityPreset.Quality, true));
            Assert.Equal("hq", Value(a, "-tune"));
            Assert.DoesNotContain("-zerolatency", a);
            Assert.DoesNotContain("-delay", a);
        }

        [Fact]
        public void B_frames_can_be_turned_off()
        {
            EncoderSettings s = Hevc(QualityPreset.Quality, true);
            s.BFrames = false;
            List<string> a = EncoderProfiles.BuildVideoArgs(s);
            Assert.Equal("0", Value(a, "-bf"));
            Assert.DoesNotContain("-b_ref_mode", a);
        }

        [Fact]
        public void X265_hdr_carries_mastering_display_and_repeat_headers()
        {
            var s = new EncoderSettings
            {
                Encoder = VideoEncoder.LibX265, Hdr = true, BitrateKbps = 40000, Fps = 60,
                GopSeconds = 2, BufferSeconds = 2, MasteringMaxNits = 1000,
            };
            string p = Value(EncoderProfiles.BuildVideoArgs(s), "-x265-params");
            Assert.Contains("hdr-opt=1", p);
            Assert.Contains("repeat-headers=1", p);          // MPEG-TS needs VPS/SPS/PPS before every IDR
            Assert.Contains("master-display=", p);
            Assert.Contains("max-cll=1000,250", p);
            Assert.Contains("keyint=120", p);
            Assert.Contains("vbv-maxrate=40000", p);
        }

        [Fact]
        public void Only_hevc_encoders_are_offered()
        {
            // The P2P fan-out carries MPEG-TS, which has no AV1 mapping in ffmpeg (EncoderArgsIntegrationTests
            // proves it), so every encoder in the catalogue must be HEVC.
            foreach (EncoderInfo info in EncoderProfiles.All) Assert.StartsWith("HEVC", info.DisplayName);
            Assert.Equal(4, EncoderProfiles.All.Length);
        }

        [Fact]
        public void Suggested_bitrate_grows_with_pixels_and_frame_rate()
        {
            int p1080 = EncoderProfiles.SuggestedMbps(1920, 1080, 60);
            int p1440 = EncoderProfiles.SuggestedMbps(2560, 1440, 60);
            int p2160 = EncoderProfiles.SuggestedMbps(3840, 2160, 60);
            Assert.True(p1080 < p1440 && p1440 < p2160);
            Assert.InRange(p1440, 30, 50);
            Assert.True(EncoderProfiles.SuggestedMbps(2560, 1440, 30) < p1440);
            Assert.InRange(p2160, 10, 100);
        }

        [Fact]
        public void Every_encoder_produces_a_parseable_option_list()
        {
            foreach (EncoderInfo info in EncoderProfiles.All)
            {
                foreach (bool hdr in new[] { false, true })
                {
                    var s = new EncoderSettings { Encoder = info.Id, Hdr = hdr, BitrateKbps = 20000, Fps = 60 };
                    List<string> a = EncoderProfiles.BuildVideoArgs(s);
                    Assert.Equal("-c:v", a[0]);
                    Assert.Equal(info.FfmpegName, a[1]);
                    Assert.Equal(0, a.Count % 2);                       // every option has a value
                    for (int i = 0; i < a.Count; i += 2)
                        Assert.StartsWith("-", a[i]);
                }
            }
        }
    }

    public class FfmpegCommandTests
    {
        static FfmpegPlan Plan(bool hdr = true, bool audio = true) => new FfmpegPlan
        {
            VideoPipeName = "hdrcast-video-1234",
            AudioPipeName = audio ? "hdrcast-audio-1234" : null,
            PixelFormat = hdr ? CaptureFormat.P010 : CaptureFormat.Nv12,
            Width = 2560,
            Height = 1440,
            Fps = 60,
            Encoder = new EncoderSettings { Encoder = VideoEncoder.HevcNvenc, Hdr = hdr, BitrateKbps = 40000, Fps = 60 },
            Audio = audio ? AudioCodec.Aac : AudioCodec.None,
        };

        static string Value(List<string> args, string key)
        {
            int i = args.IndexOf(key);
            return i >= 0 && i + 1 < args.Count ? args[i + 1] : "";
        }

        /// <summary>Last occurrence: "-f" appears once per input and once for the output format.</summary>
        static string LastValue(List<string> args, string key)
        {
            int i = args.LastIndexOf(key);
            return i >= 0 && i + 1 < args.Count ? args[i + 1] : "";
        }

        [Fact]
        public void Builds_raw_video_input_from_the_named_pipe()
        {
            List<string> a = FfmpegCommandBuilder.Build(Plan());
            Assert.Equal("rawvideo", Value(a, "-f"));
            Assert.Equal("p010le", Value(a, "-pix_fmt"));
            Assert.Equal("2560x1440", Value(a, "-video_size"));
            Assert.Equal("60", Value(a, "-framerate"));
            Assert.Contains(@"\\.\pipe\hdrcast-video-1234", a);
        }

        [Fact]
        public void Audio_input_is_f32le_48k_stereo()
        {
            List<string> a = FfmpegCommandBuilder.Build(Plan());
            Assert.Contains("f32le", a);
            Assert.Contains("48000", a);
            Assert.Contains(@"\\.\pipe\hdrcast-audio-1234", a);
            Assert.Equal("aac", Value(a, "-c:a"));
            Assert.Equal("192k", Value(a, "-b:a"));
        }

        [Fact]
        public void Without_audio_there_is_only_one_input()
        {
            List<string> a = FfmpegCommandBuilder.Build(Plan(audio: false));
            int inputs = a.FindAll(x => x == "-i").Count;
            Assert.Equal(1, inputs);
            Assert.DoesNotContain("-c:a", a);
        }

        [Fact]
        public void Mpegts_output_resends_headers_and_has_no_mux_delay()
        {
            List<string> a = FfmpegCommandBuilder.Build(Plan());
            Assert.Equal("+resend_headers", Value(a, "-mpegts_flags"));
            Assert.Equal("0", Value(a, "-muxdelay"));
            Assert.Equal("0", Value(a, "-muxpreload"));
            Assert.Equal("mpegts", LastValue(a, "-f"));
            Assert.Equal("pipe:1", a[a.Count - 1]);                   // stdout, into the P2P fan-out
        }

        [Fact]
        public void Progress_is_reported_on_stderr_because_stdout_carries_the_stream()
        {
            List<string> a = FfmpegCommandBuilder.Build(Plan());
            Assert.Equal("pipe:2", Value(a, "-progress"));
        }

        [Fact]
        public void Hdr_encoding_requires_p010_input()
        {
            FfmpegPlan p = Plan(hdr: true);
            p.PixelFormat = CaptureFormat.Nv12;
            Assert.Throws<ArgumentException>(() => FfmpegCommandBuilder.Build(p));
        }

        [Fact]
        public void Odd_frame_size_is_refused()
        {
            FfmpegPlan p = Plan();
            p.Width = 1921;
            Assert.Throws<ArgumentException>(() => FfmpegCommandBuilder.Build(p));
        }

        [Fact]
        public void Probe_encodes_a_synthetic_second_to_null()
        {
            List<string> a = FfmpegCommandBuilder.BuildProbe(new EncoderSettings { Encoder = VideoEncoder.HevcNvenc, Hdr = true });
            Assert.Contains("lavfi", a);
            Assert.Contains(a, x => x.Contains("duration=1"));   // SVT-AV1 emits nothing for a shorter clip
            Assert.Equal("null", LastValue(a, "-f"));
            Assert.Equal("hevc_nvenc", Value(a, "-c:v"));
            Assert.Equal("p010le", Value(a, "-pix_fmt"));
        }

        [Fact]
        public void Cpu_encoders_get_a_planar_pixel_format()
        {
            // The capture engine only produces p010/nv12; libx265 accepts neither, so the profile must name the
            // planar format or ffmpeg may negotiate 8-bit and silently drop HDR precision.
            Assert.Equal("p010le", EncoderProfiles.PixelFormat(VideoEncoder.HevcNvenc, true));
            Assert.Equal("nv12", EncoderProfiles.PixelFormat(VideoEncoder.HevcNvenc, false));
            Assert.Equal("yuv420p10le", EncoderProfiles.PixelFormat(VideoEncoder.LibX265, true));
            Assert.Equal("yuv420p", EncoderProfiles.PixelFormat(VideoEncoder.LibX265, false));
        }

        [Fact]
        public void X265_writes_the_colour_description_into_x265_params()
        {
            // ffmpeg's libx265 wrapper drops -color_primaries / -color_trc; without these the viewer gets an
            // untagged stream and shows HDR as washed-out SDR.
            var s = new EncoderSettings { Encoder = VideoEncoder.LibX265, Hdr = true, BitrateKbps = 20000, Fps = 60 };
            string p = Value(EncoderProfiles.BuildVideoArgs(s), "-x265-params");
            Assert.Contains("colorprim=bt2020", p);
            Assert.Contains("transfer=smpte2084", p);
            Assert.Contains("colormatrix=bt2020nc", p);

            s.Hdr = false;
            p = Value(EncoderProfiles.BuildVideoArgs(s), "-x265-params");
            Assert.Contains("colorprim=bt709", p);
            Assert.Contains("transfer=bt709", p);
        }

        [Fact]
        public void Pipe_path_uses_the_windows_prefix()
        {
            Assert.Equal(@"\\.\pipe\foo", FfmpegCommandBuilder.PipePath("foo"));
        }
    }

    public class FfmpegProgressTests
    {
        [Fact]
        public void Parses_a_complete_block()
        {
            var parser = new FfmpegProgressParser();
            string[] lines =
            {
                "frame=1234", "fps=59.94", "stream_0_0_q=23.0", "bitrate=39876.5kbits/s",
                "total_size=12345678", "out_time_us=20566666", "dup_frames=3", "drop_frames=1",
                "speed=1.001x", "progress=continue",
            };
            FfmpegProgress? block = null;
            foreach (string l in lines) block = parser.Feed(l) ?? block;

            Assert.NotNull(block);
            Assert.Equal(1234, block!.Frame);
            Assert.Equal(59.94, block.Fps, 2);
            Assert.Equal(39876.5, block.BitrateKbps, 1);
            Assert.Equal(3, block.DupFrames);
            Assert.Equal(1, block.DropFrames);
            Assert.Equal(1.001, block.Speed, 3);
            Assert.False(block.Ended);
            Assert.Equal(20.566666, block.OutTime.TotalSeconds, 3);
        }

        [Fact]
        public void Handles_na_values()
        {
            var parser = new FfmpegProgressParser();
            parser.Feed("bitrate=N/A");
            parser.Feed("speed=N/A");
            FfmpegProgress? b = parser.Feed("progress=continue");
            Assert.NotNull(b);
            Assert.True(b!.BitrateKbps < 0);
            Assert.True(b.Speed < 0);
        }

        [Fact]
        public void End_block_is_flagged()
        {
            var parser = new FfmpegProgressParser();
            parser.Feed("frame=10");
            FfmpegProgress? b = parser.Feed("progress=end");
            Assert.True(b!.Ended);
        }

        [Fact]
        public void Ignores_unrelated_lines()
        {
            var parser = new FfmpegProgressParser();
            Assert.Null(parser.Feed("this is not progress output"));
            Assert.Null(parser.Feed(""));
            Assert.Null(parser.Feed(null));
        }
    }

    public class SenderSettingsTests
    {
        [Fact]
        public void Defaults_are_a_working_group_configuration()
        {
            var s = new SenderSettings();
            Assert.Equal(VideoEncoder.HevcNvenc, s.Encoder);
            Assert.True(s.Audio);
            Assert.Equal(4, s.MaxViewers);
            Assert.Empty(s.Validate());
        }

        [Fact]
        public void An_old_settings_file_with_relay_fields_and_an_av1_encoder_still_loads()
        {
            // "encoder":1 was av1_nvenc; "mode"/"host"/... belonged to the removed relay and direct modes.
            const string json = "{\"encoder\":1,\"bitrateMbps\":50,\"mode\":0,\"host\":\"192.168.1.201\",\"port\":8890," +
                                "\"path\":\"screen\",\"user\":\"sender\",\"password\":\"pw\",\"autoRestart\":true}";
            SenderSettings? s = Json.Deserialize<SenderSettings>(json);
            Assert.NotNull(s);
            s!.Normalize();
            Assert.Equal(VideoEncoder.HevcNvenc, s.Encoder);
            Assert.Equal(50, s.BitrateMbps);
            Assert.Empty(s.Validate());
        }

        [Fact]
        public void Scale_keeps_aspect_ratio_and_even_numbers()
        {
            var s = new SenderSettings { Scale = ScaleMode.H1080 };
            s.ResolveSize(3840, 2160, out int w, out int h);
            Assert.Equal(1920, w);
            Assert.Equal(1080, h);

            s.Scale = ScaleMode.H1440;
            s.ResolveSize(3440, 1440, out w, out h);
            Assert.Equal(3440, w);      // already 1440 tall: no upscale, no change
            Assert.Equal(1440, h);

            s.Scale = ScaleMode.H720;
            s.ResolveSize(2560, 1080, out w, out h);
            Assert.Equal(0, w % 2);
            Assert.Equal(720, h);
        }

        [Fact]
        public void Native_scale_only_rounds_to_even()
        {
            var s = new SenderSettings { Scale = ScaleMode.Native };
            s.ResolveSize(1366, 769, out int w, out int h);
            Assert.Equal(1366, w);
            Assert.Equal(768, h);
        }

        [Fact]
        public void Format_follows_the_monitor_unless_sdr_is_forced()
        {
            var s = new SenderSettings { HdrMode = HdrMode.Auto };
            Assert.Equal(CaptureFormat.P010, s.ResolveFormat(true));
            Assert.Equal(CaptureFormat.Nv12, s.ResolveFormat(false));
            s.HdrMode = HdrMode.ForceSdr;
            Assert.Equal(CaptureFormat.Nv12, s.ResolveFormat(true));
        }

        [Fact]
        public void Normalize_clamps_hand_edited_values()
        {
            var s = new SenderSettings { BitrateMbps = 9999, Fps = 0, GopSeconds = 99, MaxViewers = 99 };
            s.Normalize();
            Assert.InRange(s.BitrateMbps, 1, 500);
            Assert.Equal(60, s.Fps);
            Assert.InRange(s.GopSeconds, 0.25, 10);
            Assert.Equal(16, s.MaxViewers);
        }

        [Fact]
        public void Capture_args_round_trip_through_the_command_line()
        {
            var s = new SenderSettings { Fps = 60, Cursor = false, Audio = true, SdrWhite = 120 };
            var output = new OutputInfo { Index = 2, DeviceName = @"\\.\DISPLAY3" };
            CaptureArgs args = s.ToCaptureArgs(output, 2560, 1440, CaptureFormat.P010, "vid", "aud", 4321);
            CaptureArgs parsed = CaptureArgs.Parse(args.ToArgv());

            Assert.Equal(2, parsed.OutputIndex);
            Assert.Equal(60, parsed.Fps);
            Assert.Equal(CaptureFormat.P010, parsed.Format);
            Assert.Equal(2560, parsed.Width);
            Assert.Equal(1440, parsed.Height);
            Assert.False(parsed.Cursor);
            Assert.Equal("vid", parsed.VideoPipe);
            Assert.Equal("aud", parsed.AudioPipe);
            Assert.Equal(4321, parsed.ParentPid);
            Assert.Equal(120f, parsed.SdrWhite);
        }
    }
}
