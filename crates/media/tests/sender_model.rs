//! Port of tests/SenderModelTests.cs, plus what the Rust version added (platforms, VideoToolbox).

use hdrcast_capture::{CaptureArgs, CaptureFormat, FrameLayout, OutputInfo};
use hdrcast_media::sender::encoder_profiles::{self, EncoderSettings, QualityPreset, VideoEncoder};
use hdrcast_media::sender::ffmpeg_command::{self, AudioCodec, FfmpegPlan, FfmpegPlanError};
use hdrcast_media::sender::{FfmpegProgressParser, HdrMode, ScaleMode, SenderSettings};
use hdrcast_media::Platform;

/// The value after the first `key`.
fn value<'a>(args: &'a [String], key: &str) -> &'a str {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .map_or("", String::as_str)
}

/// Last occurrence: "-f" appears once per input and once for the output format.
fn last_value<'a>(args: &'a [String], key: &str) -> &'a str {
    args.iter()
        .rposition(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .map_or("", String::as_str)
}

fn has(args: &[String], arg: &str) -> bool {
    args.iter().any(|a| a == arg)
}

mod encoder_profile {
    use super::*;

    fn hevc(preset: QualityPreset, hdr: bool) -> EncoderSettings {
        EncoderSettings {
            encoder: VideoEncoder::HevcNvenc,
            preset,
            hdr,
            bitrate_kbps: 40000,
            fps: 60,
            gop_seconds: 2.0,
            buffer_seconds: 2.0,
            ..EncoderSettings::default()
        }
    }

    #[test]
    fn nvenc_hdr_uses_main10_and_pq_bt2020_tags() {
        let a = encoder_profiles::build_video_args(&hevc(QualityPreset::Balanced, true));
        assert_eq!(value(&a, "-c:v"), "hevc_nvenc");
        assert_eq!(value(&a, "-profile:v"), "main10");
        assert_eq!(value(&a, "-color_primaries"), "bt2020");
        assert_eq!(value(&a, "-color_trc"), "smpte2084");
        assert_eq!(value(&a, "-colorspace"), "bt2020nc");
        assert_eq!(value(&a, "-color_range"), "tv");
    }

    #[test]
    fn nvenc_sdr_uses_bt709_tags() {
        let a = encoder_profiles::build_video_args(&hevc(QualityPreset::Balanced, false));
        assert_eq!(value(&a, "-profile:v"), "main");
        assert_eq!(value(&a, "-color_primaries"), "bt709");
        assert_eq!(value(&a, "-color_trc"), "bt709");
        assert_eq!(value(&a, "-colorspace"), "bt709");
    }

    #[test]
    fn gop_is_fps_times_seconds() {
        let mut s = hevc(QualityPreset::Balanced, true);
        (s.fps, s.gop_seconds) = (60, 2.0);
        assert_eq!(value(&encoder_profiles::build_video_args(&s), "-g"), "120");
        (s.fps, s.gop_seconds) = (30, 1.5);
        assert_eq!(value(&encoder_profiles::build_video_args(&s), "-g"), "45");
    }

    #[test]
    fn cbr_sets_maxrate_and_a_buffer_of_buffer_seconds() {
        let mut s = hevc(QualityPreset::Balanced, true);
        s.bitrate_kbps = 40000;
        s.buffer_seconds = 2.0;
        let a = encoder_profiles::build_video_args(&s);
        assert_eq!(value(&a, "-rc"), "cbr");
        assert_eq!(value(&a, "-b:v"), "40000k");
        assert_eq!(value(&a, "-maxrate"), "40000k");
        assert_eq!(value(&a, "-bufsize"), "80000k");
    }

    #[test]
    fn quality_preset_spends_more_on_picture_than_speed() {
        let speed = encoder_profiles::build_video_args(&hevc(QualityPreset::Speed, true));
        let quality = encoder_profiles::build_video_args(&hevc(QualityPreset::Quality, true));
        assert_eq!(value(&speed, "-preset"), "p3");
        assert_eq!(value(&quality, "-preset"), "p7");
        assert_eq!(value(&speed, "-multipass"), "disabled");
        assert_eq!(value(&quality, "-multipass"), "fullres");
        assert_eq!(value(&speed, "-bf"), "0");
        assert_eq!(value(&quality, "-bf"), "3");
        assert_eq!(value(&speed, "-rc-lookahead"), "0");
        assert_eq!(value(&quality, "-rc-lookahead"), "32");
    }

    #[test]
    fn this_is_not_a_low_latency_tune() {
        // A buffered livestream keeps -tune hq; ll/ull would trade picture quality for latency we do not need.
        let a = encoder_profiles::build_video_args(&hevc(QualityPreset::Quality, true));
        assert_eq!(value(&a, "-tune"), "hq");
        assert!(!has(&a, "-zerolatency"));
        assert!(!has(&a, "-delay"));
    }

    #[test]
    fn b_frames_can_be_turned_off() {
        let mut s = hevc(QualityPreset::Quality, true);
        s.b_frames = false;
        let a = encoder_profiles::build_video_args(&s);
        assert_eq!(value(&a, "-bf"), "0");
        assert!(!has(&a, "-b_ref_mode"));
    }

    #[test]
    fn x265_hdr_carries_mastering_display_and_repeat_headers() {
        let s = EncoderSettings {
            encoder: VideoEncoder::LibX265,
            hdr: true,
            bitrate_kbps: 40000,
            fps: 60,
            gop_seconds: 2.0,
            buffer_seconds: 2.0,
            mastering_max_nits: 1000,
            ..EncoderSettings::default()
        };
        let a = encoder_profiles::build_video_args(&s);
        let p = value(&a, "-x265-params");
        assert!(p.contains("hdr-opt=1"));
        assert!(p.contains("repeat-headers=1")); // MPEG-TS needs VPS/SPS/PPS before every IDR
        assert!(p.contains("master-display="));
        assert!(p.contains("max-cll=1000,250"));
        assert!(p.contains("keyint=120"));
        assert!(p.contains("vbv-maxrate=40000"));
    }

