//! The platform-neutral model: colour maths, pacer, audio clock aligner, command line, statistics, output
//! descriptions. Ported from tests/CaptureTests.cs (plus the capture-only cases of SenderModelTests.cs).

use hdrcast_capture::aligner::{AudioAlignment, AudioClockAligner};
use hdrcast_capture::app_audio::{self, ProcessEntry};
use hdrcast_capture::clock::TICKS_PER_SECOND;
use hdrcast_capture::color;
use hdrcast_capture::pacer::FramePacer;
use hdrcast_capture::{CaptureArgs, CaptureFormat, CaptureStats, OutputInfo};

mod color_math {
    use super::*;

    // Textbook ST.2084 values for scRGB grey at sdr-white = 80 nit, limited-range 10-bit luma
    // (Y' = 64 + 876 * E): 1.0 = 80 nit -> E 0.4859 -> 490; 2.5 = 200 nit -> 571; 125 = 10000 nit -> 940 (clipped).
    // ffmpeg's zscale (npl=80) produced 491 / 573 / 943 for the same inputs on this project's Linux box:
    // zimg rounds slightly differently and lets the limited-range maximum overshoot past 940, which is why
    // the tolerance below is 1 code value and not 0 against zscale.
    #[test]
    fn grey_scrgb_matches_st2084_reference() {
        for (value, expected_y10) in [(1.0, 490), (2.5, 571), (125.0, 940)] {
            let y10 = color::scrgb_grey_to_y10(value, 80.0);
            assert!(
                (expected_y10 - 1..=expected_y10 + 1).contains(&y10),
                "scRGB {value}: {y10}"
            );
        }
    }

    #[test]
    fn limited_range_luma_never_exceeds_940() {
        // The encoder gets limited-range P010; clipping here (rather than letting PQ overshoot) keeps
        // every sample inside 64..940 whatever the desktop throws at us.
        assert_eq!(color::scrgb_grey_to_y10(1000.0, 80.0), 940);
        assert_eq!(color::scrgb_grey_to_y10(0.0, 80.0), 64);
    }

    #[test]
    fn black_is_limited_range_64_and_white_point_is_neutral() {
        assert_eq!(color::scrgb_to_yuv10(0.0, 0.0, 0.0, 80.0), (64, 512, 512));
        // Grey stays neutral at every level (the matrix rows sum to 1).
        for v in [0.1, 1.0, 2.5, 50.0] {
            let (_, cb, cr) = color::scrgb_to_yuv10(v, v, v, 80.0);
            assert_eq!((cb, cr), (512, 512), "grey {v}");
        }
    }

    #[test]
    fn bt2020_matrix_rows_sum_to_one() {
        // A grey input must stay grey: each row of the BT.709 -> BT.2020 matrix sums to 1.
        for row in color::M709_TO_2020 {
            let sum: f64 = row.iter().sum();
            assert!((sum - 1.0).abs() < 5e-6, "{sum}");
        }
    }

    #[test]
    fn pq_curve_round_trips() {
        for nits in [0.1, 1.0, 80.0, 203.0, 1000.0, 4000.0, 10000.0] {
            let e = color::pq_oetf(nits / 10000.0);
            assert!((color::pq_eotf_nits(e) - nits).abs() < 0.5, "{nits} nit");
        }
    }

    #[test]
    fn sdr_white_scales_the_result() {
        // With a higher "scRGB 1.0 = N nit" the same input encodes brighter.
        assert!(color::scrgb_grey_to_y10(1.0, 203.0) > color::scrgb_grey_to_y10(1.0, 80.0));
    }

    #[test]
    fn out_of_gamut_negatives_are_clipped_not_wrapped() {
        let (y, cb, cr) = color::scrgb_to_yuv10(-0.5, 0.5, 1.5, 80.0);
        assert!((64..=940).contains(&y));
        assert!((64..=960).contains(&cb));
        assert!((64..=960).contains(&cr));
    }

    #[test]
    fn srgb_conversion_hits_bt709_limited_range() {
        assert_eq!(color::srgb_to_yuv8(0.0, 0.0, 0.0), (16, 128, 128));
        assert_eq!(color::srgb_to_yuv8(1.0, 1.0, 1.0).0, 235);
    }

