using System;
using System.Collections.Generic;
using System.Diagnostics;
using HdrCast.Capture;
using Xunit;

namespace HdrCast.Tests
{
    public class ColorMathTests
    {
        // Textbook ST.2084 values for scRGB grey at sdr-white = 80 nit, limited-range 10-bit luma
        // (Y' = 64 + 876 * E): 1.0 = 80 nit -> E 0.4859 -> 490; 2.5 = 200 nit -> 571; 125 = 10000 nit -> 940 (clipped).
        // ffmpeg's zscale (npl=80) produced 491 / 573 / 943 for the same inputs on this project's Linux box:
        // zimg rounds slightly differently and lets the limited-range maximum overshoot past 940, which is why
        // the tolerance below is 1 code value and not 0 against zscale.
        [Theory]
        [InlineData(1.0, 490)]
        [InlineData(2.5, 571)]
        [InlineData(125.0, 940)]
        public void Grey_scrgb_matches_st2084_reference(double value, int expectedY10)
        {
            Assert.InRange(ColorMath.ScRgbGreyToY10(value), expectedY10 - 1, expectedY10 + 1);
        }

        [Fact]
        public void Limited_range_luma_never_exceeds_940()
        {
            // The encoder gets limited-range P010; clipping here (rather than letting PQ overshoot) keeps
            // every sample inside 64..940 whatever the desktop throws at us.
            Assert.Equal(940, ColorMath.ScRgbGreyToY10(1000.0));
            Assert.Equal(64, ColorMath.ScRgbGreyToY10(0.0));
        }

        [Fact]
        public void Black_is_limited_range_64_and_white_point_is_neutral()
        {
            ColorMath.ScRgbToYuv10(0, 0, 0, 80, out int y, out int cb, out int cr);
            Assert.Equal(64, y);
            Assert.Equal(512, cb);
            Assert.Equal(512, cr);
        }

        [Fact]
        public void Bt2020_matrix_rows_sum_to_one()
        {
            // A grey input must stay grey: each row of the BT.709 -> BT.2020 matrix sums to 1.
            for (int row = 0; row < 3; row++)
            {
                double sum = ColorMath.M709To2020[row, 0] + ColorMath.M709To2020[row, 1] + ColorMath.M709To2020[row, 2];
                Assert.Equal(1.0, sum, 5);
            }
        }

        [Fact]
        public void Pq_curve_round_trips()
        {
            foreach (double nits in new[] { 0.1, 1.0, 80.0, 203.0, 1000.0, 4000.0, 10000.0 })
            {
                double e = ColorMath.PqOetf(nits / 10000.0);
                Assert.Equal(nits, ColorMath.PqEotfNits(e), 0);
            }
        }

        [Fact]
        public void Sdr_white_scales_the_result()
        {
            // With a higher "scRGB 1.0 = N nit" the same input encodes brighter.
            Assert.True(ColorMath.ScRgbGreyToY10(1.0, 203.0) > ColorMath.ScRgbGreyToY10(1.0, 80.0));
        }

        [Fact]
        public void Out_of_gamut_negatives_are_clipped_not_wrapped()
        {
            ColorMath.ScRgbToYuv10(-0.5, 0.5, 1.5, 80, out int y, out int cb, out int cr);
            Assert.InRange(y, 64, 940);
            Assert.InRange(cb, 64, 960);
            Assert.InRange(cr, 64, 960);
        }

        [Fact]
        public void Srgb_conversion_hits_bt709_limited_range()
        {
            ColorMath.SrgbToYuv8(0, 0, 0, out int y0, out int cb0, out int cr0);
            Assert.Equal(16, y0);
            Assert.Equal(128, cb0);
            Assert.Equal(128, cr0);
            ColorMath.SrgbToYuv8(1, 1, 1, out int y1, out _, out _);
            Assert.Equal(235, y1);
        }

        [Fact]
        public void P010_samples_are_stored_in_the_high_bits()
        {
            Assert.Equal(491 << 6, ColorMath.ToP010Sample(491));
            Assert.Equal(491, ColorMath.FromP010Sample(ColorMath.ToP010Sample(491)));
        }