    #[test]
    fn only_hevc_encoders_are_offered() {
        // The P2P fan-out carries MPEG-TS, which has no AV1 mapping in ffmpeg (the integration tests prove
        // it), so every encoder in the catalogue must be HEVC.
        for info in &encoder_profiles::ALL {
            assert!(
                info.display_name.starts_with("HEVC"),
                "{}",
                info.display_name
            );
            assert!(
                info.ffmpeg_name.contains("hevc") || info.ffmpeg_name.contains("265"),
                "{}",
                info.ffmpeg_name
            );
            assert!(info.supports_10bit);
        }
        assert_eq!(encoder_profiles::ALL.len(), 5);
        assert_eq!(encoder_profiles::for_platform(Platform::Windows).len(), 4); // what the C# sender offered
    }

    #[test]
    fn suggested_bitrate_grows_with_pixels_and_frame_rate() {
        let p1080 = encoder_profiles::suggested_mbps(1920, 1080, 60);
        let p1440 = encoder_profiles::suggested_mbps(2560, 1440, 60);
        let p2160 = encoder_profiles::suggested_mbps(3840, 2160, 60);
        assert!(p1080 < p1440 && p1440 < p2160);
        assert!((30..=50).contains(&p1440));
        assert!(encoder_profiles::suggested_mbps(2560, 1440, 30) < p1440);
        assert!((10..=100).contains(&p2160));
        // the exact numbers of the C# formula
        assert_eq!((p1080, p1440, p2160), (25, 40, 80));
        assert_eq!(encoder_profiles::suggested_mbps(640, 360, 30), 10);
        assert_eq!(encoder_profiles::suggested_mbps(7680, 4320, 120), 100);
    }

    #[test]
    fn every_encoder_produces_a_parseable_option_list() {
        for info in &encoder_profiles::ALL {
            for hdr in [false, true] {
                let s = EncoderSettings {
                    encoder: info.id,
                    hdr,
                    bitrate_kbps: 20000,
                    fps: 60,
                    ..Default::default()
                };
                let a = encoder_profiles::build_video_args(&s);
                assert_eq!(a[0], "-c:v");
                assert_eq!(a[1], info.ffmpeg_name);
                assert_eq!(a.len() % 2, 0); // every option has a value
                for option in a.iter().step_by(2) {
                    assert!(option.starts_with('-'), "{option}");
                }
            }
        }
    }

    /// The exact lists 0.2.4 passed to ffmpeg: the order matters to nobody but a diff between versions, and
    /// every one of these strings was tuned against real encoders.
    #[test]
    fn the_windows_option_lists_are_the_ones_of_0_2_4() {
        let settings = |encoder, preset, hdr| EncoderSettings {
            encoder,
            preset,
            hdr,
            bitrate_kbps: 40000,
            fps: 60,
            gop_seconds: 2.0,
            buffer_seconds: 2.0,
            b_frames: true,
            lookahead: true,
            mastering_max_nits: 1000,
        };
        let line = |s: &EncoderSettings| encoder_profiles::build_video_args(s).join(" ");
        const HDR: &str = "-color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv -chroma_sample_location left";
        const SDR: &str = "-color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv -chroma_sample_location left";

        assert_eq!(
            line(&settings(VideoEncoder::HevcNvenc, QualityPreset::Balanced, true)),
            format!(
                "-c:v hevc_nvenc -pix_fmt p010le -preset p5 -tune hq -rc cbr -multipass qres -rc-lookahead 20 -bf 2 \
                 -b_ref_mode middle -spatial-aq 1 -aq-strength 8 -temporal-aq 0 -profile:v main10 -tier high -b:v 40000k \
                 -maxrate 40000k -bufsize 80000k -g 120 -forced-idr 1 -no-scenecut 0 {HDR}"
            )
        );
        assert_eq!(
            line(&settings(VideoEncoder::HevcNvenc, QualityPreset::Quality, false)),
            format!(
                "-c:v hevc_nvenc -pix_fmt nv12 -preset p7 -tune hq -rc cbr -multipass fullres -rc-lookahead 32 -bf 3 \
                 -b_ref_mode middle -spatial-aq 1 -aq-strength 8 -temporal-aq 1 -profile:v main -tier high -b:v 40000k \
                 -maxrate 40000k -bufsize 80000k -g 120 -forced-idr 1 -no-scenecut 0 {SDR}"
            )
        );
        assert_eq!(
            line(&settings(VideoEncoder::HevcNvenc, QualityPreset::Speed, true)),
            format!(
                "-c:v hevc_nvenc -pix_fmt p010le -preset p3 -tune hq -rc cbr -multipass disabled -rc-lookahead 0 -bf 0 \
                 -spatial-aq 1 -aq-strength 8 -profile:v main10 -tier high -b:v 40000k -maxrate 40000k -bufsize 80000k \
                 -g 120 -forced-idr 1 {HDR}"
            )
        );
        assert_eq!(
            line(&settings(VideoEncoder::HevcAmf, QualityPreset::Balanced, true)),
            format!(
                "-c:v hevc_amf -pix_fmt p010le -quality balanced -rc cbr -b:v 40000k -maxrate 40000k -bufsize 80000k \
                 -g 120 -latency 0 -preanalysis 1 -pa_lookahead_buffer_depth 20 -vbaq 1 -profile:v main10 {HDR}"
            )
        );
        assert_eq!(
            line(&settings(VideoEncoder::HevcAmf, QualityPreset::Speed, false)),
            format!(
                "-c:v hevc_amf -pix_fmt nv12 -quality speed -rc cbr -b:v 40000k -maxrate 40000k -bufsize 80000k -g 120 \
                 -latency 0 -vbaq 1 {SDR}"
            )
        );
        assert_eq!(
            line(&settings(VideoEncoder::HevcQsv, QualityPreset::Quality, true)),
            format!(
                "-c:v hevc_qsv -pix_fmt p010le -preset veryslow -b:v 40000k -maxrate 40000k -bufsize 80000k -g 120 -bf 3 \
                 -extbrc 1 -look_ahead_depth 32 -b_strategy 1 -profile:v main10 {HDR}"
            )
        );
        assert_eq!(
            line(&settings(VideoEncoder::HevcQsv, QualityPreset::Speed, false)),
            format!("-c:v hevc_qsv -pix_fmt nv12 -preset faster -b:v 40000k -maxrate 40000k -bufsize 80000k -g 120 -bf 0 {SDR}")
        );
        assert_eq!(
            line(&settings(VideoEncoder::LibX265, QualityPreset::Balanced, true)),
            format!(
                "-c:v libx265 -pix_fmt yuv420p10le -preset fast -b:v 40000k -x265-params \
                 keyint=120:min-keyint=120:scenecut=0:repeat-headers=1:bframes=2:rc-lookahead=20:vbv-maxrate=40000:\
                 vbv-bufsize=80000:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:range=limited:hdr-opt=1:\
                 master-display=G(8500,39850)B(6550,2300)R(35400,14600)WP(15635,16450)L(10000000,1):max-cll=1000,250 {HDR}"
            )
        );
        assert_eq!(
            line(&settings(VideoEncoder::LibX265, QualityPreset::Speed, false)),
            format!(
                "-c:v libx265 -pix_fmt yuv420p -preset veryfast -b:v 40000k -x265-params \
                 keyint=120:min-keyint=120:scenecut=0:repeat-headers=1:bframes=0:rc-lookahead=10:vbv-maxrate=40000:\
                 vbv-bufsize=80000:colorprim=bt709:transfer=bt709:colormatrix=bt709:range=limited {SDR}"
            )
        );
        // switches off: no lookahead means no temporal AQ and no scene-cut option; the AMF depth floor is 1
        let mut plain = settings(VideoEncoder::HevcNvenc, QualityPreset::Quality, true);
        (plain.b_frames, plain.lookahead) = (false, false);
        assert_eq!(
            line(&plain),
            format!(
                "-c:v hevc_nvenc -pix_fmt p010le -preset p7 -tune hq -rc cbr -multipass fullres -rc-lookahead 0 -bf 0 \
                 -spatial-aq 1 -aq-strength 8 -profile:v main10 -tier high -b:v 40000k -maxrate 40000k -bufsize 80000k \
                 -g 120 -forced-idr 1 {HDR}"
            )
        );
        plain.encoder = VideoEncoder::HevcAmf;
        assert!(line(&plain).contains("-preanalysis 1 -pa_lookahead_buffer_depth 1 -vbaq 1"));
        plain.encoder = VideoEncoder::HevcQsv;
        assert!(line(&plain).contains("-bf 0 -extbrc 1 -look_ahead_depth 10 -b_strategy 1"));
        // an unknown peak falls back to 1000 nit; a dim display still gets a sane MaxFALL
        plain.encoder = VideoEncoder::LibX265;
        plain.mastering_max_nits = 0;
        assert!(line(&plain).contains("L(10000000,1):max-cll=1000,250"));
        plain.mastering_max_nits = 120;
        assert!(line(&plain).contains("L(1200000,1):max-cll=120,50"));
    }
}