    #[test]
    fn srgb_transfer_round_trips() {
        for c in [0.0, 0.002, 0.04045, 0.2, 0.5, 1.0] {
            // (the two branches of the standard curve meet with a step of ~1e-8 at the knee)
            assert!(
                (color::linear_to_srgb(color::srgb_to_linear(c)) - c).abs() < 1e-6,
                "{c}"
            );
        }
    }

    #[test]
    fn p010_samples_are_stored_in_the_high_bits() {
        assert_eq!(color::to_p010_sample(491), 491 << 6);
        assert_eq!(color::from_p010_sample(color::to_p010_sample(491)), 491);
    }

    #[test]
    fn frame_sizes_match_the_pixel_formats() {
        assert_eq!(
            color::frame_bytes(CaptureFormat::P010, 2560, 1440),
            2560 * 1440 * 3
        ); // 2 bytes * 1.5 planes
        assert_eq!(
            color::frame_bytes(CaptureFormat::Nv12, 2560, 1440),
            2560 * 1440 * 3 / 2
        );
        assert_eq!(
            color::uv_plane_offset(CaptureFormat::P010, 2560, 1440),
            2560 * 1440 * 2
        );
        assert_eq!(
            color::uv_plane_offset(CaptureFormat::Nv12, 2560, 1440),
            2560 * 1440
        );
    }

    #[test]
    fn black_fill_is_valid_for_both_formats() {
        for format in [CaptureFormat::P010, CaptureFormat::Nv12] {
            let (w, h) = (64, 32);
            let mut frame = vec![0xAAu8; color::frame_bytes(format, w, h)];
            color::fill_black(&mut frame, format, w, h);
            let uv = color::uv_plane_offset(format, w, h);
            let last = frame.len() - 2;
            if format == CaptureFormat::P010 {
                let sample = |i: usize| {
                    color::from_p010_sample(u16::from_le_bytes([frame[i], frame[i + 1]]))
                };
                assert_eq!(sample(0), 64);
                assert_eq!(sample(uv - 2), 64);
                assert_eq!(sample(uv), 512);
                assert_eq!(sample(last), 512);
            } else {
                assert_eq!(frame[0], 16);
                assert_eq!(frame[uv - 1], 16);
                assert_eq!(frame[uv], 128);
                assert_eq!(frame[last + 1], 128);
            }
        }
    }
}

mod frame_pacer {
    use super::*;

    const HZ: i64 = 10_000_000; // 100 ns ticks

    #[test]
    fn emits_exactly_fps_frames_per_second_without_drift() {
        let mut pacer = FramePacer::new(60, HZ);
        pacer.start(0);
        let mut emitted = 0;
        for second in 1..=100 {
            let due = pacer.frames_due(second * HZ);
            pacer.consume(due);
            emitted += due;
        }
        // Frames 0..6000 have a deadline at or before t = 100 s, so 6001 is exactly right (no drift).
        assert_eq!(emitted, 6001);
    }

    #[test]
    fn nothing_is_due_before_the_first_deadline_passes() {
        let mut pacer = FramePacer::new(60, HZ);
        pacer.start(0);
        assert_eq!(pacer.frames_due(0), 1); // frame 0 is due immediately
        pacer.consume(1);
        assert_eq!(pacer.frames_due(HZ / 120), 0);
        assert_eq!(pacer.frames_due(HZ / 60), 1);
    }

    #[test]
    fn a_stall_makes_several_frames_due() {
        let mut pacer = FramePacer::new(60, HZ);
        pacer.start(0);
        pacer.consume(pacer.frames_due(0));
        assert_eq!(pacer.frames_due(HZ / 10), 6); // 100 ms stall at 60 fps
    }

    #[test]
    fn skip_missed_jumps_to_the_present() {
        let mut pacer = FramePacer::new(60, HZ);
        pacer.start(0);
        let skipped = pacer.skip_missed(5 * HZ); // 5 s gap (sleep / resume)
        assert!(skipped >= 299);
        assert!(pacer.frames_due(5 * HZ) <= 1);
    }

    #[test]
    fn ticks_until_next_is_never_negative() {
        let mut pacer = FramePacer::new(30, HZ);
        pacer.start(0);
        assert_eq!(pacer.ticks_until_next(HZ), 0);
        pacer.consume(pacer.frames_due(HZ));
        assert!(pacer.ticks_until_next(HZ) > 0);
    }

