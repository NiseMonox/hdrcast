//! The tests of tests/RegressionTests.cs and tests/P2pPlumbingTests.cs that exercise only this crate's types:
//! defects found in review or in the field; each test fails again if the fix is undone.
//! (EncoderProbeErrorTests lives next to `first_error` in src/sender/encoder_probe.rs.)

use std::path::Path;

use hdrcast_capture::CaptureFormat;
use hdrcast_core::common::srt_url;
use hdrcast_media::receiver::mpv_command;
use hdrcast_media::receiver::mpv_log::{self, MpvExitReason};
use hdrcast_media::receiver::{DisplayMode, ReceiverSettings};
use hdrcast_media::sender::encoder_profiles::{EncoderSettings, VideoEncoder};
use hdrcast_media::sender::ffmpeg_command::{self, FfmpegPlan};
use hdrcast_media::sender::{FfmpegProgressParser, SenderSettings};
use hdrcast_media::Platform;

mod mpv_exit_classification {
    use super::*;

    /// mpv prints the URL it failed to open, and our URL always contains the word "passphrase". Classifying
    /// the raw line made every failure look like an authentication failure, so a relay that simply had no
    /// publisher yet was reported to the viewer as a wrong password.
    #[test]
    fn an_echoed_url_is_not_mistaken_for_an_auth_failure() {
        let lines = [
            "[ffmpeg] srt: Connection to srt://127.0.0.1:8890?streamid=read:screen:viewer:pw&passphrase=abcdefghij1234&latency=1500000 failed: Input/output error",
            "Failed to open srt://127.0.0.1:8890?streamid=read:screen:viewer:pw&passphrase=abcdefghij1234&latency=1500000.",
        ];
        assert_ne!(mpv_log::classify(lines), MpvExitReason::AuthFailed);
        assert_eq!(mpv_log::classify(lines), MpvExitReason::ConnectionFailed);
    }

    #[test]
    fn a_relay_without_a_publisher_is_recognised_even_with_the_url_present() {
        let lines = ["[ffmpeg/demuxer] srt://h:8890?passphrase=abcdefghij: no stream is available on path 'screen'"];
        assert_eq!(mpv_log::classify(lines), MpvExitReason::NoPublisher);
        assert!(mpv_log::is_stream_unavailable(lines[0]));
    }

    #[test]
    fn a_real_passphrase_error_is_still_detected() {
        for line in [
            "[ffmpeg] srt: closed: invalid passphrase",
            "[ffmpeg] srt: closed: passphrase not provided by client",
            "[ffmpeg] srt: failed to authenticate: authentication failed",
        ] {
            assert_eq!(
                mpv_log::classify_line(line),
                MpvExitReason::AuthFailed,
                "{line}"
            );
        }
    }

    #[test]
    fn url_stripping_keeps_the_rest_of_the_line() {
        let stripped = mpv_log::strip_urls("Failed to open srt://h:8890?passphrase=x now.");
        assert!(!stripped.contains("passphrase"));
        assert!(stripped.contains("Failed to open"));
        assert!(stripped.contains("now."));
        assert_eq!(stripped, "Failed to open <url> now.");
    }
}

mod receiver_settings_clamp {
    use super::*;

    /// mpv accepts "auto" or 10..10000 for --target-peak; 1..9 makes it refuse to start.
    #[test]
    fn target_peak_is_clamped_to_a_value_mpv_accepts() {
        for (input, expected) in [
            (0, 0),
            (5, 0),
            (9, 0),
            (10, 10),
            (400, 400),
            (99999, 10000),
            (-3, 0),
        ] {
            let mut s = ReceiverSettings {
                target_peak_nits: input,
                ..ReceiverSettings::default()
            };
            s.normalize();
            assert_eq!(s.target_peak_nits, expected, "target peak {input}");
        }
    }

    #[test]
    fn a_clamped_target_peak_never_reaches_the_command_line_as_a_bad_value() {
        let s = ReceiverSettings {
            display_mode: DisplayMode::SdrToneMap,
            target_peak_nits: 5,
            ..ReceiverSettings::default()
        }
        .normalized();
        for platform in [Platform::Windows, Platform::MacOs] {
            let plan = mpv_command::build(
                &s,
                40123,
                "0123456789abcdef",
                "ipc",
                Path::new("mpv"),
                None,
                platform,
            )
            .unwrap();
            for a in &plan.mpv_args {
                assert!(
                    !a.starts_with("--target-peak=") || a == "--target-peak=auto",
                    "unexpected {a}"
                );
            }
        }
    }
}

mod progress_on_stderr {
    use super::*;

    #[test]
    fn progress_lines_are_told_apart_from_log_messages() {
        for (line, progress) in [
            ("frame=120", true),
            ("out_time_us=4000000", true),
            ("stream_0_0_q=23.0", true),
            ("progress=continue", true),
            (
                "[hevc_nvenc @ 000001] Lookahead depth set to 20 = too deep",
                false,
            ),
            ("Last message repeated 3 times", false),
            ("Some Message=with caps", false),
            ("=nothing", false),
            ("", false),
        ] {
            assert_eq!(
                FfmpegProgressParser::is_progress_line(line),
                progress,
                "{line:?}"
            );
        }
    }

    #[test]
    fn p2p_plans_send_the_stream_to_stdout_and_progress_to_stderr() {
        let plan = FfmpegPlan {
            width: 1920,
            height: 1080,
            fps: 60,
            pixel_format: CaptureFormat::P010,
            encoder: EncoderSettings {
                encoder: VideoEncoder::LibX265,
                hdr: true,
                bitrate_kbps: 20000,
                fps: 60,
                ..EncoderSettings::default()
            },
            output_url: "pipe:1".to_string(),
            progress_url: "pipe:2".to_string(),
            ..FfmpegPlan::default()
        };
        let args = ffmpeg_command::build(&plan).unwrap();
        let i = args.iter().position(|a| a == "-progress").unwrap();
        assert_eq!(args[i + 1], "pipe:2");
        assert_eq!(args.last().unwrap(), "pipe:1");
        assert!(args.iter().any(|a| a == "mpegts"));
    }

    #[test]
    fn sender_settings_need_no_connection_fields() {
        let s = SenderSettings::default().normalized();
        assert!(s.validate().is_empty());
        assert_eq!(s.max_viewers, 4);
        assert_eq!(
            SenderSettings {
                max_viewers: 99,
                ..SenderSettings::default()
            }
            .normalized()
            .max_viewers,
            16
        );
    }

    #[test]
    fn receiver_settings_become_a_local_caller() {
        let r = ReceiverSettings {
            latency_ms: 3500,
            ..ReceiverSettings::default()
        }
        .normalized();
        assert!(r.validate().is_empty());
        assert_eq!(r.overhead_percent(), 50);
        let url =
            srt_url::build(&r.to_srt_options(40123, "a77MWmBi3_603E3kH2abfOI5O-dIU5dn")).unwrap();
        assert!(
            url.starts_with("srt://127.0.0.1:40123?mode=caller"),
            "{url}"
        );
        assert!(url.contains("latency=3500000"));
        assert!(url.contains("pbkeylen=32"));
    }
}
