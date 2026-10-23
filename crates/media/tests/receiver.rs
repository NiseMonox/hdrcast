//! Port of tests/ReceiverTests.cs (without MiniJsonTests: JSON lives in hdrcast-core), plus what the Rust
//! version added (macOS, the remote-control properties, arbitrary IPC commands).

use std::path::Path;

use hdrcast_core::common::srt_url;
use hdrcast_core::{SrtRole, SrtTopology};
use hdrcast_media::receiver::mpv_command::{self, MpvLaunchPlan, MpvPlanError};
use hdrcast_media::receiver::mpv_ipc::{
    self, observed, MpvIpcMessage, MpvIpcMessageKind, MpvStatusModel, MpvVideoParams,
};
use hdrcast_media::receiver::mpv_log::{self, MpvExitReason};
use hdrcast_media::receiver::{DisplayMode, HwDecMode, ReceiverFrontend, ReceiverSettings};
use hdrcast_media::Platform;
use serde_json::{json, Value};

const LOCAL_PORT: u16 = 40123; // the viewer link's end on 127.0.0.1
const PASSPHRASE: &str = "0123456789abcdef";

fn settings(display: DisplayMode) -> ReceiverSettings {
    ReceiverSettings {
        latency_ms: 1500,
        expected_bitrate_mbps: 60,
        display_mode: display,
        ..ReceiverSettings::default()
    }
    .normalized()
}

fn endpoint(pid: u32) -> String {
    format!(
        "{}{}",
        mpv_command::PIPE_PREFIX,
        mpv_command::pipe_name(pid)
    )
}

fn build_on(
    platform: Platform,
    s: &ReceiverSettings,
    pid: u32,
    mpv: &str,
    ffmpeg: Option<&str>,
) -> MpvLaunchPlan {
    mpv_command::build(
        s,
        LOCAL_PORT,
        PASSPHRASE,
        &endpoint(pid),
        Path::new(mpv),
        ffmpeg.map(Path::new),
        platform,
    )
    .expect("valid settings give a plan")
}

fn build(s: &ReceiverSettings, pid: u32, mpv: &str) -> MpvLaunchPlan {
    build_on(Platform::Windows, s, pid, mpv, None)
}

/// The value of `--name=value`.
fn flag<'a>(args: &'a [String], name: &str) -> &'a str {
    let prefix = format!("--{name}=");
    args.iter()
        .find_map(|a| a.strip_prefix(prefix.as_str()))
        .unwrap_or("")
}

fn has(args: &[String], name: &str) -> bool {
    let prefix = format!("--{name}=");
    args.iter()
        .any(|a| *a == format!("--{name}") || a.starts_with(&prefix))
}

mod mpv_command_line {
    use super::*;

    #[test]
    fn hdr_passthrough_needs_gpu_next_and_the_colorspace_hint() {
        let plan = build(
            &settings(DisplayMode::HdrPassthrough),
            1234,
            r"C:\mpv\mpv.exe",
        );
        assert_eq!(flag(&plan.mpv_args, "vo"), "gpu-next");
        assert_eq!(flag(&plan.mpv_args, "gpu-api"), "d3d11");
        assert_eq!(flag(&plan.mpv_args, "target-colorspace-hint"), "yes");
    }

    #[test]
    fn sdr_mode_turns_the_hint_off_and_tone_maps() {
        let plan = build(&settings(DisplayMode::SdrToneMap), 1234, "mpv");
        assert_eq!(flag(&plan.mpv_args, "target-colorspace-hint"), "no");
        assert_eq!(
            flag(&plan.mpv_args, "tone-mapping"),
            ReceiverSettings::DEFAULT_TONE_MAPPING
        );
        // only then: tone mapping is not named when mpv passes HDR through
        assert!(!has(
            &build(&settings(DisplayMode::Auto), 1, "mpv").mpv_args,
            "tone-mapping"
        ));
        let mut s = settings(DisplayMode::SdrToneMap);
        s.tone_mapping = " hable ".to_string();
        assert_eq!(flag(&build(&s, 1, "mpv").mpv_args, "tone-mapping"), "hable");
        s.tone_mapping = " ".to_string(); // not normalised: still never an empty value
        assert_eq!(
            flag(&build(&s, 1, "mpv").mpv_args, "tone-mapping"),
            "bt.2390"
        );
    }

    #[test]
    fn auto_mode_lets_mpv_decide() {
        let plan = build(&settings(DisplayMode::Auto), 1234, "mpv");
        assert_eq!(flag(&plan.mpv_args, "target-colorspace-hint"), "auto");
    }

    #[test]
    fn url_is_the_last_argument_and_calls_the_local_end_of_the_path() {
        let plan = build(&settings(DisplayMode::Auto), 1234, "mpv");
        let url = plan.mpv_args.last().unwrap();
        assert!(
            url.starts_with(&format!("srt://127.0.0.1:{LOCAL_PORT}?mode=caller")),
            "{url}"
        );
        assert_eq!(*url, plan.url);
        let p = srt_url::parse(url).unwrap();
        assert_eq!(p.get("streamid"), None);
        assert_eq!(p.get("passphrase"), Some(PASSPHRASE));
        assert_eq!(p.get("pbkeylen"), Some("32"));
        assert_eq!(p.get("latency"), Some("1500000"));
        // Reading a 60 Mbps stream with 1.5 s of latency needs a bigger receive buffer than libsrt's default.
        assert!(p.has("rcvbuf"));
        assert!(p.has("ffs"));
        assert!(p.has("recv_buffer_size"));
        // the whole URL, as 0.2.4 built it
        assert_eq!(
            url,
            "srt://127.0.0.1:40123?mode=caller&passphrase=0123456789abcdef&pbkeylen=32&pkt_size=1316&transtype=live\
             &latency=1500000&rcvbuf=37683200&ffs=25600&recv_buffer_size=8000000&connect_timeout=5000"
        );
    }