mod videotoolbox {
    use super::*;

    fn settings(preset: QualityPreset, hdr: bool) -> EncoderSettings {
        EncoderSettings {
            encoder: VideoEncoder::HevcVideoToolbox,
            preset,
            hdr,
            bitrate_kbps: 40000,
            fps: 60,
            gop_seconds: 2.0,
            buffer_seconds: 2.0,
            b_frames: true,
            lookahead: true,
            mastering_max_nits: 1000,
        }
    }

    #[test]
    fn hdr_is_main10_from_p010_with_the_same_colour_tags_as_every_encoder() {
        let a = encoder_profiles::build_video_args(&settings(QualityPreset::Balanced, true));
        assert_eq!(
            a.join(" "),
            "-c:v hevc_videotoolbox -pix_fmt p010le -profile:v main10 -b:v 40000k -maxrate 40000k -bufsize 80000k \
             -g 120 -bf 2 -realtime 1 -allow_sw 0 \
             -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv -chroma_sample_location left"
        );
    }

    #[test]
    fn sdr_is_main_from_nv12() {
        let a = encoder_profiles::build_video_args(&settings(QualityPreset::Quality, false));
        assert_eq!(
            a.join(" "),
            "-c:v hevc_videotoolbox -pix_fmt nv12 -profile:v main -b:v 40000k -maxrate 40000k -bufsize 80000k \
             -g 120 -bf 3 -realtime 1 -allow_sw 0 \
             -color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv -chroma_sample_location left"
        );
    }

    #[test]
    fn b_frames_follow_the_switch_and_the_preset_and_the_gop_follows_the_settings() {
        let mut s = settings(QualityPreset::Quality, true);
        s.b_frames = false;
        assert_eq!(value(&encoder_profiles::build_video_args(&s), "-bf"), "0");
        assert_eq!(
            value(
                &encoder_profiles::build_video_args(&settings(QualityPreset::Speed, true)),
                "-bf"
            ),
            "0"
        );
        (s.fps, s.gop_seconds) = (30, 1.0);
        assert_eq!(value(&encoder_profiles::build_video_args(&s), "-g"), "30");
    }

    #[test]
    fn it_is_a_hardware_encoder_that_takes_the_capture_format() {
        let info = encoder_profiles::info(VideoEncoder::HevcVideoToolbox);
        assert_eq!(
            (info.ffmpeg_name, info.vendor, info.is_hardware),
            ("hevc_videotoolbox", "Apple", true)
        );
        assert_eq!(
            encoder_profiles::pixel_format(VideoEncoder::HevcVideoToolbox, true),
            "p010le"
        );
        assert_eq!(
            encoder_profiles::pixel_format(VideoEncoder::HevcVideoToolbox, false),
            "nv12"
        );
        assert_eq!(
            encoder_profiles::by_ffmpeg_name("HEVC_VideoToolbox").map(|i| i.id),
            Some(VideoEncoder::HevcVideoToolbox)
        );
        assert_eq!(encoder_profiles::by_ffmpeg_name("av1_nvenc"), None);
    }