    /// The engine's own time base: nanosecond ticks that do not start at zero. Deadlines are absolute, there
    /// is no drift over a day at an awkward frame rate, and the arithmetic does not overflow.
    #[test]
    fn works_on_absolute_nanosecond_ticks_far_from_zero() {
        let start = 123_456_789_012_345;
        let mut pacer = FramePacer::new(60, TICKS_PER_SECOND);
        pacer.start(start);
        assert_eq!(pacer.deadline_ticks(0), start);
        assert_eq!(pacer.deadline_ticks(60), start + TICKS_PER_SECOND);
        assert_eq!(pacer.deadline_ticks(1), start + 16_666_666);
        assert_eq!(pacer.frame_interval_ticks(), 16_666_667);
        assert_eq!(pacer.frames_due(start - 1), 0);
        assert_eq!(pacer.frames_due(start), 1);
        pacer.consume(1);
        assert_eq!(pacer.ticks_until_next(start), 16_666_666);
        assert_eq!(pacer.frames_due(start + 16_666_665), 0);
        assert_eq!(pacer.frames_due(start + 16_666_666), 1);

        let day = 86_400 * TICKS_PER_SECOND;
        let mut ntsc = FramePacer::new(59, TICKS_PER_SECOND);
        ntsc.start(start);
        assert_eq!(ntsc.frames_due(start + day), 59 * 86_400 + 1);
        ntsc.consume(ntsc.frames_due(start + day));
        assert_eq!(ntsc.next_frame_index(), 59 * 86_400 + 1);
        assert!(ntsc.next_deadline_ticks() > start + day);
        assert!(ntsc.next_deadline_ticks() - (start + day) <= ntsc.frame_interval_ticks());
    }
}

mod audio_clock_aligner {
    use super::*;

    #[test]
    fn perfectly_timed_audio_is_not_touched() {
        let mut a = AudioClockAligner::new(48000, 20);
        assert!(a.align(480, 480).is_identity());
        assert_eq!(a.written_frames(), 480);
    }

    #[test]
    fn falling_behind_inserts_silence() {
        let mut a = AudioClockAligner::new(48000, 20);
        // The clock says 48000 frames should exist, but only 480 arrived: pad the rest.
        let r = a.align(48000, 480);
        assert_eq!(r.drop_frames, 0);
        assert!(r.pad_frames > 0);
        assert_eq!(a.written_frames(), 48000);
        assert_eq!(a.underruns(), 1);
    }

    #[test]
    fn running_ahead_drops_frames() {
        let mut a = AudioClockAligner::new(48000, 20);
        a.align(48000, 48000);
        let r = a.align(48100, 4800); // way more audio than time has passed
        assert!(r.drop_frames > 0);
        assert_eq!(r.pad_frames, 0);
        assert!(a.overruns() > 0);
    }

    #[test]
    fn small_deviations_stay_within_the_tolerance() {
        let mut a = AudioClockAligner::new(48000, 20); // 20 ms = 960 frames
        assert_eq!(a.tolerance_frames(), 960);
        assert!(a.align(1000, 500).is_identity()); // 500 frames behind: inside the tolerance
    }

    #[test]
    fn the_default_tolerance_is_150_ms() {
        let a = AudioClockAligner::default();
        assert_eq!(a.sample_rate(), 48000);
        assert_eq!(a.tolerance_frames(), 7200);
        assert_eq!(AudioClockAligner::DEFAULT_TOLERANCE_MS, 150);
    }

    #[test]
    fn expected_frames_follows_the_clock() {
        let a = AudioClockAligner::default();
        assert_eq!(a.expected_frames(TICKS_PER_SECOND, TICKS_PER_SECOND), 48000);
        assert_eq!(
            a.expected_frames(TICKS_PER_SECOND / 2, TICKS_PER_SECOND),
            24000
        );
        assert_eq!(a.expected_frames(-5, TICKS_PER_SECOND), 0);
        // A month of nanoseconds times the sample rate does not fit an i64; the result still does.
        let month = 30 * 86_400 * TICKS_PER_SECOND;
        assert_eq!(
            a.expected_frames(month, TICKS_PER_SECOND),
            30 * 86_400 * 48000
        );
    }

    #[test]
    fn pad_if_behind_covers_a_silent_endpoint() {
        let mut a = AudioClockAligner::new(48000, 20);
        assert_eq!(a.pad_if_behind(100), 0);
        assert!(a.pad_if_behind(48000) > 0);
        assert_eq!(a.written_frames(), 48000);
    }