    #[test]
    fn ipc_pipe_is_per_process() {
        assert_eq!(mpv_command::pipe_name(4321), "hdrcast-mpv-4321");
        let plan = build(&settings(DisplayMode::Auto), 4321, "mpv");
        assert_eq!(plan.ipc_endpoint, r"\\.\pipe\hdrcast-mpv-4321");
        assert_eq!(
            flag(&plan.mpv_args, "input-ipc-server"),
            r"\\.\pipe\hdrcast-mpv-4321"
        );
        // elsewhere the app passes a socket path, and it reaches mpv untouched
        let s = settings(DisplayMode::Auto);
        let socket = "/var/folders/x/T/hdrcast-mpv-4321.sock";
        let mac = mpv_command::build(
            &s,
            LOCAL_PORT,
            PASSPHRASE,
            socket,
            Path::new("mpv"),
            None,
            Platform::MacOs,
        )
        .unwrap();
        assert_eq!(mac.ipc_endpoint, socket);
        assert_eq!(flag(&mac.mpv_args, "input-ipc-server"), socket);
        let here = mpv_command::default_ipc_endpoint(4321);
        assert!(here.contains("hdrcast-mpv-4321"), "{here}");
        #[cfg(windows)]
        assert_eq!(here, r"\\.\pipe\hdrcast-mpv-4321");
        #[cfg(not(windows))]
        assert!(
            here.ends_with(".sock") && Path::new(&here).is_absolute(),
            "{here}"
        );
    }

    #[test]
    fn cache_settings_follow_the_buffer_seconds() {
        let mut s = settings(DisplayMode::Auto);
        s.buffer_seconds = 2.5;
        let plan = build(&s, 1, "mpv");
        assert!(has(&plan.mpv_args, "cache"));
        assert!(has(&plan.mpv_args, "cache-pause-initial"));
        assert_eq!(flag(&plan.mpv_args, "cache-pause-wait"), "2.5");
        assert_eq!(flag(&plan.mpv_args, "demuxer-readahead-secs"), "5");
        assert_eq!(flag(&plan.mpv_args, "cache-secs"), "10");

        // a small buffer still reads ahead a second and keeps a generous cache
        s.buffer_seconds = 0.3;
        let small = build(&s, 1, "mpv");
        assert_eq!(flag(&small.mpv_args, "cache-pause-wait"), "0.3");
        assert_eq!(flag(&small.mpv_args, "demuxer-readahead-secs"), "1");
        assert_eq!(flag(&small.mpv_args, "cache-secs"), "8");

        // settings that were never normalised: a useless buffer becomes the default one
        s.buffer_seconds = 0.0;
        assert_eq!(
            flag(&build(&s, 1, "mpv").mpv_args, "cache-pause-wait"),
            "1.5"
        );
        // numbers are written like the C# "0.###" did
        s.buffer_seconds = 1.23456;
        let odd = build(&s, 1, "mpv");
        assert_eq!(flag(&odd.mpv_args, "cache-pause-wait"), "1.235");
        assert_eq!(flag(&odd.mpv_args, "demuxer-readahead-secs"), "2.469");
        assert_eq!(flag(&odd.mpv_args, "cache-secs"), "8");
    }

    #[test]
    fn fullscreen_targets_the_chosen_screen() {
        let mut s = settings(DisplayMode::Auto);
        s.fullscreen = true;
        s.fullscreen_screen = 2;
        let plan = build(&s, 1, "mpv");
        assert!(has(&plan.mpv_args, "fs"));
        assert_eq!(flag(&plan.mpv_args, "fs-screen"), "2");
        s.fullscreen = false;
        let windowed = build(&s, 1, "mpv");
        assert!(!has(&windowed.mpv_args, "fs") && !has(&windowed.mpv_args, "fs-screen"));
    }

    #[test]
    fn exclusive_fullscreen_is_never_used() {
        // --d3d11-exclusive-fs breaks HDR passthrough on some drivers; borderless --fs is the supported path.
        let plan = build(&settings(DisplayMode::HdrPassthrough), 1, "mpv");
        assert!(!has(&plan.mpv_args, "d3d11-exclusive-fs"));
    }