    #[test]
    fn a_whole_mac_command_line_feeds_it_tagged_p010() {
        let plan = FfmpegPlan {
            video_pipe_name: "hdrcast-video-1".to_string(),
            width: 3840,
            height: 2160,
            encoder: settings(QualityPreset::Balanced, true),
            ..FfmpegPlan::default()
        };
        let a = ffmpeg_command::build(&plan).unwrap();
        let input_end = a.iter().position(|x| x == "-i").unwrap();
        assert_eq!(value(&a[..input_end], "-pix_fmt"), "p010le");
        assert_eq!(value(&a[..input_end], "-color_trc"), "smpte2084");
        assert_eq!(value(&a, "-c:v"), "hevc_videotoolbox");
        assert_eq!(a.last().unwrap(), "pipe:1");
    }
}

mod platforms {
    use super::*;

    fn ids(platform: Platform) -> Vec<VideoEncoder> {
        encoder_profiles::for_platform(platform)
            .iter()
            .map(|info| info.id)
            .collect()
    }

    #[test]
    fn each_platform_offers_its_own_encoders_best_first() {
        use VideoEncoder::*;
        assert_eq!(
            ids(Platform::Windows),
            [HevcNvenc, HevcAmf, HevcQsv, LibX265]
        );
        assert_eq!(ids(Platform::MacOs), [HevcVideoToolbox, LibX265]);
        assert_eq!(ids(Platform::Other), ids(Platform::Windows));
        for platform in [Platform::Windows, Platform::MacOs, Platform::Other] {
            assert_eq!(
                encoder_profiles::auto_order(platform).collect::<Vec<_>>(),
                ids(platform)
            );
            assert_eq!(
                encoder_profiles::default_encoder(platform),
                ids(platform)[0]
            );
            assert_eq!(
                SenderSettings::defaults_for(platform).encoder,
                ids(platform)[0]
            );
            // the CPU encoder is everywhere, and always the last resort
            assert_eq!(ids(platform).last(), Some(&LibX265));
        }
        assert!(encoder_profiles::is_offered(HevcNvenc, Platform::Windows));
        assert!(!encoder_profiles::is_offered(HevcNvenc, Platform::MacOs));
        assert!(!encoder_profiles::is_offered(
            HevcVideoToolbox,
            Platform::Windows
        ));
    }

    #[test]
    fn the_encoder_numbers_are_the_ones_sender_json_stores() {
        use VideoEncoder::*;
        assert_eq!(
            [HevcNvenc, HevcAmf, HevcQsv, LibX265, HevcVideoToolbox].map(VideoEncoder::number),
            [0, 2, 4, 5, 6]
        );
        assert_eq!(
            encoder_profiles::ALL.map(|info| info.id.number()),
            [0, 2, 4, 5, 6]
        );
        assert_eq!(VideoEncoder::ALL.len(), encoder_profiles::ALL.len());
        assert_eq!(
            [
                QualityPreset::Speed,
                QualityPreset::Balanced,
                QualityPreset::Quality
            ]
            .map(QualityPreset::number),
            [0, 1, 2]
        );
        assert_eq!(
            [AudioCodec::None, AudioCodec::Aac, AudioCodec::Opus].map(AudioCodec::number),
            [0, 1, 2]
        );
        assert_eq!(
            ScaleMode::ALL
                .iter()
                .map(|m| m.number())
                .collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        assert_eq!(
            [HdrMode::Auto, HdrMode::ForceSdr].map(HdrMode::number),
            [0, 1]
        );
    }

    #[test]
    fn an_encoder_of_another_platform_becomes_this_platforms_default() {
        let mut from_windows = SenderSettings {
            encoder: VideoEncoder::HevcAmf,
            ..SenderSettings::defaults_for(Platform::Windows)
        };
        assert_eq!(
            from_windows.validate_for(Platform::MacOs),
            ["Pick an encoder."]
        );
        from_windows.normalize_for(Platform::MacOs);
        assert_eq!(from_windows.encoder, VideoEncoder::HevcVideoToolbox);
        assert!(from_windows.validate_for(Platform::MacOs).is_empty());

        let mut from_mac = SenderSettings::defaults_for(Platform::MacOs);
        assert_eq!(from_mac.encoder, VideoEncoder::HevcVideoToolbox);
        from_mac.normalize_for(Platform::Windows);
        assert_eq!(from_mac.encoder, VideoEncoder::HevcNvenc);

        // x265 exists everywhere and is left alone
        let mut cpu = SenderSettings {
            encoder: VideoEncoder::LibX265,
            ..SenderSettings::default()
        };
        cpu.normalize_for(Platform::MacOs)
            .normalize_for(Platform::Windows);
        assert_eq!(cpu.encoder, VideoEncoder::LibX265);
    }

    #[test]
    fn the_catalogue_serialises_for_the_ui() {
        let v = serde_json::to_value(encoder_profiles::for_platform(Platform::MacOs)).unwrap();
        assert_eq!(
            v[0],
            serde_json::json!({"id": 6, "ffmpegName": "hevc_videotoolbox", "displayName": "HEVC (Apple VideoToolbox)",
                               "vendor": "Apple", "isHardware": true, "supports10Bit": true})
        );
        assert_eq!(v[1]["id"], 5);
    }
}

mod ffmpeg_command_line {
    use super::*;

    fn plan(hdr: bool, audio: bool) -> FfmpegPlan {
        FfmpegPlan {
            video_pipe_name: "hdrcast-video-1234".to_string(),
            audio_pipe_name: audio.then(|| "hdrcast-audio-1234".to_string()),
            pixel_format: if hdr {
                CaptureFormat::P010
            } else {
                CaptureFormat::Nv12
            },
            width: 2560,
            height: 1440,
            fps: 60,
            encoder: EncoderSettings {
                encoder: VideoEncoder::HevcNvenc,
                hdr,
                bitrate_kbps: 40000,
                fps: 60,
                ..EncoderSettings::default()
            },
            audio: if audio {
                AudioCodec::Aac
            } else {
                AudioCodec::None
            },
            ..FfmpegPlan::default()
        }
    }