    /// Small deterministic generator for the jitter (any sequence within the bounds has to pass).
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, bound: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % bound
        }
    }

    /// 20 s of loopback capture delivered every `period_ms` (+ jitter) and `late_ms` late, with the pipe
    /// writer's idle check every 20 ms in between. Returns the silence padded and the audio dropped, in frames.
    fn replay_loopback(
        mut a: AudioClockAligner,
        period_ms: i64,
        jitter_ms: u64,
        late_ms: i64,
    ) -> (u64, u64) {
        let mut rng = Lcg(1);
        let (mut captured, mut next_capture, mut padded, mut dropped) =
            (0i64, period_ms, 0u64, 0u64);
        for ms in 1..=20_000i64 {
            let expected = a.expected_frames(ms, 1000);
            if ms >= next_capture {
                let available = (ms - late_ms).max(0) * 48; // the endpoint renders continuously
                let r: AudioAlignment = a.align(expected, (available - captured) as usize);
                captured = available;
                padded += r.pad_frames as u64;
                dropped += r.drop_frames as u64;
                next_capture = ms + period_ms + rng.next(jitter_ms + 1) as i64;
            } else if ms % 20 == 0 {
                padded += a.pad_if_behind(expected) as u64; // the writer found nothing to send
            }
        }
        (padded, dropped)
    }

    #[test]
    fn bursty_loopback_audio_passes_through_untouched() {
        // v0.2.1 on Windows: WASAPI loopback hands audio over in bursts while the pipe writer checks every 20 ms
        // whether the stream fell behind the video clock. With a 20 ms tolerance the writer padded ~40 ms of
        // silence between two bursts and the next burst lost ~40 ms to the "ahead" check: a third of the audio
        // survived, in slivers 16 times a second, which viewers heard as a low buzz.
        let cases = [
            (62, 3, 0), // measured: NAudio's loopback capture called back every ~62 ms with ~62 ms of audio
            (62, 3, 25), // the same, delivered late
            (100, 30, 0), // a slower, busier machine
            (10, 5, 0), // this engine: the loopback buffer is polled every 10 ms
        ];
        for (period_ms, jitter_ms, late_ms) in cases {
            let result =
                replay_loopback(AudioClockAligner::default(), period_ms, jitter_ms, late_ms);
            assert_eq!(
                result,
                (0, 0),
                "period {period_ms} ms, jitter {jitter_ms} ms, {late_ms} ms late"
            );
        }
    }

    #[test]
    fn the_old_tolerance_chopped_bursty_audio() {
        let (padded, dropped) = replay_loopback(AudioClockAligner::new(48000, 20), 62, 3, 0);
        assert!(
            padded > 48000 * 5 && dropped > 48000 * 5,
            "padded {padded}, dropped {dropped}"
        );
    }
}

mod capture_args {
    use super::*;

    fn parse(args: &[&str]) -> Result<CaptureArgs, hdrcast_capture::CaptureArgsError> {
        CaptureArgs::parse(args)
    }

    #[test]
    fn output_index_is_required() {
        let error = parse(&["--fps", "60"]).unwrap_err();
        assert_eq!(error.to_string(), "--output <idx> is required");
    }

    #[test]
    fn selftest_does_not_need_an_output() {
        let a = parse(&["--selftest"]).unwrap();
        assert!(a.self_test);
        // ... and such a command line survives the round trip.
        assert!(CaptureArgs::parse(&a.to_argv()).unwrap().self_test);
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let a = parse(&["--output", "0"]).unwrap();
        assert_eq!(a.fps, 60);
        assert_eq!(a.format, CaptureFormat::Auto);
        assert!(!a.has_explicit_size());
        assert_eq!(a.sdr_white, 80.0);
        assert!(a.cursor);
        assert_eq!(a.video_pipe, "hdrcast-video");
        assert_eq!(a.audio_pipe, "hdrcast-audio");
        assert!(a.audio);
        assert_eq!(a.audio_device, "default");
        assert_eq!(a.audio_exclude, "");
        assert_eq!(a.pipe_timeout_seconds, 30);
        assert_eq!(a.parent_pid, 0);
        assert_eq!(a.dump_path, None);
        assert_eq!(a, CaptureArgs::for_output(0));
    }