    #[test]
    fn the_ffmpeg_pipe_hands_the_raw_stream_to_the_guard_and_mpv_stdin() {
        let mut s = settings(DisplayMode::Auto);
        s.frontend = ReceiverFrontend::FfmpegPipe;
        let plan = build_on(Platform::Windows, &s, 1, "mpv", Some(r"C:\ff\ffmpeg.exe"));
        assert!(plan.uses_ffmpeg_pipe());
        let ffmpeg_args = plan.ffmpeg_args.as_ref().unwrap();
        assert_eq!(plan.mpv_args.last().unwrap(), "-"); // mpv reads stdin
        assert!(plan.to_command_line().contains(" | "));
        // -f data in and out: the SRT payload byte for byte, continuity counters and all (docs/design.md §3.2)
        assert_eq!(
            ffmpeg_args.join(" "),
            format!("-hide_banner -loglevel warning -nostdin -f data -i {} -map 0 -c copy -f data -flush_packets 1 pipe:1", plan.url)
        );
        // stdin is not a terminal and not a seekable file: mpv is told both
        assert_eq!(flag(&plan.mpv_args, "demuxer-lavf-format"), "mpegts");
        assert_eq!(flag(&plan.mpv_args, "input-terminal"), "no");
        let direct = build(&settings(DisplayMode::Auto), 1, "mpv");
        assert!(!direct.uses_ffmpeg_pipe());
        assert!(
            !has(&direct.mpv_args, "demuxer-lavf-format")
                && !has(&direct.mpv_args, "input-terminal")
        );
        assert_eq!(direct.ffmpeg_command_line(), None);
        assert_eq!(direct.to_command_line(), direct.mpv_command_line());
    }

    #[test]
    fn automatic_uses_the_pipe_whenever_ffmpeg_is_there() {
        let auto = settings(DisplayMode::Auto);
        assert_eq!(auto.frontend, ReceiverFrontend::Auto, "the default");
        assert!(build_on(Platform::Windows, &auto, 1, "mpv", Some("ffmpeg")).uses_ffmpeg_pipe());
        assert!(!build_on(Platform::Windows, &auto, 1, "mpv", None).uses_ffmpeg_pipe(), "no ffmpeg: mpv direct");
        assert!(!build_on(Platform::Windows, &auto, 1, "mpv", Some("")).uses_ffmpeg_pipe());
        let direct = ReceiverSettings { frontend: ReceiverFrontend::MpvDirect, ..auto };
        assert!(!build_on(Platform::Windows, &direct, 1, "mpv", Some("ffmpeg")).uses_ffmpeg_pipe(), "asked for");
    }

    #[test]
    fn a_plan_needs_its_programs_and_a_usable_url() {
        let s = settings(DisplayMode::Auto);
        let build = |s: &ReceiverSettings, mpv: &str, ffmpeg: Option<&str>, passphrase: &str| {
            mpv_command::build(
                s,
                LOCAL_PORT,
                passphrase,
                "ipc",
                Path::new(mpv),
                ffmpeg.map(Path::new),
                Platform::Windows,
            )
        };
        assert_eq!(
            build(&s, "", None, PASSPHRASE),
            Err(MpvPlanError::MpvPathRequired)
        );
        assert_eq!(
            build(&s, "", None, PASSPHRASE).unwrap_err().to_string(),
            "mpv path required"
        );
        let mut pipe = s.clone();
        pipe.frontend = ReceiverFrontend::FfmpegPipe;
        assert_eq!(
            build(&pipe, "mpv", None, PASSPHRASE),
            Err(MpvPlanError::FfmpegPathRequired)
        );
        assert_eq!(
            build(&pipe, "mpv", Some(""), PASSPHRASE)
                .unwrap_err()
                .to_string(),
            "ffmpeg path required for the pipe frontend"
        );
        // libsrt refuses a passphrase shorter than 10 characters; so do we, before mpv is started
        assert_eq!(
            build(&s, "mpv", None, "short").unwrap_err().to_string(),
            "passphrase must be 10-79 characters"
        );
        // no passphrase = no encryption options at all
        let open = build(&s, "mpv", None, "").unwrap();
        assert!(
            !open.url.contains("passphrase") && !open.url.contains("pbkeylen"),
            "{}",
            open.url
        );
    }

    /// The whole Windows argument list of 0.2.4, argument for argument.
    #[test]
    fn the_windows_argument_list_is_the_one_of_0_2_4() {
        let plan = build(
            &settings(DisplayMode::Auto),
            4321,
            r"C:\Program Files\HdrCast\tools\mpv.exe",
        );
        let expected = [
            "--vo=gpu-next",
            "--gpu-api=d3d11",
            "--hwdec=d3d11va",
            "--target-colorspace-hint=auto",
            "--hdr-compute-peak=auto",
            "--cache=yes",
            "--demuxer-readahead-secs=3",
            "--cache-secs=8",
            "--cache-pause-initial=yes",
            "--cache-pause-wait=1.5",
            "--demuxer-max-bytes=256MiB",
            "--video-sync=audio",
            "--interpolation=no",
            "--framedrop=vo",
            "--demuxer-lavf-probesize=5000000",
            "--demuxer-lavf-analyzeduration=2",
            r"--input-ipc-server=\\.\pipe\hdrcast-mpv-4321",
            "--keep-open=no",
            "--force-window=yes",
            "--title=HdrCast",
            "--msg-level=all=info",
            "--term-status-msg=",
            plan.url.as_str(),
        ];
        assert_eq!(plan.mpv_args, expected);
        assert!(plan.mpv_command_line().starts_with(
            r#""C:\Program Files\HdrCast\tools\mpv.exe" --vo=gpu-next --gpu-api=d3d11"#
        ));

        let s = ReceiverSettings {
            display_mode: DisplayMode::SdrToneMap,
            tone_mapping: "spline".to_string(),
            target_peak_nits: 400,
            hw_dec: HwDecMode::D3d11vaCopy,
            fullscreen: true,
            fullscreen_screen: 1,
            buffer_seconds: 2.0,
            frontend: ReceiverFrontend::FfmpegPipe,
            ..settings(DisplayMode::Auto)
        };
        let plan = build_on(Platform::Windows, &s, 7, "mpv", Some("ffmpeg"));
        let expected = [
            "--vo=gpu-next",
            "--gpu-api=d3d11",
            "--hwdec=d3d11va-copy",
            "--target-colorspace-hint=no",
            "--tone-mapping=spline",
            "--target-peak=400",
            "--hdr-compute-peak=auto",
            "--cache=yes",
            "--demuxer-readahead-secs=4",
            "--cache-secs=8",
            "--cache-pause-initial=yes",
            "--cache-pause-wait=2",
            "--demuxer-max-bytes=256MiB",
            "--video-sync=audio",
            "--interpolation=no",
            "--framedrop=vo",
            "--demuxer-lavf-probesize=5000000",
            "--demuxer-lavf-analyzeduration=2",
            "--demuxer-lavf-format=mpegts",
            r"--input-ipc-server=\\.\pipe\hdrcast-mpv-7",
            "--keep-open=no",
            "--force-window=yes",
            "--title=HdrCast",
            "--msg-level=all=info",
            "--term-status-msg=",
            "--input-terminal=no",
            "--fs",
            "--fs-screen=1",
            "-",
        ];
        assert_eq!(plan.mpv_args, expected);
    }
}