    #[test]
    fn builds_raw_video_input_from_the_named_pipe() {
        let a = ffmpeg_command::build(&plan(true, true)).unwrap();
        assert_eq!(value(&a, "-f"), "rawvideo");
        assert_eq!(value(&a, "-pix_fmt"), "p010le");
        assert_eq!(value(&a, "-video_size"), "2560x1440");
        assert_eq!(value(&a, "-framerate"), "60");
        assert_eq!(
            value(&a, "-i"),
            hdrcast_capture::pipe_path("hdrcast-video-1234")
        );
        #[cfg(windows)]
        assert!(has(&a, r"\\.\pipe\hdrcast-video-1234"));
    }

    #[test]
    fn audio_input_is_f32le_48k_stereo() {
        let a = ffmpeg_command::build(&plan(true, true)).unwrap();
        assert!(has(&a, "f32le"));
        assert!(has(&a, "48000"));
        assert_eq!(
            last_value(&a, "-i"),
            hdrcast_capture::pipe_path("hdrcast-audio-1234")
        );
        #[cfg(windows)]
        assert!(has(&a, r"\\.\pipe\hdrcast-audio-1234"));
        assert_eq!(value(&a, "-c:a"), "aac");
        assert_eq!(value(&a, "-b:a"), "192k");
    }

    #[test]
    fn the_raw_video_input_itself_is_colour_tagged() {
        // Output-side tags alone let ffmpeg 7.1+ re-matrix the frames and drop transfer / primaries (v0.2.1).
        for hdr in [true, false] {
            let a = ffmpeg_command::build(&plan(hdr, true)).unwrap();
            let input = &a[..a.iter().position(|x| x == "-i").unwrap()];
            assert_eq!(
                value(input, "-color_primaries"),
                if hdr { "bt2020" } else { "bt709" }
            );
            assert_eq!(
                value(input, "-color_trc"),
                if hdr { "smpte2084" } else { "bt709" }
            );
            assert_eq!(
                value(input, "-colorspace"),
                if hdr { "bt2020nc" } else { "bt709" }
            );
            assert_eq!(value(input, "-color_range"), "tv");
        }
    }

    #[test]
    fn without_audio_there_is_only_one_input() {
        let a = ffmpeg_command::build(&plan(true, false)).unwrap();
        assert_eq!(a.iter().filter(|x| *x == "-i").count(), 1);
        assert!(!has(&a, "-c:a"));
        // a pipe name alone is not audio, and neither is a codec without a pipe
        let mut p = plan(true, true);
        p.audio = AudioCodec::None;
        assert_eq!(ffmpeg_command::build(&p).unwrap(), a);
        let mut p = plan(true, false);
        p.audio = AudioCodec::Opus;
        assert_eq!(ffmpeg_command::build(&p).unwrap(), a);
        p.audio_pipe_name = Some(String::new());
        assert_eq!(ffmpeg_command::build(&p).unwrap(), a);
    }

    #[test]
    fn mpegts_output_resends_headers_and_has_no_mux_delay() {
        let a = ffmpeg_command::build(&plan(true, true)).unwrap();
        assert_eq!(value(&a, "-mpegts_flags"), "+resend_headers");
        assert_eq!(value(&a, "-muxdelay"), "0");
        assert_eq!(value(&a, "-muxpreload"), "0");
        assert_eq!(last_value(&a, "-f"), "mpegts");
        assert_eq!(a.last().unwrap(), "pipe:1"); // stdout, into the P2P fan-out
    }

    #[test]
    fn progress_is_reported_on_stderr_because_stdout_carries_the_stream() {
        let a = ffmpeg_command::build(&plan(true, true)).unwrap();
        assert_eq!(value(&a, "-progress"), "pipe:2");
    }

    #[test]
    fn hdr_encoding_requires_p010_input() {
        let mut p = plan(true, true);
        p.pixel_format = CaptureFormat::Nv12;
        assert_eq!(
            ffmpeg_command::build(&p),
            Err(FfmpegPlanError::HdrFormatMismatch)
        );
        let mut p = plan(false, true);
        p.pixel_format = CaptureFormat::P010;
        let error = ffmpeg_command::build(&p).unwrap_err();
        assert_eq!(
            error.to_string(),
            "HDR encoding needs p010 input (and SDR needs nv12)"
        );
    }

    #[test]
    fn the_plan_follows_the_frames_the_engine_announced() {
        let layout = |format, width, height| FrameLayout {
            format,
            width,
            height,
        };
        // Planned as HDR, but the engine fell back to SDR (a Mac that refuses HDR capture): the same command
        // as a plan made for SDR in the first place.
        let fell_back = plan(true, true).for_frames(layout(CaptureFormat::Nv12, 2560, 1440));
        assert_eq!(fell_back, plan(false, true));
        let a = ffmpeg_command::build(&fell_back).unwrap();
        assert_eq!(value(&a, "-pix_fmt"), "nv12");
        assert_eq!(value(&a, "-color_trc"), "bt709");

        // and the other way round, with the size the engine really delivers
        let hdr = plan(false, true).for_frames(layout(CaptureFormat::P010, 3840, 2160));
        assert!(hdr.encoder.hdr);
        let a = ffmpeg_command::build(&hdr).unwrap();
        assert_eq!(value(&a, "-pix_fmt"), "p010le");
        assert_eq!(value(&a, "-video_size"), "3840x2160");
        assert_eq!(value(&a, "-color_trc"), "smpte2084");
        // everything else stays as planned
        assert_eq!(
            hdr,
            FfmpegPlan {
                pixel_format: CaptureFormat::P010,
                width: 3840,
                height: 2160,
                encoder: EncoderSettings {
                    hdr: true,
                    ..plan(false, true).encoder
                },
                ..plan(false, true)
            }
        );
    }

    #[test]
    fn odd_frame_size_is_refused() {
        let mut p = plan(true, true);
        p.width = 1921;
        assert_eq!(
            ffmpeg_command::build(&p),
            Err(FfmpegPlanError::FrameSizeOdd)
        );
    }