    #[test]
    fn parses_a_full_command_line() {
        let a = parse(&[
            "--output",
            "1",
            "--fps",
            "60",
            "--format",
            "p010",
            "--size",
            "2560x1440",
            "--sdr-white",
            "80",
            "--cursor",
            "off",
            "--video-pipe",
            "v",
            "--audio",
            "on",
            "--audio-pipe",
            "s",
            "--audio-device",
            "{0.0.0.x}",
            "--parent-pid",
            "42",
        ])
        .unwrap();
        assert_eq!(a.output_index, 1);
        assert_eq!(a.format, CaptureFormat::P010);
        assert_eq!((a.width, a.height), (2560, 1440));
        assert!(!a.cursor);
        assert_eq!(a.audio_device, "{0.0.0.x}");
        assert_eq!(a.parent_pid, 42);
    }

    #[test]
    fn options_and_switch_values_ignore_case() {
        let a = parse(&[
            "--OUTPUT", "2", "--Format", "NV12", "--cursor", "No", "--audio", "FALSE", "--size",
            "1280X720",
        ])
        .unwrap();
        assert_eq!(a.output_index, 2);
        assert_eq!(a.format, CaptureFormat::Nv12);
        assert!(!a.cursor && !a.audio);
        assert_eq!((a.width, a.height), (1280, 720));
    }

    #[test]
    fn rejects_bad_sizes() {
        for size in [
            "2561x1440",
            "2560x1441",
            "2560",
            "0x0",
            "x1080",
            "1920x",
            "20000x2",
        ] {
            assert!(parse(&["--output", "0", "--size", size]).is_err(), "{size}");
        }
    }

    #[test]
    fn rejects_values_out_of_range_with_a_readable_message() {
        assert_eq!(
            parse(&["--output", "64"]).unwrap_err().to_string(),
            "--output: 64 is out of range [0..63]"
        );
        assert_eq!(
            parse(&["--output", "0", "--fps", "0"])
                .unwrap_err()
                .to_string(),
            "--fps: 0 is out of range [1..480]"
        );
        assert_eq!(
            parse(&["--output", "x"]).unwrap_err().to_string(),
            "--output: not an integer: x"
        );
        assert_eq!(
            parse(&["--output", "0", "--sdr-white", "0.5"])
                .unwrap_err()
                .to_string(),
            "--sdr-white: 0.5 is out of range [1..10000]"
        );
        assert!(parse(&["--output", "0", "--sdr-white", "NaN"]).is_err());
        assert_eq!(
            parse(&["--output", "0", "--cursor", "maybe"])
                .unwrap_err()
                .to_string(),
            "--cursor: expected on|off, got maybe"
        );
        assert_eq!(
            parse(&["--output", "0", "--format", "yuv"])
                .unwrap_err()
                .to_string(),
            "--format: expected auto|p010|nv12, got yuv"
        );
        assert_eq!(
            parse(&["--output"]).unwrap_err().to_string(),
            "missing value after --output"
        );
    }

    #[test]
    fn rejects_a_pipe_path_instead_of_a_name() {
        let error = parse(&["--output", "0", "--video-pipe", r"\\.\pipe\x"]).unwrap_err();
        assert_eq!(
            error.to_string(),
            r"--video-pipe: give only the pipe name (without \\.\pipe\), got \\.\pipe\x"
        );
        assert!(parse(&["--output", "0", "--audio-pipe", "a/b"]).is_err());
    }

    #[test]
    fn unknown_option_is_an_error() {
        assert_eq!(
            parse(&["--output", "0", "--turbo"])
                .unwrap_err()
                .to_string(),
            "unknown option: --turbo"
        );
    }

    #[test]
    fn dump_seconds_needs_a_dump_path() {
        assert!(parse(&["--output", "0", "--seconds", "5"]).is_err());
        let a = parse(&["--output", "0", "--dump", "x.p010", "--seconds", "5"]).unwrap();
        assert_eq!(
            (a.dump_path.as_deref(), a.dump_seconds),
            (Some("x.p010"), 5)
        );
        assert_eq!(CaptureArgs::parse(&a.to_argv()).unwrap(), a);
    }

    #[test]
    fn the_app_to_leave_out_survives_the_round_trip_to_the_engine() {
        let a = parse(&["--output", "0", "--audio-exclude", "discord"]).unwrap();
        assert_eq!(a.audio_exclude, "discord.exe");
        assert_eq!(
            CaptureArgs::parse(&a.to_argv()).unwrap().audio_exclude,
            "discord.exe"
        );
        assert!(parse(&["--output", "0", "--audio-exclude", " "]).is_err());
        assert!(!parse(&["--output", "0"])
            .unwrap()
            .to_argv()
            .contains(&"--audio-exclude".to_owned()));
    }