mod macos {
    use super::*;

    /// The macOS list: the Windows one without `--gpu-api=d3d11`, with VideoToolbox as the hardware decoder.
    #[test]
    fn the_mac_argument_list_differs_only_in_the_gpu_api_and_the_decoder() {
        let s = ReceiverSettings {
            display_mode: DisplayMode::HdrPassthrough,
            ..settings(DisplayMode::Auto)
        };
        let socket = "/tmp/hdrcast-mpv-9.sock";
        let mac = mpv_command::build(
            &s,
            LOCAL_PORT,
            PASSPHRASE,
            socket,
            Path::new("/opt/homebrew/bin/mpv"),
            None,
            Platform::MacOs,
        )
        .unwrap();
        let expected = [
            "--vo=gpu-next",
            "--hwdec=videotoolbox",
            "--target-colorspace-hint=yes",
            "--hdr-compute-peak=auto",
            "--cache=yes",
            "--demuxer-readahead-secs=3",
            "--cache-secs=8",
            "--cache-pause-initial=yes",
            "--cache-pause-wait=1.5",
            "--demuxer-max-bytes=256MiB",
            "--video-sync=audio",
            "--interpolation=no",
            "--framedrop=vo",
            "--demuxer-lavf-probesize=5000000",
            "--demuxer-lavf-analyzeduration=2",
            "--input-ipc-server=/tmp/hdrcast-mpv-9.sock",
            "--keep-open=no",
            "--force-window=yes",
            "--title=HdrCast",
            "--msg-level=all=info",
            "--term-status-msg=",
            mac.url.as_str(),
        ];
        assert_eq!(mac.mpv_args, expected);

        // the same settings on Windows: one argument more, one different, the rest identical
        let windows = mpv_command::build(
            &s,
            LOCAL_PORT,
            PASSPHRASE,
            socket,
            Path::new("mpv"),
            None,
            Platform::Windows,
        )
        .unwrap();
        let without: Vec<&String> = windows
            .mpv_args
            .iter()
            .filter(|a| *a != "--gpu-api=d3d11")
            .collect();
        assert_eq!(without.len(), mac.mpv_args.len());
        let different: Vec<_> = without
            .iter()
            .zip(&mac.mpv_args)
            .filter(|(w, m)| **w != *m)
            .collect();
        assert_eq!(different.len(), 1, "{different:?}");
        assert_eq!(
            (different[0].0.as_str(), different[0].1.as_str()),
            ("--hwdec=d3d11va", "--hwdec=videotoolbox")
        );
    }

    #[test]
    fn the_hdr_logic_is_the_same_as_on_windows() {
        for (display, hint) in [
            (DisplayMode::Auto, "auto"),
            (DisplayMode::HdrPassthrough, "yes"),
            (DisplayMode::SdrToneMap, "no"),
        ] {
            let s = ReceiverSettings {
                target_peak_nits: 600,
                fullscreen: true,
                fullscreen_screen: 1,
                ..settings(display)
            };
            let plan = build_on(Platform::MacOs, &s, 1, "mpv", None);
            assert_eq!(flag(&plan.mpv_args, "vo"), "gpu-next");
            assert!(!has(&plan.mpv_args, "gpu-api"));
            assert_eq!(flag(&plan.mpv_args, "target-colorspace-hint"), hint);
            assert_eq!(
                has(&plan.mpv_args, "tone-mapping"),
                display == DisplayMode::SdrToneMap
            );
            assert_eq!(flag(&plan.mpv_args, "target-peak"), "600");
            assert_eq!(flag(&plan.mpv_args, "hdr-compute-peak"), "auto");
            assert!(has(&plan.mpv_args, "fs"));
            assert_eq!(flag(&plan.mpv_args, "fs-screen"), "1");
        }
    }