    #[test]
    fn every_unusable_plan_is_refused_with_its_own_sentence() {
        let refused = |change: fn(&mut FfmpegPlan)| {
            let mut p = plan(true, true);
            change(&mut p);
            ffmpeg_command::build(&p).unwrap_err().to_string()
        };
        assert_eq!(refused(|p| p.width = 0), "frame size is required");
        assert_eq!(refused(|p| p.height = 0), "frame size is required");
        assert_eq!(refused(|p| p.height = 1081), "frame size must be even");
        assert_eq!(refused(|p| p.fps = 0), "fps must be positive");
        assert_eq!(refused(|p| p.output_url.clear()), "output url is required");
        assert_eq!(
            refused(|p| p.pixel_format = CaptureFormat::Auto),
            "pixel format must be resolved"
        );
        assert_eq!(
            FfmpegPlanError::No10Bit("HEVC (Some GPU)").to_string(),
            "HEVC (Some GPU) has no 10-bit mode"
        );
    }

    #[test]
    fn probe_encodes_a_synthetic_second_to_null() {
        let a = ffmpeg_command::build_probe(&EncoderSettings {
            encoder: VideoEncoder::HevcNvenc,
            hdr: true,
            ..EncoderSettings::default()
        });
        assert!(has(&a, "lavfi"));
        assert!(a.iter().any(|x| x.contains("duration=1"))); // SVT-AV1 emits nothing for a shorter clip
        assert_eq!(value(&a, "-i"), "testsrc2=size=640x480:rate=30:duration=1");
        assert_eq!(last_value(&a, "-f"), "null");
        assert_eq!(a.last().unwrap(), "-");
        assert_eq!(value(&a, "-c:v"), "hevc_nvenc");
        assert_eq!(value(&a, "-pix_fmt"), "p010le");
        // the probe is small and short whatever the stream settings are
        assert_eq!(value(&a, "-b:v"), "2000k");
        assert_eq!(value(&a, "-g"), "30"); // one second of 30 fps: the default keyframe interval
        let sized = ffmpeg_command::build_probe_sized(&EncoderSettings::default(), 320, 240);
        assert_eq!(
            value(&sized, "-i"),
            "testsrc2=size=320x240:rate=30:duration=1"
        );
    }

    #[test]
    fn cpu_encoders_get_a_planar_pixel_format() {
        // The capture engine only produces p010/nv12; libx265 accepts neither, so the profile must name the
        // planar format or ffmpeg may negotiate 8-bit and silently drop HDR precision.
        assert_eq!(
            encoder_profiles::pixel_format(VideoEncoder::HevcNvenc, true),
            "p010le"
        );
        assert_eq!(
            encoder_profiles::pixel_format(VideoEncoder::HevcNvenc, false),
            "nv12"
        );
        assert_eq!(
            encoder_profiles::pixel_format(VideoEncoder::LibX265, true),
            "yuv420p10le"
        );
        assert_eq!(
            encoder_profiles::pixel_format(VideoEncoder::LibX265, false),
            "yuv420p"
        );
    }

    #[test]
    fn x265_writes_the_colour_description_into_x265_params() {
        // ffmpeg's libx265 wrapper drops -color_primaries / -color_trc; without these the viewer gets an
        // untagged stream and shows HDR as washed-out SDR.
        let mut s = EncoderSettings {
            encoder: VideoEncoder::LibX265,
            hdr: true,
            bitrate_kbps: 20000,
            fps: 60,
            ..Default::default()
        };
        let a = encoder_profiles::build_video_args(&s);
        let p = value(&a, "-x265-params");
        assert!(p.contains("colorprim=bt2020"));
        assert!(p.contains("transfer=smpte2084"));
        assert!(p.contains("colormatrix=bt2020nc"));

        s.hdr = false;
        let a = encoder_profiles::build_video_args(&s);
        let p = value(&a, "-x265-params");
        assert!(p.contains("colorprim=bt709"));
        assert!(p.contains("transfer=bt709"));
    }

    #[test]
    fn pipe_path_is_the_capture_engines() {
        assert_eq!(
            ffmpeg_command::pipe_path("foo"),
            hdrcast_capture::pipe_path("foo")
        );
        #[cfg(windows)]
        assert_eq!(ffmpeg_command::pipe_path("foo"), r"\\.\pipe\foo");
    }