    #[test]
    fn argv_round_trip_is_stable() {
        let original = CaptureArgs {
            output_index: 3,
            fps: 120,
            format: CaptureFormat::Nv12,
            width: 1920,
            height: 1080,
            sdr_white: 100.0,
            cursor: true,
            video_pipe: "vp".into(),
            audio_pipe: "ap".into(),
            audio: true,
            audio_device: "dev-1".into(),
            parent_pid: 7,
            ..CaptureArgs::default()
        };
        let again = CaptureArgs::parse(&original.to_argv()).unwrap();
        assert_eq!(again, original);
        assert_eq!(again.to_argv(), original.to_argv());
    }

    /// What the app does for every session (SenderModelTests.Capture_args_round_trip_through_the_command_line).
    #[test]
    fn capture_args_round_trip_through_the_command_line() {
        let args = CaptureArgs {
            fps: 60,
            format: CaptureFormat::P010,
            width: 2560,
            height: 1440,
            sdr_white: 120.0,
            cursor: false,
            video_pipe: "vid".into(),
            audio_pipe: "aud".into(),
            parent_pid: 4321,
            ..CaptureArgs::for_output(2)
        };
        let argv = args.to_argv();
        assert_eq!(
            argv,
            [
                "--output",
                "2",
                "--fps",
                "60",
                "--format",
                "p010",
                "--size",
                "2560x1440",
                "--sdr-white",
                "120",
                "--cursor",
                "off",
                "--video-pipe",
                "vid",
                "--audio",
                "on",
                "--audio-pipe",
                "aud",
                "--parent-pid",
                "4321",
            ]
        );
        let parsed = CaptureArgs::parse(&argv).unwrap();
        assert_eq!(parsed.output_index, 2);
        assert_eq!(parsed.fps, 60);
        assert_eq!(parsed.format, CaptureFormat::P010);
        assert_eq!((parsed.width, parsed.height), (2560, 1440));
        assert!(!parsed.cursor);
        assert_eq!(parsed.video_pipe, "vid");
        assert_eq!(parsed.audio_pipe, "aud");
        assert_eq!(parsed.parent_pid, 4321);
        assert_eq!(parsed.sdr_white, 120.0);
    }

    #[test]
    fn audio_off_leaves_the_audio_options_out() {
        let args = CaptureArgs {
            audio: false,
            audio_device: "dev".into(),
            audio_exclude: "a.exe".into(),
            ..CaptureArgs::for_output(0)
        };
        let argv = args.to_argv();
        assert!(argv.windows(2).any(|w| w == ["--audio", "off"]));
        assert!(!argv
            .iter()
            .any(|a| a == "--audio-pipe" || a == "--audio-device" || a == "--audio-exclude"));
    }