        [Fact]
        public void Frame_sizes_match_the_pixel_formats()
        {
            Assert.Equal(2560L * 1440 * 3, ColorMath.FrameBytes(CaptureFormat.P010, 2560, 1440));   // 2 bytes * 1.5 planes
            Assert.Equal(2560L * 1440 * 3 / 2, ColorMath.FrameBytes(CaptureFormat.Nv12, 2560, 1440));
            Assert.Equal(2560L * 1440 * 2, ColorMath.UvPlaneOffset(CaptureFormat.P010, 2560, 1440));
            Assert.Equal(2560L * 1440, ColorMath.UvPlaneOffset(CaptureFormat.Nv12, 2560, 1440));
        }

        [Fact]
        public void Black_fill_is_valid_for_both_formats()
        {
            foreach (CaptureFormat f in new[] { CaptureFormat.P010, CaptureFormat.Nv12 })
            {
                int w = 64, h = 32;
                var frame = new byte[ColorMath.FrameBytes(f, w, h)];
                ColorMath.FillBlack(frame, f, w, h);
                long uv = ColorMath.UvPlaneOffset(f, w, h);
                if (f == CaptureFormat.P010)
                {
                    Assert.Equal(64, ColorMath.FromP010Sample((ushort)(frame[0] | (frame[1] << 8))));
                    Assert.Equal(512, ColorMath.FromP010Sample((ushort)(frame[uv] | (frame[uv + 1] << 8))));
                }
                else
                {
                    Assert.Equal(16, frame[0]);
                    Assert.Equal(128, frame[uv]);
                }
            }
        }
    }

    public class FramePacerTests
    {
        const long Hz = 10_000_000;   // 100 ns ticks

        [Fact]
        public void Emits_exactly_fps_frames_per_second_without_drift()
        {
            var pacer = new FramePacer(60, Hz);
            pacer.Start(0);
            long emitted = 0;
            for (long second = 1; second <= 100; second++)
            {
                long now = second * Hz;
                long due = pacer.FramesDue(now);
                pacer.Consume(due);
                emitted += due;
            }
            // Frames 0..6000 have a deadline at or before t = 100 s, so 6001 is exactly right (no drift).
            Assert.Equal(6001, emitted);
        }

        [Fact]
        public void Nothing_is_due_before_the_first_deadline_passes()
        {
            var pacer = new FramePacer(60, Hz);
            pacer.Start(0);
            Assert.Equal(1, pacer.FramesDue(0));       // frame 0 is due immediately
            pacer.Consume(1);
            Assert.Equal(0, pacer.FramesDue(Hz / 120));
            Assert.Equal(1, pacer.FramesDue(Hz / 60));
        }

        [Fact]
        public void A_stall_makes_several_frames_due()
        {
            var pacer = new FramePacer(60, Hz);
            pacer.Start(0);
            pacer.Consume(pacer.FramesDue(0));
            Assert.Equal(6, pacer.FramesDue(Hz / 10));   // 100 ms stall at 60 fps
        }

        [Fact]
        public void Skip_missed_jumps_to_the_present()
        {
            var pacer = new FramePacer(60, Hz);
            pacer.Start(0);
            long skipped = pacer.SkipMissed(5 * Hz);     // 5 s gap (sleep / resume)
            Assert.True(skipped >= 299);
            Assert.True(pacer.FramesDue(5 * Hz) <= 1);
        }

        [Fact]
        public void Ticks_until_next_is_never_negative()
        {
            var pacer = new FramePacer(30, Hz);
            pacer.Start(0);
            Assert.Equal(0, pacer.TicksUntilNext(Hz));
            pacer.Consume(pacer.FramesDue(Hz));
            Assert.True(pacer.TicksUntilNext(Hz) > 0);
        }
    }

    public class AudioClockAlignerTests
    {
        [Fact]
        public void Perfectly_timed_audio_is_not_touched()
        {
            var a = new AudioClockAligner(48000, 20);
            AudioAlignment r = a.Align(480, 480);
            Assert.True(r.IsIdentity);
            Assert.Equal(480, a.WrittenFrames);
        }