    #[test]
    fn hardware_decoding_is_named_per_platform() {
        use HwDecMode::*;
        let values =
            |platform| [AutoSafe, D3d11va, D3d11vaCopy, No].map(|mode| mode.mpv_value(platform));
        assert_eq!(
            values(Platform::Windows),
            ["auto-safe", "d3d11va", "d3d11va-copy", "no"]
        );
        assert_eq!(
            values(Platform::MacOs),
            ["auto-safe", "videotoolbox", "videotoolbox-copy", "no"]
        );
        assert_eq!(
            values(Platform::Other),
            ["auto-safe", "auto-safe", "auto-copy", "no"]
        );
        let names =
            |platform| [AutoSafe, D3d11va, D3d11vaCopy, No].map(|mode| mode.display_name(platform));
        assert_eq!(
            names(Platform::Windows),
            ["auto-safe", "d3d11va", "d3d11va-copy", "no (software)"]
        ); // the C# combo box
        assert_eq!(
            names(Platform::MacOs),
            [
                "auto-safe",
                "videotoolbox",
                "videotoolbox-copy",
                "no (software)"
            ]
        );
        // the numbers in receiver.json did not change
        assert_eq!(
            [AutoSafe, D3d11va, D3d11vaCopy, No].map(HwDecMode::number),
            [0, 1, 2, 3]
        );
        assert_eq!(
            mpv_command::hwdec_value(D3d11vaCopy, Platform::MacOs),
            "videotoolbox-copy"
        );
        for mode in [AutoSafe, D3d11va, D3d11vaCopy, No] {
            let s = ReceiverSettings {
                hw_dec: mode,
                ..settings(DisplayMode::Auto)
            };
            assert_eq!(
                flag(
                    &build_on(Platform::MacOs, &s, 1, "mpv", None).mpv_args,
                    "hwdec"
                ),
                mode.mpv_value(Platform::MacOs)
            );
            assert_eq!(
                flag(
                    &build_on(Platform::Windows, &s, 1, "mpv", None).mpv_args,
                    "hwdec"
                ),
                mode.mpv_value(Platform::Windows)
            );
        }
    }

    #[test]
    fn other_systems_let_mpv_pick_its_gpu_api_too() {
        let plan = build_on(
            Platform::Other,
            &settings(DisplayMode::Auto),
            1,
            "mpv",
            None,
        );
        assert!(!has(&plan.mpv_args, "gpu-api"));
        assert_eq!(flag(&plan.mpv_args, "hwdec"), "auto-safe");
    }
}

mod mpv_ipc_messages {
    use super::*;

    #[test]
    fn property_change_is_recognised() {
        let m = MpvIpcMessage::try_parse_line(
            r#"{"event":"property-change","id":1,"name":"paused-for-cache","data":true}"#,
        )
        .expect("a message");
        assert!(m.is_property_change());
        assert_eq!(m.kind, MpvIpcMessageKind::Event);
        assert_eq!(m.name.as_deref(), Some("paused-for-cache"));
        assert_eq!(m.id, Some(1));
        assert_eq!(m.data, json!(true));
    }