    #[test]
    fn argv_is_quoted_by_the_windows_rules() {
        assert_eq!(
            hdrcast_capture::quote_argv(&[
                "--dump",
                r"C:\My Files\a.p010",
                "",
                r#"say "hi""#,
                r"trail\"
            ]),
            r#"--dump "C:\My Files\a.p010" "" "say \"hi\"" trail\"#
        );
        assert_eq!(
            hdrcast_capture::quote_argv(&[r"dir with space\"]),
            r#""dir with space\\""#
        );
    }

    #[test]
    fn usage_lists_every_option() {
        let usage = CaptureArgs::usage("hdrcast.exe");
        assert!(usage
            .starts_with("usage: hdrcast.exe capture --output <idx> [options]\n  --output <idx>"));
        for option in [
            "--fps",
            "--format",
            "--size",
            "--sdr-white",
            "--cursor",
            "--video-pipe",
            "--audio-pipe",
            "--audio on|off",
            "--audio-device",
            "--audio-exclude",
            "--pipe-timeout",
            "--parent-pid",
            "--dump",
            "--selftest",
        ] {
            assert!(usage.contains(option), "{option}");
        }
    }
}

mod app_audio_target {
    use super::*;

    #[test]
    fn names_are_reduced_to_an_exe_file_name() {
        let cases = [
            ("Discord.exe", "Discord.exe"),
            (" discord ", "discord.exe"),
            (
                r"C:\Users\me\AppData\Local\Discord\app-1.0.9\Discord.exe",
                "Discord.exe",
            ),
            ("\"QQ.exe\"", "QQ.exe"),
            ("  Discord ", "Discord.exe"),
            ("game.EXE", "game.EXE"),
            ("", ""),
            ("a<b", ""),
            ("dir/", ""),
        ];
        for (input, expected) in cases {
            assert_eq!(app_audio::normalize_exe_name(input), expected, "{input:?}");
        }
        assert_eq!(app_audio::normalize_exe_name(&"x".repeat(97)), "");
        assert_eq!(app_audio::normalize_exe_name(&"x".repeat(96)).len(), 100);
    }

    #[test]
    fn the_whole_app_is_left_out_through_its_topmost_process() {
        // Discord: the main process starts helpers with the same name, one of which plays the audio.
        let ps = [
            ProcessEntry::new(4, 0, "System"),
            ProcessEntry::new(50, 4, "explorer.exe"),
            ProcessEntry::new(100, 50, "Discord.exe"),
            ProcessEntry::new(101, 100, "Discord.exe"),
            ProcessEntry::new(102, 100, "Discord.exe"),
            ProcessEntry::new(103, 101, "Discord.exe"),
            ProcessEntry::new(200, 999, "Discord.exe"), // a stray one whose parent is gone
        ];
        assert_eq!(app_audio::find_root(&ps, "discord.exe"), 100);
        assert_eq!(app_audio::find_root(&ps, "QQ.exe"), 0);
        assert_eq!(app_audio::find_root(&ps, ""), 0);
    }

    #[test]
    fn reused_parent_ids_cannot_make_the_walk_loop() {
        let ps = [
            ProcessEntry::new(5, 6, "a.exe"),
            ProcessEntry::new(6, 5, "a.exe"),
            ProcessEntry::new(7, 7, "a.exe"),
        ];
        assert!([5, 6, 7].contains(&app_audio::find_root(&ps, "a.exe")));
    }
}

mod capture_stats {
    use super::*;

    fn sample() -> CaptureStats {
        CaptureStats {
            t: 12.0,
            captured: 700,
            duplicated: 20,
            dropped: 1,
            fps: 59.94,
            gpu_ms: 0.834,
            audio_underrun: 2,
            pipe_queue_ms: 16.666,
        }
    }

    #[test]
    fn the_line_has_exactly_the_documented_keys_in_order() {
        assert_eq!(
            sample().to_json_line(),
            r#"{"t":12.0,"captured":700,"duplicated":20,"dropped":1,"fps":59.9,"gpu_ms":0.83,"audio_underrun":2,"pipe_queue_ms":16.7}"#
        );
    }

    #[test]
    fn json_line_round_trips() {
        let back = CaptureStats::try_parse(&sample().to_json_line()).unwrap();
        assert_eq!(back.captured, 700);
        assert_eq!(back.duplicated, 20);
        assert_eq!(back.dropped, 1);
        assert_eq!(back.fps, 59.9);
        assert_eq!(back.gpu_ms, 0.83);
        assert_eq!(back.audio_underrun, 2);
        assert_eq!(back.t, 12.0);
    }

    #[test]
    fn lines_of_the_csharp_engine_parse_too() {
        // "0.80" with its trailing zero, CRLF, and no pipe_queue_ms in the oldest builds.
        let line = "{\"t\":5.3,\"captured\":311,\"duplicated\":3,\"dropped\":14375,\"fps\":58.0,\"gpu_ms\":0.80,\"audio_underrun\":0}\r\n";
        let s = CaptureStats::try_parse(line).unwrap();
        assert_eq!((s.captured, s.duplicated, s.dropped), (311, 3, 14375));
        assert_eq!((s.fps, s.gpu_ms, s.pipe_queue_ms), (58.0, 0.8, 0.0));
    }

    #[test]
    fn non_stats_lines_are_rejected() {
        assert_eq!(CaptureStats::try_parse("capture: starting"), None);
        assert_eq!(CaptureStats::try_parse(""), None);
        assert_eq!(CaptureStats::try_parse("{not json}"), None);
        assert_eq!(CaptureStats::try_parse("{}"), None);
        assert_eq!(CaptureStats::try_parse("[1,2]"), None);
        assert_eq!(CaptureStats::try_parse("{\"note\":\"text only\"}"), None);
    }
}

mod output_info {
    use super::*;

    fn sample() -> OutputInfo {
        OutputInfo {
            index: 1,
            adapter_index: 0,
            output_index_on_adapter: 1,
            adapter: "NVIDIA GeForce RTX 4090".into(),
            device_name: r"\\.\DISPLAY2".into(),
            friendly_name: "AW3225QF".into(),
            width: 3840,
            height: 2160,
            refresh_rate: 239.76,
            hdr: true,
            color_space: OutputInfo::COLOR_SPACE_HDR10,
            bits_per_color: 10,
            max_luminance: 1000.0,
            attached_to_desktop: true,
            rotation: 1,
            ..OutputInfo::default()
        }
    }

    #[test]
    fn json_round_trips() {
        let back = OutputInfo::from_json(&OutputInfo::to_json(&[sample()])).unwrap();
        assert_eq!(back, [sample()]);
        assert_eq!(back[0].device_name, r"\\.\DISPLAY2");
        assert!(back[0].hdr);
        assert_eq!(back[0].max_luminance, 1000.0);
    }

    #[test]
    fn json_keeps_the_field_names_and_number_style_of_the_csharp_sender() {
        assert_eq!(
            OutputInfo::to_json(&[sample()]),
            concat!(
                r#"[{"index":1,"adapterIndex":0,"outputIndexOnAdapter":1,"adapter":"NVIDIA GeForce RTX 4090","#,
                r#""deviceName":"\\\\.\\DISPLAY2","friendlyName":"AW3225QF","width":3840,"height":2160,"x":0,"y":0,"#,
                r#""rotation":1,"refreshRate":239.76,"attachedToDesktop":true,"colorSpace":12,"hdr":true,"#,
                r#""bitsPerColor":10,"minLuminance":0,"maxLuminance":1000,"maxFullFrameLuminance":0}]"#
            )
        );
    }

    #[test]
    fn json_written_by_the_csharp_sender_still_loads() {
        // As printed by HdrCastSender.exe 0.2.4 (17 significant digits, unknown members ignored, missing ones default).
        let json = r#"[{"index":2,"adapterIndex":1,"outputIndexOnAdapter":0,"adapter":"AMD Radeon(TM) Graphics","deviceName":"\\\\.\\DISPLAY5","friendlyName":"AV Receiver","width":3840,"height":2160,"x":5760,"y":0,"rotation":1,"refreshRate":59.940059940059939,"attachedToDesktop":true,"colorSpace":12,"hdr":true,"bitsPerColor":8,"minLuminance":0.0002,"maxLuminance":426,"maxFullFrameLuminance":265,"future":1},{"index":3}]"#;
        let outputs = OutputInfo::from_json(json).unwrap();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].friendly_name, "AV Receiver");
        assert_eq!(outputs[0].refresh_rate, 60000.0 / 1001.0);
        assert_eq!(outputs[0].min_luminance, 0.0002);
        assert_eq!(outputs[0].x, 5760);
        assert_eq!(
            outputs[1],
            OutputInfo {
                index: 3,
                ..OutputInfo::default()
            }
        );
        assert!(OutputInfo::from_json("null").unwrap().is_empty());
        assert!(OutputInfo::from_json("nonsense").is_err());
    }

    #[test]
    fn label_shows_index_name_size_and_hdr() {
        assert_eq!(
            sample().label(),
            r"1: AW3225QF (\\.\DISPLAY2) 3840x2160@239.8 HDR 1000nit"
        );
        let sdr = OutputInfo {
            hdr: false,
            friendly_name: String::new(),
            refresh_rate: 60.0,
            ..sample()
        };
        assert_eq!(sdr.label(), r"1: Display (\\.\DISPLAY2) 3840x2160@60 SDR");
    }

    #[test]
    fn rotation_helpers() {
        let mut o = sample();
        assert!(!o.is_rotated90());
        o.rotation = 2;
        assert!(o.is_rotated90());
        assert_eq!(o.rotation_name(), "90");
    }

    #[test]
    fn table_has_a_header_and_one_row_per_output() {
        let table = OutputInfo::to_table(&[sample(), sample()]);
        let lines: Vec<&str> = table.trim_end().split('\n').collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("idx"));
        assert_eq!(
            lines[1],
            r"1    \\.\DISPLAY2   AW3225QF                  3840x2160   0,0          239.76 on   12          10    1000    0       0       NVIDIA GeForce RTX 4090"
        );
    }
}