        [Fact]
        public void Falling_behind_inserts_silence()
        {
            var a = new AudioClockAligner(48000, 20);
            // The clock says 48000 frames should exist, but only 480 arrived: pad the rest.
            AudioAlignment r = a.Align(48000, 480);
            Assert.Equal(0, r.DropFrames);
            Assert.True(r.PadFrames > 0);
            Assert.Equal(48000, a.WrittenFrames);
            Assert.Equal(1, a.Underruns);
        }

        [Fact]
        public void Running_ahead_drops_frames()
        {
            var a = new AudioClockAligner(48000, 20);
            a.Align(48000, 48000);
            AudioAlignment r = a.Align(48100, 4800);     // way more audio than time has passed
            Assert.True(r.DropFrames > 0);
            Assert.Equal(0, r.PadFrames);
            Assert.True(a.Overruns > 0);
        }

        [Fact]
        public void Small_deviations_stay_within_the_tolerance()
        {
            var a = new AudioClockAligner(48000, 20);       // 20 ms = 960 frames
            Assert.True(a.Align(1000, 500).IsIdentity);     // 500 frames behind: inside the tolerance
        }

        [Fact]
        public void Expected_frames_follows_the_clock()
        {
            var a = new AudioClockAligner(48000);
            Assert.Equal(48000, a.ExpectedFrames(Stopwatch.Frequency, Stopwatch.Frequency));
            Assert.Equal(24000, a.ExpectedFrames(Stopwatch.Frequency / 2, Stopwatch.Frequency));
        }

        [Fact]
        public void Pad_if_behind_covers_a_silent_endpoint()
        {
            var a = new AudioClockAligner(48000, 20);
            Assert.Equal(0, a.PadIfBehind(100));
            int pad = a.PadIfBehind(48000);
            Assert.True(pad > 0);
            Assert.Equal(48000, a.WrittenFrames);
        }
    }

    public class CaptureArgsTests
    {
        [Fact]
        public void Output_index_is_required()
        {
            Assert.Throws<CaptureArgsException>(() => CaptureArgs.Parse(new[] { "--fps", "60" }));
        }

        [Fact]
        public void Selftest_does_not_need_an_output()
        {
            CaptureArgs a = CaptureArgs.Parse(new[] { "--selftest" });
            Assert.True(a.SelfTest);
        }

        [Fact]
        public void Parses_a_full_command_line()
        {
            CaptureArgs a = CaptureArgs.Parse(new[]
            {
                "--output", "1", "--fps", "60", "--format", "p010", "--size", "2560x1440",
                "--sdr-white", "80", "--cursor", "off", "--video-pipe", "v", "--audio", "on",
                "--audio-pipe", "s", "--audio-device", "{0.0.0.x}", "--parent-pid", "42",
            });
            Assert.Equal(1, a.OutputIndex);
            Assert.Equal(CaptureFormat.P010, a.Format);
            Assert.Equal(2560, a.Width);
            Assert.False(a.Cursor);
            Assert.Equal("{0.0.0.x}", a.AudioDevice);
            Assert.Equal(42, a.ParentPid);
        }

        [Theory]
        [InlineData("2561x1440")]     // odd width
        [InlineData("2560x1441")]     // odd height
        [InlineData("2560")]          // not WxH
        [InlineData("0x0")]
        public void Rejects_bad_sizes(string size)
        {
            Assert.Throws<CaptureArgsException>(() => CaptureArgs.Parse(new[] { "--output", "0", "--size", size }));
        }

        [Fact]
        public void Rejects_a_pipe_path_instead_of_a_name()
        {
            Assert.Throws<CaptureArgsException>(() =>
                CaptureArgs.Parse(new[] { "--output", "0", "--video-pipe", @"\\.\pipe\x" }));
        }

        [Fact]
        public void Unknown_option_is_an_error()
        {
            Assert.Throws<CaptureArgsException>(() => CaptureArgs.Parse(new[] { "--output", "0", "--turbo" }));
        }

        [Fact]
        public void Dump_seconds_needs_a_dump_path()
        {
            Assert.Throws<CaptureArgsException>(() => CaptureArgs.Parse(new[] { "--output", "0", "--seconds", "5" }));
        }