    #[test]
    fn reply_success_and_error_are_distinguished() {
        let ok = MpvIpcMessage::try_parse_line(r#"{"error":"success","data":1.5,"request_id":7}"#)
            .unwrap();
        assert!(ok.is_success());
        assert_eq!(ok.request_id, Some(7));
        assert_eq!(ok.data, json!(1.5));

        let bad =
            MpvIpcMessage::try_parse_line(r#"{"error":"property unavailable","request_id":8}"#)
                .unwrap();
        assert!(!bad.is_success());
        assert_eq!(bad.error.as_deref(), Some("property unavailable"));
        assert_eq!(bad.data, Value::Null);
        // a reply without a status counts as success, an event never does
        assert!(MpvIpcMessage::try_parse_line(r#"{"request_id":9}"#)
            .unwrap()
            .is_success());
        assert!(!MpvIpcMessage::try_parse_line(r#"{"event":"idle"}"#)
            .unwrap()
            .is_success());
    }

    #[test]
    fn garbage_lines_are_ignored() {
        assert_eq!(MpvIpcMessage::try_parse_line("not json"), None);
        assert_eq!(MpvIpcMessage::try_parse_line(""), None);
        assert_eq!(MpvIpcMessage::try_parse_line("   \t"), None);
        assert_eq!(MpvIpcMessage::try_parse_line("[1,2]"), None);
        assert_eq!(MpvIpcMessage::try_parse_line("42"), None);
        assert_eq!(MpvIpcMessage::try_parse_line("{\"event\":"), None);
    }

    #[test]
    fn other_shapes_are_kept_but_not_interpreted() {
        let end = MpvIpcMessage::try_parse_line(r#"{"event":"end-file","reason":"error","playlist_entry_id":1,"file_error":"loading failed"}"#).unwrap();
        assert_eq!(
            (end.event.as_deref(), end.reason.as_deref()),
            (Some("end-file"), Some("error"))
        );
        assert!(!end.is_property_change());
        assert_eq!(end.raw["file_error"], json!("loading failed"));

        let odd = MpvIpcMessage::try_parse_line(r#" {"something":1} trailing"#).unwrap();
        assert_eq!(odd.kind, MpvIpcMessageKind::Unknown);
        assert_eq!(odd.raw.len(), 1);
        // an event name that is not text is no event
        let not_event = MpvIpcMessage::try_parse_line(r#"{"event":5,"error":null}"#).unwrap();
        assert_eq!(not_event.kind, MpvIpcMessageKind::Reply);
        assert_eq!(not_event.error, None);
        // ids arrive as numbers, but a string or a float would still be read
        let loose = MpvIpcMessage::try_parse_line(
            r#"{"event":"property-change","id":"12","name":"fullscreen","data":"yes"}"#,
        )
        .unwrap();
        assert_eq!(loose.id, Some(12));
    }

    #[test]
    fn commands_serialise_to_one_line() {
        let cmd = mpv_ipc::observe_property(3, "demuxer-cache-duration");
        assert!(!cmd.contains('\n'));
        assert_eq!(
            cmd,
            r#"{"command":["observe_property",3,"demuxer-cache-duration"]}"#
        );

        let set = mpv_ipc::set_property("fullscreen", true, Some(9));
        assert_eq!(
            set,
            r#"{"command":["set_property","fullscreen",true],"request_id":9}"#
        );
        assert_eq!(
            mpv_ipc::set_property("speed", 1.01, None),
            r#"{"command":["set_property","speed",1.01]}"#
        );
        assert_eq!(
            mpv_ipc::get_property("mouse-pos", 4),
            r#"{"command":["get_property","mouse-pos"],"request_id":4}"#
        );
        assert_eq!(
            mpv_ipc::cycle_property("pause", Some(5)),
            r#"{"command":["cycle","pause"],"request_id":5}"#
        );
        assert_eq!(
            mpv_ipc::cycle_property("pause", None),
            r#"{"command":["cycle","pause"]}"#
        );
        assert_eq!(mpv_ipc::quit(None), r#"{"command":["quit"]}"#);
        assert_eq!(
            mpv_ipc::quit(Some(6)),
            r#"{"command":["quit"],"request_id":6}"#
        );
    }

    #[test]
    fn any_command_can_be_sent() {
        let line = mpv_ipc::command(Some(21), &[json!("keypress"), json!("MBTN_LEFT")]);
        assert_eq!(
            line,
            r#"{"command":["keypress","MBTN_LEFT"],"request_id":21}"#
        );
        let line = mpv_ipc::command(
            None,
            &[
                json!("show-text"),
                json!("a \"quoted\"\nline / ünï"),
                json!(1500),
            ],
        );
        assert_eq!(
            line,
            r#"{"command":["show-text","a \"quoted\"\nline / ünï",1500]}"#
        );
        assert!(!line.contains('\n'));
        assert_eq!(mpv_ipc::command(None, &[]), r#"{"command":[]}"#);
        // what we send, mpv-style parsers read back
        let back = MpvIpcMessage::try_parse_line(&mpv_ipc::command(
            Some(3),
            &[json!("set_property"), json!("mute"), json!(false)],
        ))
        .unwrap();
        assert_eq!(back.request_id, Some(3));
        assert_eq!(back.raw["command"], json!(["set_property", "mute", false]));
    }

    #[test]
    fn the_observed_properties_keep_their_ids_and_gain_the_remote_control_ones() {
        let expected = [
            (1, "demuxer-cache-duration"),
            (2, "paused-for-cache"),
            (3, "frame-drop-count"),
            (4, "decoder-frame-drop-count"),
            (5, "estimated-vf-fps"),
            (6, "hwdec-current"),
            (7, "video-params"),
            (8, "video-target-params"),
            (9, "video-bitrate"),
            (10, "cache-buffering-state"),
            (11, "speed"),
            (12, "fullscreen"),
            (13, "mouse-pos"),
            (14, "osd-dimensions"),
        ];
        assert_eq!(observed::ALL, expected);
        let commands: Vec<String> = observed::observe_commands().collect();
        assert_eq!(commands.len(), 14);
        assert_eq!(
            commands[0],
            r#"{"command":["observe_property",1,"demuxer-cache-duration"]}"#
        );
        assert_eq!(
            commands[12],
            r#"{"command":["observe_property",13,"mouse-pos"]}"#
        );
        assert_eq!(
            commands[13],
            r#"{"command":["observe_property",14,"osd-dimensions"]}"#
        );
    }

    #[test]
    fn the_remote_control_properties_are_read_from_the_raw_value() {
        let pos = MpvIpcMessage::try_parse_line(r#"{"event":"property-change","id":13,"name":"mouse-pos","data":{"x":640,"y":360,"hover":true}}"#).unwrap();
        assert!(pos.is_property_change());
        assert_eq!(
            (pos.id, pos.name.as_deref()),
            (Some(13), Some(observed::MOUSE_POS))
        );
        assert_eq!(pos.data["x"], json!(640));
        assert_eq!(pos.data["hover"], json!(true));

        let osd = MpvIpcMessage::try_parse_line(
            r#"{"event":"property-change","id":14,"name":"osd-dimensions","data":{"w":1920,"h":1200,"par":1.0,"aspect":1.6,"mt":60,"mb":60,"ml":0,"mr":0}}"#,
        )
        .unwrap();
        assert_eq!(osd.data["w"], json!(1920));
        assert_eq!(osd.data["mt"].as_i64(), Some(60));

        // the status model leaves them alone
        let mut status = MpvStatusModel::new();
        assert!(!status.apply(&pos));
        assert!(!status.apply(&osd));
        assert_eq!(status, MpvStatusModel::new());
    }

    #[test]
    fn the_status_model_follows_property_changes() {
        let mut status = MpvStatusModel::new();
        let mut feed = |line: &str| status.apply(&MpvIpcMessage::try_parse_line(line).unwrap());
        assert!(feed(
            r#"{"event":"property-change","id":1,"name":"demuxer-cache-duration","data":2.5}"#
        ));
        assert!(feed(
            r#"{"event":"property-change","id":2,"name":"paused-for-cache","data":true}"#
        ));
        assert!(feed(
            r#"{"event":"property-change","id":10,"name":"cache-buffering-state","data":27}"#
        ));
        assert!(feed(
            r#"{"event":"property-change","id":3,"name":"frame-drop-count","data":4}"#
        ));
        assert!(feed(
            r#"{"event":"property-change","id":4,"name":"decoder-frame-drop-count","data":1}"#
        ));
        assert!(feed(
            r#"{"event":"property-change","id":5,"name":"estimated-vf-fps","data":59.94}"#
        ));
        assert!(feed(
            r#"{"event":"property-change","id":6,"name":"hwdec-current","data":"d3d11va"}"#
        ));
        assert!(feed(
            r#"{"event":"property-change","id":9,"name":"video-bitrate","data":39876543}"#
        ));
        assert!(feed(
            r#"{"event":"property-change","id":11,"name":"speed","data":1.05}"#
        ));
        assert!(feed(
            r#"{"event":"property-change","id":12,"name":"fullscreen","data":true}"#
        ));
        assert!(feed(
            r#"{"event":"property-change","id":7,"name":"video-params","data":{"pixelformat":"p010","w":3840,"h":2160,"colormatrix":"bt.2020-ncl","primaries":"bt.2020","gamma":"pq","colorlevels":"limited","max-luma":1000.0,"min-luma":0.0001,"sig-peak":4.926}}"#
        ));
        assert!(feed(
            r#"{"event":"property-change","id":8,"name":"video-target-params","data":{"primaries":"bt.709","gamma":"bt.1886","sig-peak":1.0}}"#
        ));
        // not a property change, an unknown property, a reply: nothing happens
        assert!(!feed(r#"{"event":"file-loaded"}"#));
        assert!(!feed(
            r#"{"event":"property-change","id":99,"name":"volume","data":50}"#
        ));
        assert!(!feed(r#"{"error":"success","request_id":1}"#));

        assert_eq!(status.cache_seconds, Some(2.5));
        assert!(status.paused_for_cache && status.fullscreen);
        assert_eq!(
            (
                status.buffering_percent,
                status.frame_drops,
                status.decoder_frame_drops
            ),
            (Some(27), 4, 1)
        );
        assert_eq!(
            (status.fps, status.hw_dec.as_deref()),
            (Some(59.94), Some("d3d11va"))
        );
        assert!(status.last_update_utc.is_some());
        assert_eq!(status.describe_bitrate(), "39.9 Mbps");
        assert_eq!(
            status.describe_cache(),
            "2.50 s  (buffering 27%)  speed 1.05x"
        );

        let source = status.source.clone().unwrap();
        assert!(source.is_hdr());
        assert_eq!(
            source.describe(),
            "bt.2020 / pq / bt.2020-ncl / p010 / 3840x2160 / peak 1000 nit"
        );
        assert_eq!(source.to_string(), source.describe());
        let target = status.target.clone().unwrap();
        assert!(!target.is_hdr());
        assert_eq!(target.describe(), "bt.709 / bt.1886 / sig-peak 1");

        // mpv reports a property that went away as null
        assert!(status.apply_property(observed::DEMUXER_CACHE_DURATION, &Value::Null));
        assert!(status.apply_property(observed::VIDEO_PARAMS, &Value::Null));
        assert!(status.apply_property(observed::PAUSED_FOR_CACHE, &json!("no")));
        assert!(status.apply_property(observed::SPEED, &Value::Null));
        assert_eq!(
            (
                status.cache_seconds,
                status.source.as_ref(),
                status.paused_for_cache,
                status.speed
            ),
            (None, None, false, 1.0)
        );
        assert_eq!(status.describe_cache(), "-");

        status.reset();
        assert_eq!(status, MpvStatusModel::new());
        assert_eq!(status.describe_bitrate(), "-");
        assert_eq!(status.describe_cache(), "-");
    }

    #[test]
    fn video_params_describe_only_what_mpv_reported() {
        assert_eq!(MpvVideoParams::from_node(&json!(null)), None);
        assert_eq!(MpvVideoParams::from_node(&json!("x")), None);
        assert_eq!(
            MpvVideoParams::from_node(&json!({})).unwrap().describe(),
            "-"
        );
        let hlg = MpvVideoParams::from_node(&json!({"gamma": "hlg", "w": 1920, "sig-peak": 3.456}))
            .unwrap();
        assert!(hlg.is_hdr());
        assert_eq!(hlg.describe(), "hlg / sig-peak 3.46"); // a width without a height is not a size
        let buffering = MpvStatusModel {
            cache_seconds: Some(0.4),
            paused_for_cache: true,
            ..MpvStatusModel::new()
        };
        assert_eq!(buffering.describe_cache(), "0.40 s  (buffering)");
    }
}

mod mpv_log_classifier {
    use super::*;

    #[test]
    fn recognises_a_relay_without_a_publisher() {
        assert!(mpv_log::is_stream_unavailable(
            "[ffmpeg/demuxer] srt: no stream is available on path 'screen'"
        ));
    }

    #[test]
    fn classifies_authentication_and_passphrase_problems() {
        let auth =
            mpv_log::classify(["[ffmpeg] srt: Connection setup failure: authentication failed"]);
        assert_eq!(auth, MpvExitReason::AuthFailed);
        assert!(!mpv_log::describe(auth).is_empty());
    }

    #[test]
    fn unknown_lines_stay_unknown() {
        assert_eq!(
            mpv_log::classify_line("Playing: srt://..."),
            MpvExitReason::Unknown
        );
    }
}

mod receiver_settings {
    use super::*;

    #[test]
    fn defaults_are_the_japan_preset() {
        let s = ReceiverSettings::default();
        assert_eq!(s.latency_ms, 1500);
        assert_eq!(s.overhead_percent(), 25);
        assert!(s.validate().is_empty());
        assert_eq!(s.clone().normalized(), s);
        assert_eq!(s.hw_dec, HwDecMode::D3d11va); // = the platform's hardware decoder
    }

    #[test]
    fn validation_reports_the_usual_mistakes() {
        let s = ReceiverSettings {
            latency_ms: 5,
            expected_bitrate_mbps: 0,
            buffer_seconds: 0.0,
            ..ReceiverSettings::default()
        };
        assert_eq!(
            s.validate(),
            [
                "Latency must be at least 20 ms.",
                "Expected bitrate must be at least 1 Mbps.",
                "Buffer seconds must be positive."
            ]
        );
    }

    #[test]
    fn read_role_options_include_the_receive_buffer() {
        let o = ReceiverSettings::default().to_srt_options(40000, "0123456789abcdef");
        assert_eq!(o.role, SrtRole::Read);
        assert_eq!(o.topology, SrtTopology::Caller);
        assert_eq!(o.host, "127.0.0.1");
        assert_eq!(o.port, 40000);
        assert!(o.include_recv_socket_buffer);
        assert!(!o.rate_control);
        assert_eq!(
            (
                o.latency_ms,
                o.bitrate_bps,
                o.pb_key_len,
                o.connect_timeout_ms
            ),
            (1500, 100_000_000, 32, 5000)
        );
        assert_eq!(o.passphrase.as_deref(), Some("0123456789abcdef"));
        assert_eq!(
            ReceiverSettings::default().to_srt_options(1, "").passphrase,
            None
        );
    }

    #[test]
    fn an_old_settings_file_with_relay_fields_still_loads() {
        let json = r#"{"mode":0,"host":"relay.example.net","port":8890,"path":"screen","user":"viewer","password":"pw","passphrase":"0123456789abcdef","latencyMs":3500,"expectedBitrateMbps":80}"#;
        let mut s: ReceiverSettings = serde_json::from_str(json).expect("an old file still reads");
        s.normalize();
        assert_eq!(s.latency_ms, 3500);
        assert_eq!(s.expected_bitrate_mbps, 80);
        assert_eq!(s.overhead_percent(), 50);
    }

    #[test]
    fn presets_cover_fast_and_japan_australia_and_china() {
        let presets = ReceiverSettings::LATENCY_PRESETS;
        let pairs: Vec<(&str, i32)> = presets.iter().map(|p| (p.name, p.latency_ms)).collect();
        assert_eq!(
            pairs,
            [
                ("Fast", 300),
                ("Japan", 1500),
                ("Australia", 2000),
                ("China", 3500)
            ]
        );
        assert_eq!(presets[0].to_string(), "Fast (300 ms)");
        assert_eq!(presets[3].to_string(), "China (3500 ms)");
        // the regions keep seconds of retransmission room; only the remote-control preset is short
        assert!(presets[1..].iter().all(|p| p.latency_ms >= 1000));
        // every preset survives normalisation, and only the cross-border one asks for more overhead
        for preset in presets {
            let s = ReceiverSettings {
                latency_ms: preset.latency_ms,
                ..ReceiverSettings::default()
            }
            .normalized();
            assert_eq!(s.latency_ms, preset.latency_ms);
            assert!(s.validate().is_empty());
            assert_eq!(
                s.overhead_percent(),
                if preset.name == "China" { 50 } else { 25 }
            );
        }
        assert_eq!(
            serde_json::to_value(presets[0]).unwrap(),
            json!({"name": "Fast", "latencyMs": 300})
        );
        assert_eq!(
            ReceiverSettings::TONE_MAPPING_ALGORITHMS[0],
            ReceiverSettings::DEFAULT_TONE_MAPPING
        );
    }

    #[test]
    fn normalize_repairs_hand_edited_values() {
        let s = ReceiverSettings {
            latency_ms: 5,
            expected_bitrate_mbps: 0,
            buffer_seconds: f64::NAN,
            fullscreen_screen: -1,
            tone_mapping: "  ".to_string(),
            last_host: "  abc  ".to_string(),
            ..ReceiverSettings::default()
        }
        .normalized();
        assert_eq!(
            (
                s.latency_ms,
                s.expected_bitrate_mbps,
                s.buffer_seconds,
                s.fullscreen_screen
            ),
            (1500, 100, 1.5, 0)
        );
        assert_eq!(
            (s.tone_mapping.as_str(), s.last_host.as_str()),
            ("bt.2390", "abc")
        );

        let s = ReceiverSettings {
            latency_ms: 99_999,
            expected_bitrate_mbps: 5000,
            buffer_seconds: 99.0,
            ..ReceiverSettings::default()
        }
        .normalized();
        assert_eq!(
            (s.latency_ms, s.expected_bitrate_mbps, s.buffer_seconds),
            (30000, 1000, 30.0)
        );
        let s = ReceiverSettings {
            latency_ms: 20,
            buffer_seconds: 0.05,
            ..ReceiverSettings::default()
        }
        .normalized();
        assert_eq!((s.latency_ms, s.buffer_seconds), (20, 1.5));
    }
}