    /// The whole Windows command line of a 0.2.4 stream, argument for argument.
    #[cfg(windows)]
    #[test]
    fn the_whole_command_line_is_the_one_of_0_2_4() {
        let mut p = plan(true, true);
        p.encoder.preset = QualityPreset::Balanced;
        // 0.2.4's defaults (0.3.2 has a 1 s keyframe interval and a 0.25 s buffer)
        (p.encoder.gop_seconds, p.encoder.buffer_seconds) = (2.0, 2.0);
        let expected = r"-hide_banner -loglevel warning -nostdin -stats_period 1 -progress pipe:2 \
            -thread_queue_size 1024 -f rawvideo -pix_fmt p010le -video_size 2560x1440 -framerate 60 \
            -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv -i \\.\pipe\hdrcast-video-1234 \
            -thread_queue_size 1024 -f f32le -ar 48000 -ac 2 -i \\.\pipe\hdrcast-audio-1234 \
            -c:v hevc_nvenc -pix_fmt p010le -preset p5 -tune hq -rc cbr -multipass qres -rc-lookahead 20 -bf 2 -b_ref_mode middle \
            -spatial-aq 1 -aq-strength 8 -temporal-aq 0 -profile:v main10 -tier high -b:v 40000k -maxrate 40000k -bufsize 80000k \
            -g 120 -forced-idr 1 -no-scenecut 0 \
            -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv -chroma_sample_location left \
            -fps_mode cfr -c:a aac -b:a 192k -ac 2 -muxdelay 0 -muxpreload 0 -f mpegts -mpegts_flags +resend_headers pipe:1";
        let expected: Vec<&str> = expected
            .split_whitespace()
            .filter(|part| *part != "\\")
            .collect();
        assert_eq!(ffmpeg_command::build(&p).unwrap(), expected);

        p.audio = AudioCodec::Opus;
        p.audio_bitrate_kbps = 128;
        p.progress = false;
        p.log_level = "error".to_string();
        let a = ffmpeg_command::build(&p).unwrap();
        assert_eq!(a[..4], ["-hide_banner", "-loglevel", "error", "-nostdin"]);
        assert_eq!(a[4], "-thread_queue_size");
        assert_eq!((value(&a, "-c:a"), value(&a, "-b:a")), ("libopus", "128k"));

        let line = ffmpeg_command::command_line(
            std::path::Path::new(r"C:\Program Files\HdrCast\tools\ffmpeg.exe"),
            &p,
        )
        .unwrap();
        assert!(line.starts_with(r#""C:\Program Files\HdrCast\tools\ffmpeg.exe" -hide_banner -loglevel error -nostdin -thread_queue_size 1024"#), "{line}");
        assert!(
            line.ends_with("-f mpegts -mpegts_flags +resend_headers pipe:1"),
            "{line}"
        );
    }
}

mod ffmpeg_progress {
    use super::*;

    #[test]
    fn parses_a_complete_block() {
        let mut parser = FfmpegProgressParser::new();
        let lines = [
            "frame=1234",
            "fps=59.94",
            "stream_0_0_q=23.0",
            "bitrate=39876.5kbits/s",
            "total_size=12345678",
            "out_time_us=20566666",
            "dup_frames=3",
            "drop_frames=1",
            "speed=1.001x",
            "progress=continue",
        ];
        let mut block = None;
        for line in lines {
            block = parser.feed(line).or(block);
        }

        let block = block.expect("the block was closed");
        assert_eq!(block.frame, 1234);
        assert!((block.fps - 59.94).abs() < 0.005);
        assert!((block.bitrate_kbps - 39876.5).abs() < 0.05);
        assert_eq!(block.total_size_bytes, 12345678);
        assert_eq!(block.dup_frames, 3);
        assert_eq!(block.drop_frames, 1);
        assert!((block.speed - 1.001).abs() < 0.0005);
        assert!(!block.ended);
        assert!((block.out_time().as_secs_f64() - 20.566666).abs() < 0.0005);
    }

    #[test]
    fn handles_na_values() {
        let mut parser = FfmpegProgressParser::new();
        parser.feed("bitrate=N/A");
        parser.feed("speed=N/A");
        let b = parser
            .feed("progress=continue")
            .expect("the block was closed");
        assert!(b.bitrate_kbps < 0.0);
        assert!(b.speed < 0.0);
    }

    #[test]
    fn end_block_is_flagged() {
        let mut parser = FfmpegProgressParser::new();
        parser.feed("frame=10");
        assert!(parser.feed("progress=end").unwrap().ended);
    }

    #[test]
    fn ignores_unrelated_lines() {
        let mut parser = FfmpegProgressParser::new();
        assert_eq!(parser.feed("this is not progress output"), None);
        assert_eq!(parser.feed(""), None);
        assert_eq!(parser.feed("=nothing"), None);
        assert_eq!(parser.feed("unknown_key=1"), None);
    }
}

mod sender_settings {
    use super::*;

    #[test]
    fn defaults_are_a_working_group_configuration() {
        let s = SenderSettings::defaults_for(Platform::Windows);
        assert_eq!(s.encoder, VideoEncoder::HevcNvenc);
        assert!(s.audio);
        assert_eq!(s.max_viewers, 4);
        assert!(s.validate_for(Platform::Windows).is_empty());
        // on the machine the tests run on, too
        assert!(SenderSettings::default().validate().is_empty());
        assert_eq!(
            SenderSettings::default(),
            SenderSettings::defaults_for(Platform::CURRENT)
        );
        assert_eq!(
            SenderSettings::default().normalized(),
            SenderSettings::default()
        );
    }

    #[test]
    fn an_old_settings_file_with_relay_fields_and_an_av1_encoder_still_loads() {
        // "encoder":1 was av1_nvenc; "mode"/"host"/... belonged to the removed relay and direct modes.
        let json = r#"{"encoder":1,"bitrateMbps":50,"mode":0,"host":"192.168.1.201","port":8890,"path":"screen","user":"sender","password":"pw","autoRestart":true}"#;
        let mut s: SenderSettings = serde_json::from_str(json).expect("an old file still reads");
        s.normalize();
        assert_eq!(
            s.encoder,
            encoder_profiles::default_encoder(Platform::CURRENT)
        );
        assert_eq!(s.bitrate_mbps, 50);
        assert!(s.validate().is_empty());

        // 3 and 6 were AV1 too; on Windows 6 must not suddenly mean an encoder Windows does not have
        for number in [1, 3, 6, 7, -1] {
            let mut s: SenderSettings =
                serde_json::from_str(&format!(r#"{{"encoder":{number},"bitrateMbps":50}}"#))
                    .unwrap();
            s.normalize_for(Platform::Windows);
            assert_eq!(s.encoder, VideoEncoder::HevcNvenc, "encoder {number}");
            assert_eq!(s.bitrate_mbps, 50);
        }
    }

    #[test]
    fn the_app_to_leave_out_is_cleaned_up_and_reaches_the_engine() {
        let s = SenderSettings {
            audio_exclude_app: "  Discord ".to_string(),
            ..SenderSettings::default()
        }
        .normalized();
        assert_eq!(s.audio_exclude_app, "Discord.exe");
        let a = s.to_capture_args(
            &OutputInfo::default(),
            1920,
            1080,
            CaptureFormat::P010,
            "v",
            "a",
            1,
        );
        assert_eq!(a.audio_exclude, "Discord.exe");
        assert_eq!(SenderSettings::default().normalized().audio_exclude_app, "");
        let older: SenderSettings = serde_json::from_str(r#"{"audio":true}"#).unwrap(); // older files
        assert_eq!(older.normalized().audio_exclude_app, "");
    }

    #[test]
    fn scale_keeps_aspect_ratio_and_even_numbers() {
        let mut s = SenderSettings {
            scale: ScaleMode::H1080,
            ..SenderSettings::default()
        };
        assert_eq!(s.resolve_size(3840, 2160), (1920, 1080));

        s.scale = ScaleMode::H1440;
        assert_eq!(s.resolve_size(3440, 1440), (3440, 1440)); // already 1440 tall: no upscale, no change

        s.scale = ScaleMode::H720;
        let (w, h) = s.resolve_size(2560, 1080);
        assert_eq!(w % 2, 0);
        assert_eq!(h, 720);
        assert_eq!(w, 1706); // 1706.67 rounds to 1707, made even downwards

        s.scale = ScaleMode::H1080;
        assert_eq!(s.resolve_size(1280, 720), (1280, 720)); // never upscales
        assert_eq!(s.resolve_size(5120, 1440), (3840, 1080));
        assert_eq!(s.scale_height(), 1080);
    }

    #[test]
    fn native_scale_only_rounds_to_even() {
        let s = SenderSettings {
            scale: ScaleMode::Native,
            ..SenderSettings::default()
        };
        assert_eq!(s.resolve_size(1366, 769), (1366, 768));
        assert_eq!(s.resolve_size(0, -5), (2, 2)); // nonsense in, the smallest picture out
        assert_eq!(s.scale_height(), 0);
    }

    #[test]
    fn format_follows_the_monitor_unless_sdr_is_forced() {
        let mut s = SenderSettings {
            hdr_mode: HdrMode::Auto,
            ..SenderSettings::default()
        };
        assert_eq!(s.resolve_format(true), CaptureFormat::P010);
        assert_eq!(s.resolve_format(false), CaptureFormat::Nv12);
        s.hdr_mode = HdrMode::ForceSdr;
        assert_eq!(s.resolve_format(true), CaptureFormat::Nv12);
    }

    #[test]
    fn normalize_clamps_hand_edited_values() {
        let mut s = SenderSettings {
            bitrate_mbps: 9999,
            fps: 0,
            gop_seconds: 99.0,
            max_viewers: 99,
            ..SenderSettings::default()
        };
        s.normalize();
        assert_eq!(s.bitrate_mbps, 500);
        assert_eq!(s.fps, 60);
        assert_eq!(s.gop_seconds, 10.0);
        assert_eq!(s.max_viewers, 16);

        let mut low = SenderSettings {
            output_index: -3,
            fps: 481,
            sdr_white: 0.5,
            bitrate_mbps: -4,
            gop_seconds: 0.0,
            buffer_seconds: 0.1,
            audio_bitrate_kbps: 8,
            audio_device: "  ".to_string(),
            max_viewers: 0,
            upload_mbps: -1,
            ..SenderSettings::default()
        };
        assert_eq!(
            low.validate(),
            ["Bitrate must be 1..500 Mbps.", "Frame rate must be 1..480."]
        );
        low.normalize();
        assert_eq!(
            (low.output_index, low.fps, low.sdr_white, low.bitrate_mbps),
            (0, 60, 80.0, 1)
        );
        assert_eq!(
            (low.gop_seconds, low.buffer_seconds, low.audio_bitrate_kbps),
            (0.25, 0.25, 32) // the buffer may be a quarter of a second since 0.3.2 (0.5 s before)
        );
        assert_eq!(
            (low.audio_device.as_str(), low.max_viewers, low.upload_mbps),
            ("default", 1, 0)
        );
        assert!(low.validate().is_empty());

        let mut high = SenderSettings {
            sdr_white: 20000.0,
            buffer_seconds: 60.0,
            audio_bitrate_kbps: 9000,
            upload_mbps: 2_000_000,
            ..SenderSettings::default()
        };
        high.normalize();
        assert_eq!(
            (
                high.sdr_white,
                high.buffer_seconds,
                high.audio_bitrate_kbps,
                high.upload_mbps
            ),
            (80.0, 10.0, 512, 100_000)
        );
    }

    #[test]
    fn capture_args_round_trip_through_the_command_line() {
        let s = SenderSettings {
            fps: 60,
            cursor: false,
            audio: true,
            sdr_white: 120.0,
            ..SenderSettings::default()
        };
        let output = OutputInfo {
            index: 2,
            device_name: r"\\.\DISPLAY3".to_string(),
            ..OutputInfo::default()
        };
        let args = s.to_capture_args(&output, 2560, 1440, CaptureFormat::P010, "vid", "aud", 4321);
        let parsed =
            CaptureArgs::parse(&args.to_argv()).expect("the engine reads back what the app wrote");

        assert_eq!(parsed.output_index, 2);
        assert_eq!(parsed.fps, 60);
        assert_eq!(parsed.format, CaptureFormat::P010);
        assert_eq!(parsed.width, 2560);
        assert_eq!(parsed.height, 1440);
        assert!(!parsed.cursor);
        assert_eq!(parsed.video_pipe, "vid");
        assert_eq!(parsed.audio_pipe, "aud");
        assert_eq!(parsed.parent_pid, 4321);
        assert_eq!(parsed.sdr_white, 120.0);
        assert_eq!(parsed, args);
    }

    #[test]
    fn the_encoder_gets_the_stream_settings() {
        let s = SenderSettings {
            encoder: VideoEncoder::LibX265,
            quality: QualityPreset::Quality,
            bitrate_mbps: 25,
            fps: 30,
            gop_seconds: 1.0,
            buffer_seconds: 3.0,
            b_frames: false,
            lookahead: false,
            ..SenderSettings::default()
        };
        assert_eq!(
            s.to_encoder_settings(true, 1015),
            EncoderSettings {
                encoder: VideoEncoder::LibX265,
                preset: QualityPreset::Quality,
                hdr: true,
                bitrate_kbps: 25000,
                fps: 30,
                gop_seconds: 1.0,
                buffer_seconds: 3.0,
                b_frames: false,
                lookahead: false,
                mastering_max_nits: 1015,
            }
        );
        assert_eq!(s.bitrate_bps(), 25_000_000);
        assert_eq!(s.to_string(), "libx265 25 Mbps 30fps, up to 4 viewers");
    }
}