        [Fact]
        public void Argv_round_trip_is_stable()
        {
            var original = new CaptureArgs
            {
                OutputIndex = 3, Fps = 120, Format = CaptureFormat.Nv12, Width = 1920, Height = 1080,
                SdrWhite = 100, Cursor = true, VideoPipe = "vp", AudioPipe = "ap", Audio = true,
                AudioDevice = "dev-1", ParentPid = 7, DumpPath = null,
            };
            CaptureArgs again = CaptureArgs.Parse(original.ToArgv());
            Assert.Equal(original.OutputIndex, again.OutputIndex);
            Assert.Equal(original.Fps, again.Fps);
            Assert.Equal(original.Format, again.Format);
            Assert.Equal(original.Width, again.Width);
            Assert.Equal(original.Height, again.Height);
            Assert.Equal(original.SdrWhite, again.SdrWhite);
            Assert.Equal(original.VideoPipe, again.VideoPipe);
            Assert.Equal(original.AudioDevice, again.AudioDevice);
            Assert.Equal(original.ParentPid, again.ParentPid);
        }
    }

    public class CaptureStatsTests
    {
        [Fact]
        public void Json_line_round_trips()
        {
            var s = new CaptureStats
            {
                T = 12.0, Captured = 700, Duplicated = 20, Dropped = 1,
                Fps = 59.9, GpuMs = 0.83, AudioUnderrun = 2, PipeQueueMs = 16.7,
            };
            CaptureStats? back = CaptureStats.TryParse(s.ToJsonLine());
            Assert.NotNull(back);
            Assert.Equal(700, back!.Captured);
            Assert.Equal(20, back.Duplicated);
            Assert.Equal(1, back.Dropped);
            Assert.Equal(59.9, back.Fps, 1);
            Assert.Equal(0.83, back.GpuMs, 2);
            Assert.Equal(2, back.AudioUnderrun);
        }

        [Fact]
        public void Non_stats_lines_are_rejected()
        {
            Assert.Null(CaptureStats.TryParse("capture: starting"));
            Assert.Null(CaptureStats.TryParse(""));
            Assert.Null(CaptureStats.TryParse(null));
            Assert.Null(CaptureStats.TryParse("{not json}"));
        }
    }

    public class OutputInfoTests
    {
        static OutputInfo Sample() => new OutputInfo
        {
            Index = 1, AdapterIndex = 0, OutputIndexOnAdapter = 1, Adapter = "NVIDIA GeForce RTX 4090",
            DeviceName = @"\\.\DISPLAY2", FriendlyName = "AW3225QF", Width = 3840, Height = 2160,
            RefreshRate = 239.76, Hdr = true, ColorSpace = OutputInfo.ColorSpaceHdr10,
            BitsPerColor = 10, MaxLuminance = 1000, AttachedToDesktop = true, Rotation = 1,
        };

        [Fact]
        public void Json_round_trips()
        {
            List<OutputInfo> back = OutputInfo.FromJson(OutputInfo.ToJson(new List<OutputInfo> { Sample() }));
            Assert.Single(back);
            Assert.Equal(@"\\.\DISPLAY2", back[0].DeviceName);
            Assert.True(back[0].Hdr);
            Assert.Equal(1000, back[0].MaxLuminance);
        }

        [Fact]
        public void Label_shows_index_name_size_and_hdr()
        {
            string label = Sample().Label;
            Assert.StartsWith("1: AW3225QF", label);
            Assert.Contains("3840x2160", label);
            Assert.Contains("HDR", label);
        }

        [Fact]
        public void Rotation_helpers()
        {
            var o = Sample();
            Assert.False(o.IsRotated90);
            o.Rotation = 2;
            Assert.True(o.IsRotated90);
            Assert.Equal("90", o.RotationName);
        }

        [Fact]
        public void Table_has_a_header_and_one_row_per_output()
        {
            string table = OutputInfo.ToTable(new List<OutputInfo> { Sample(), Sample() });
            string[] lines = table.TrimEnd().Split('\n');
            Assert.Equal(3, lines.Length);
            Assert.Contains("idx", lines[0]);
        }
    }
}
