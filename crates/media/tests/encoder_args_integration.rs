//! Runs the option strings of `encoder_profiles` through a real ffmpeg (port of
//! tests/EncoderArgsIntegrationTests.cs). Unit tests only prove the builder emits what we intended; only
//! ffmpeg can prove the option NAMES exist and the values parse.
//!
//! The CPU encoder must work wherever ffmpeg has it. A hardware encoder needs its GPU: when the encode fails
//! for any reason other than ffmpeg refusing the command line, the test says so and moves on. The app does the
//! same at run time: "Test encoders" runs `ffmpeg_command::build_probe` on the user's own machine.
//! `hevc_videotoolbox` exists only in macOS builds of ffmpeg, so on Windows it is never exercised here.
//!
//! Each test is skipped, with a note, when ffmpeg is missing (see tests/common/mod.rs).

mod common;

use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use common::{has_encoder, is_option_error, note, run, tail, tool_or_skip, TempDir};
use hdrcast_capture::{color, CaptureFormat};
use hdrcast_core::common::arg_quote;
use hdrcast_media::sender::encoder_probe;
use hdrcast_media::sender::encoder_profiles::{self, EncoderSettings, QualityPreset, VideoEncoder};
use hdrcast_media::sender::ffmpeg_command::{self, AudioCodec, FfmpegPlan, VideoInput};
use hdrcast_media::Platform;

const PRESETS: [QualityPreset; 3] = [
    QualityPreset::Speed,
    QualityPreset::Balanced,
    QualityPreset::Quality,
];

fn range(hdr: bool) -> &'static str {
    if hdr {
        "HDR"
    } else {
        "SDR"
    }
}

#[test]
fn ffmpeg_accepts_the_generated_options() {
    let Some(ffmpeg) = tool_or_skip("ffmpeg", "ffmpeg_accepts_the_generated_options") else {
        return;
    };
    for info in encoder_profiles::for_platform(Platform::CURRENT) {
        if !has_encoder(&ffmpeg, info.ffmpeg_name) {
            note(&format!(
                "{}: not in this ffmpeg build, options NOT checked",
                info.ffmpeg_name
            ));
            continue;
        }
        for hdr in [false, true] {
            let mut encoded = 0;
            for preset in PRESETS {
                // with every switch on, then with B-frames and lookahead off (other options appear and vanish)
                for switches in [true, false] {
                    let settings = EncoderSettings {
                        encoder: info.id,
                        preset,
                        hdr,
                        bitrate_kbps: 8000,
                        fps: 30,
                        gop_seconds: 2.0,
                        buffer_seconds: 2.0,
                        b_frames: switches,
                        lookahead: switches,
                        mastering_max_nits: 1000,
                    };
                    // a hardware encoder may refuse a tiny picture; x265 is merely faster on one
                    let args = if info.is_hardware {
                        ffmpeg_command::build_probe(&settings)
                    } else {
                        ffmpeg_command::build_probe_sized(&settings, 320, 240)
                    };
                    let (output, exit) = run(&ffmpeg, &args);
                    let what = format!(
                        "{} / {preset:?} / {} / switches {switches}",
                        info.ffmpeg_name,
                        range(hdr)
                    );
                    assert!(
                        !is_option_error(&output),
                        "{what}: ffmpeg refused the command line:\n{}\n{}",
                        arg_quote::join(&args),
                        tail(&output)
                    );
                    if exit == 0 {
                        encoded += 1;
                    } else {
                        assert!(
                            info.is_hardware,
                            "{what} was rejected by ffmpeg:\n{}\n{}",
                            arg_quote::join(&args),
                            tail(&output)
                        );
                    }
                }
            }
            note(&match encoded {
                6 => format!(
                    "{} {}: 6 option sets really encoded",
                    info.ffmpeg_name,
                    range(hdr)
                ),
                0 => format!(
                    "{} {}: 6 option sets parsed by ffmpeg, but no such GPU here: nothing encoded",
                    info.ffmpeg_name,
                    range(hdr)
                ),
                n => format!(
                    "{} {}: only {n} of 6 option sets encoded on this GPU",
                    info.ffmpeg_name,
                    range(hdr)
                ),
            });
        }
    }
}

#[test]
fn mpegts_accepts_a_real_hevc_main10_stream_with_our_muxer_options() {
    let test = "mpegts_accepts_a_real_hevc_main10_stream_with_our_muxer_options";
    let Some(ffmpeg) = tool_or_skip("ffmpeg", test) else {
        return;
    };
    if !has_encoder(&ffmpeg, "libx265") {
        return note(&format!("SKIPPED {test}: this ffmpeg has no libx265"));
    }

    let dir = TempDir::new("mux");
    let ts = dir.file("out.ts");
    let settings = EncoderSettings {
        encoder: VideoEncoder::LibX265,
        preset: QualityPreset::Speed,
        hdr: true,
        bitrate_kbps: 8000,
        fps: 30,
        gop_seconds: 1.0,
        buffer_seconds: 2.0,
        mastering_max_nits: 1000,
        ..EncoderSettings::default()
    };
    let mut args: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=30:duration=1",
        "-pix_fmt",
        "p010le",
    ]
    .map(String::from)
    .to_vec();
    args.extend(encoder_profiles::build_video_args(&settings));
    args.extend(
        [
            "-muxdelay",
            "0",
            "-muxpreload",
            "0",
            "-f",
            "mpegts",
            "-mpegts_flags",
            "+resend_headers",
            "-y",
        ]
        .map(String::from),
    );
    args.push(ts.to_string_lossy().into_owned());
    let (log, exit) = run(&ffmpeg, &args);
    assert_eq!(
        exit,
        0,
        "encode failed:\n{}\n{}",
        arg_quote::join(&args),
        tail(&log)
    );

    // Read it back: the colour tags the viewer depends on must survive the muxer.
    let Some(ffprobe) = tool_or_skip("ffprobe", "the read-back half of the MPEG-TS test") else {
        return;
    };
    let (probe, probe_exit) = run(
        &ffprobe,
        [
            "-hide_banner",
            "-loglevel",
            "error",
            "-show_streams",
            "-select_streams",
            "v:0",
            &ts.to_string_lossy(),
        ],
    );
    assert_eq!(probe_exit, 0, "{probe}");
    for expected in [
        "codec_name=hevc",
        "profile=Main 10",
        "pix_fmt=yuv420p10le",
        "color_transfer=smpte2084",
        "color_primaries=bt2020",
        "color_space=bt2020nc",
        "color_range=tv",
    ] {
        assert!(
            probe.contains(expected),
            "{expected} missing from:\n{probe}"
        );
    }
    note("libx265 HDR10 -> MPEG-TS with our muxer options: encoded and read back with ffprobe");
}

/// When the capture engine encodes (docs/design.md §2.8), ffmpeg reads an MPEG-TS of HEVC with B-frames from the
/// video pipe and raw f32le from the audio pipe, copies the video and encodes the sound. The stand-in for the
/// engine's stream is ffmpeg's own (the engine's writer has unit tests of its own and the NVENC run in the
/// checklist); what is checked here is that this ffmpeg takes the command line and that every picture, with
/// its times, comes out next to the sound.
#[test]
fn a_stream_the_engine_encoded_is_copied_and_gets_its_sound() {
    let test = "a_stream_the_engine_encoded_is_copied_and_gets_its_sound";
    let Some(ffmpeg) = tool_or_skip("ffmpeg", test) else {
        return;
    };
    if !has_encoder(&ffmpeg, "libx265") {
        return note(&format!("SKIPPED {test}: this ffmpeg has no libx265"));
    }
    let dir = TempDir::new("engine-ts");
    let (video, audio, out) = (dir.file("engine.ts"), dir.file("audio.f32"), dir.file("out.ts"));
    let video_path = video.to_string_lossy().into_owned();
    let audio_path = audio.to_string_lossy().into_owned();
    let (log, exit) = run(
        &ffmpeg,
        [
            "-hide_banner", "-loglevel", "error", "-nostdin", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30:duration=2",
            "-pix_fmt", "yuv420p10le", "-c:v", "libx265", "-preset", "ultrafast", "-x265-params", "bframes=2:keyint=30:log-level=error",
            "-f", "mpegts", "-y", &video_path,
        ],
    );
    assert_eq!(exit, 0, "{}", tail(&log));
    let (log, exit) = run(
        &ffmpeg,
        ["-hide_banner", "-loglevel", "error", "-nostdin", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=2", "-ac", "2", "-f", "f32le", "-y", &audio_path],
    );
    assert_eq!(exit, 0, "{}", tail(&log));

    let plan = FfmpegPlan {
        video_pipe_name: "engine-video".to_string(),
        video_input: VideoInput::EncodedTs,
        audio_pipe_name: Some("engine-audio".to_string()),
        pixel_format: CaptureFormat::P010,
        width: 320,
        height: 240,
        fps: 30,
        encoder: EncoderSettings { encoder: VideoEncoder::HevcNvenc, hdr: true, fps: 30, ..EncoderSettings::default() },
        audio: AudioCodec::Aac,
        progress: false,
        output_url: out.to_string_lossy().into_owned(),
        log_level: "error".to_string(),
        ..FfmpegPlan::default()
    };
    let mut args = ffmpeg_command::build(&plan).unwrap();
    assert!(args.windows(2).any(|w| w == ["-c:v", "copy"]), "{args:?}");
    assert!(!args.iter().any(|a| a == "hevc_nvenc" || a == "-fps_mode"), "ffmpeg encodes nothing of the video: {args:?}");
    for (pipe, file) in [("engine-video", &video_path), ("engine-audio", &audio_path)] {
        let at = args.iter().position(|a| *a == ffmpeg_command::pipe_path(pipe)).expect("the pipe is an input");
        args[at] = file.clone();
    }
    let (log, exit) = run(&ffmpeg, &args);
    assert!(!is_option_error(&log), "ffmpeg refused the command line:\n{}\n{}", arg_quote::join(&args), tail(&log));
    assert_eq!(exit, 0, "{}\n{}", arg_quote::join(&args), tail(&log));

    let Some(ffprobe) = tool_or_skip("ffprobe", "the read-back half of the engine stream test") else {
        return;
    };
    let out_path = out.to_string_lossy().into_owned();
    let (probe, probe_exit) = run(&ffprobe, ["-hide_banner", "-loglevel", "error", "-show_entries", "stream=codec_type,codec_name,profile", "-of", "csv=p=0", &out_path]);
    assert_eq!(probe_exit, 0, "{probe}");
    assert!(probe.contains("hevc,Main 10,video"), "{probe}");
    assert!(probe.contains("aac,LC,audio"), "{probe}");
    let (frames, _) = run(&ffprobe, ["-hide_banner", "-loglevel", "error", "-select_streams", "v:0", "-show_entries", "packet=pts", "-of", "csv=p=0", &out_path]);
    let mut pts: Vec<i64> = frames.lines().filter_map(|l| l.trim().trim_end_matches(',').parse().ok()).collect();
    assert_eq!(pts.len(), 60, "every picture copied");
    pts.sort_unstable();
    assert!(pts.windows(2).all(|w| w[1] - w[0] == 3000), "one picture every 1/30 s: {pts:?}");
    note("an engine-like MPEG-TS (HEVC with B-frames) + raw sound -> ffmpeg -c:v copy: every picture and the sound came out");
}

fn put(frame: &mut [u8], at: usize, code10: i32) {
    frame[at..at + 2].copy_from_slice(&color::to_p010_sample(code10).to_le_bytes());
}

fn get(frame: &[u8], at: usize) -> i32 {
    color::from_p010_sample(u16::from_le_bytes([frame[at], frame[at + 1]]))
}

/// v0.2.1 with ffmpeg 9: the raw input was untagged, so ffmpeg treated it as BT.601 and re-matrixed it to
/// BT.2020 (blue lost 23 luma code values) and NVENC wrote transfer / primaries "unknown": viewers saw the PQ
/// picture as SDR with odd colours. Encodes known HDR10 patches with the sender's own arguments.
#[test]
fn hdr_colours_come_out_of_the_encoder_unchanged_and_tagged() {
    let test = "hdr_colours_come_out_of_the_encoder_unchanged_and_tagged";
    let Some(ffmpeg) = tool_or_skip("ffmpeg", test) else {
        return;
    };
    let ffprobe = tool_or_skip("ffprobe", "the tag check of the HDR colour test");

    const W: usize = 320;
    const H: usize = 160;
    const PW: usize = 64; // 5 x 2 patches
    const PH: usize = 80;
    let rgb: [[i32; 3]; 10] = [
        [255, 255, 255],
        [128, 128, 128],
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [0, 255, 255],
        [255, 0, 255],
        [255, 255, 0],
        [230, 180, 140],
        [-1, -1, -1],
    ];
    // an SDR window on an HDR desktop at 200 nit SDR brightness (scRGB 2.5); the last one a 1000 nit highlight
    let expected: Vec<(i32, i32, i32)> = rgb
        .iter()
        .map(|patch| {
            let c = if patch[0] < 0 {
                [12.5; 3]
            } else {
                patch.map(|v| color::srgb_to_linear(f64::from(v) / 255.0) * 2.5)
            };
            color::scrgb_to_yuv10(c[0], c[1], c[2], 80.0)
        })
        .collect();
    let mut frame = vec![0u8; W * H * 3];
    for (i, &(y10, cb10, cr10)) in expected.iter().enumerate() {
        let (x0, y0) = ((i % 5) * PW, (i / 5) * PH);
        for y in y0..y0 + PH {
            for x in x0..x0 + PW {
                put(&mut frame, (y * W + x) * 2, y10);
            }
        }
        for y in y0 / 2..(y0 + PH) / 2 {
            for x in (x0..x0 + PW).step_by(2) {
                put(&mut frame, W * H * 2 + (y * W + x) * 2, cb10);
                put(&mut frame, W * H * 2 + (y * W + x) * 2 + 2, cr10);
            }
        }
    }

    let dir = TempDir::new("colour");
    let raw = dir.file("in.p010");
    std::fs::write(&raw, frame.repeat(30)).unwrap();

    for info in encoder_profiles::for_platform(Platform::CURRENT) {
        let encoder = info.id;
        if !has_encoder(&ffmpeg, info.ffmpeg_name) {
            note(&format!(
                "{}: not in this ffmpeg build, HDR colours NOT checked",
                info.ffmpeg_name
            ));
            continue;
        }
        let ts = dir.file(&format!("{}.ts", info.ffmpeg_name));
        let back = dir.file(&format!("{}.p010", info.ffmpeg_name));

        let mut args = ffmpeg_command::build(&FfmpegPlan {
            video_pipe_name: "colour-test".to_string(),
            pixel_format: CaptureFormat::P010,
            width: W as u32,
            height: H as u32,
            fps: 30,
            encoder: EncoderSettings {
                encoder,
                preset: QualityPreset::Speed,
                hdr: true,
                bitrate_kbps: 8000,
                fps: 30,
                gop_seconds: 1.0,
                ..EncoderSettings::default()
            },
            audio: AudioCodec::None,
            progress: false,
            output_url: ts.to_string_lossy().into_owned(),
            log_level: "error".to_string(),
            ..FfmpegPlan::default()
        })
        .unwrap();
        let pipe = ffmpeg_command::pipe_path("colour-test");
        let input = args
            .iter()
            .position(|a| *a == pipe)
            .expect("the pipe is the input");
        args[input] = raw.to_string_lossy().into_owned();

        let (log, exit) = run(&ffmpeg, &args);
        assert!(
            !is_option_error(&log),
            "{}: ffmpeg refused the command line:\n{}\n{}",
            info.ffmpeg_name,
            arg_quote::join(&args),
            tail(&log)
        );
        if exit != 0 && info.is_hardware {
            note(&format!(
                "{}: no such GPU here (or it refuses a {W}x{H} picture), HDR colours NOT checked",
                info.ffmpeg_name
            ));
            continue;
        }
        assert_eq!(
            exit,
            0,
            "encode failed:\n{}\n{}",
            arg_quote::join(&args),
            tail(&log)
        );

        if let Some(ffprobe) = &ffprobe {
            let (probe, probe_exit) = run(
                ffprobe,
                [
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-show_streams",
                    "-select_streams",
                    "v:0",
                    &ts.to_string_lossy(),
                ],
            );
            assert_eq!(probe_exit, 0, "{probe}");
            for tag in [
                "color_transfer=smpte2084",
                "color_primaries=bt2020",
                "color_space=bt2020nc",
                "color_range=tv",
            ] {
                assert!(
                    probe.contains(tag),
                    "{}: {tag} missing from:\n{probe}",
                    info.ffmpeg_name
                );
            }
        }

        let (decode_log, decode_exit) = run(
            &ffmpeg,
            [
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-i",
                &ts.to_string_lossy(),
                "-frames:v",
                "20",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "p010le",
                &back.to_string_lossy(),
            ],
        );
        assert_eq!(decode_exit, 0, "{decode_log}");
        let all = std::fs::read(&back).unwrap();
        assert!(
            all.len() >= frame.len(),
            "{}: nothing was decoded",
            info.ffmpeg_name
        );
        let last = &all[all.len() - frame.len()..]; // the last decoded frame
        let mut worst = 0;
        for (i, &(y10, cb10, cr10)) in expected.iter().enumerate() {
            let (cx, cy) = ((i % 5) * PW + PW / 2, (i / 5) * PH + PH / 2); // patch centre, away from the edges
            let y = get(last, (cy * W + cx) * 2);
            let cb = get(last, W * H * 2 + ((cy / 2) * W + cx) * 2);
            let cr = get(last, W * H * 2 + ((cy / 2) * W + cx) * 2 + 2);
            let off = (y - y10)
                .abs()
                .max((cb - cb10).abs())
                .max((cr - cr10).abs());
            assert!(
                off <= 3,
                "{} patch {i}: expected {y10}/{cb10}/{cr10}, got {y}/{cb}/{cr}",
                info.ffmpeg_name
            );
            worst = worst.max(off);
        }
        note(&format!("{}: 10 HDR10 patches encoded, tagged PQ/BT.2020 and decoded within {worst} code value(s)", info.ffmpeg_name));
    }
}

/// This is why HdrCast is HEVC-only: the P2P fan-out carries MPEG-TS. If a future ffmpeg gains
/// AV1-in-MPEG-TS this test starts failing, which is the signal to revisit that restriction.
#[test]
fn av1_really_cannot_be_muxed_into_mpegts() {
    let test = "av1_really_cannot_be_muxed_into_mpegts";
    let Some(ffmpeg) = tool_or_skip("ffmpeg", test) else {
        return;
    };
    if !has_encoder(&ffmpeg, "libsvtav1") {
        return note(&format!("SKIPPED {test}: this ffmpeg has no libsvtav1"));
    }

    let dir = TempDir::new("av1");
    let ts = dir.file("av1.ts");
    let (_, exit) = run(
        &ffmpeg,
        [
            "-hide_banner",
            "-loglevel",
            "warning",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=30:duration=1",
            "-pix_fmt",
            "yuv420p10le",
            "-c:v",
            "libsvtav1",
            "-preset",
            "12",
            "-b:v",
            "2000k",
            "-f",
            "mpegts",
            "-y",
            &ts.to_string_lossy(),
        ],
    );
    if exit != 0 {
        return note("ffmpeg refuses to put AV1 into MPEG-TS outright"); // also fine
    }
    let Some(ffprobe) = tool_or_skip("ffprobe", "the read-back half of the AV1 test") else {
        return;
    };
    let (probe, probe_exit) = run(
        &ffprobe,
        [
            "-hide_banner",
            "-loglevel",
            "error",
            "-show_streams",
            &ts.to_string_lossy(),
        ],
    );
    if probe_exit != 0 {
        return note("ffprobe cannot read the AV1-in-MPEG-TS file at all");
    }
    assert!(
        !probe.contains("codec_name=av1"),
        "this ffmpeg can carry AV1 in MPEG-TS now:\n{probe}"
    ); // a private stream, not AV1
    note("AV1 in MPEG-TS still comes back as a private stream, not as AV1");
}

/// The probe the app runs before a stream: the real `-encoders` listing, one real encode per offered encoder.
#[test]
fn the_probe_finds_the_encoders_of_this_machine() {
    let Some(ffmpeg) = tool_or_skip("ffmpeg", "the_probe_finds_the_encoders_of_this_machine")
    else {
        return;
    };

    let listed = encoder_probe::list_encoders(&ffmpeg);
    assert!(
        listed.len() > 20,
        "the -encoders listing was not read: {listed:?}"
    );
    for info in &encoder_profiles::ALL {
        assert_eq!(
            listed.contains(info.ffmpeg_name),
            has_encoder(&ffmpeg, info.ffmpeg_name),
            "{}",
            info.ffmpeg_name
        );
    }

    for hdr in [true, false] {
        let mut reported = Vec::new();
        let results = encoder_probe::probe_all(
            &ffmpeg,
            hdr,
            Platform::CURRENT,
            |r| reported.push(r.clone()),
            &AtomicBool::new(false),
        );
        assert_eq!(results, reported, "every result is reported as it comes");
        let offered: Vec<VideoEncoder> = encoder_profiles::for_platform(Platform::CURRENT)
            .iter()
            .map(|info| info.id)
            .collect();
        assert_eq!(
            results.iter().map(|r| r.encoder).collect::<Vec<_>>(),
            offered
        );
        for r in &results {
            let name = encoder_profiles::info(r.encoder).ffmpeg_name;
            assert_eq!(r.listed, listed.contains(name), "{name}");
            assert!(!r.message.is_empty(), "{name}");
            assert!(!is_option_error(&r.message), "{name}: {}", r.message);
            assert_eq!(r.works, r.message == "ok", "{name}: {}", r.message);
            if r.encoder == VideoEncoder::LibX265 && r.listed {
                assert!(
                    r.works,
                    "the CPU encoder must work wherever it exists: {}",
                    r.message
                );
            }
        }
        let best = encoder_probe::pick_best(&results, Platform::CURRENT);
        assert_eq!(best.is_some(), results.iter().any(|r| r.works));
        let summary: Vec<String> = results
            .iter()
            .map(|r| {
                format!(
                    "{}={}",
                    encoder_profiles::info(r.encoder).ffmpeg_name,
                    if r.works { "ok" } else { r.message.as_str() }
                )
            })
            .collect();
        note(&format!(
            "probe ({}): {}; best = {best:?}",
            range(hdr),
            summary.join(", ")
        ));
    }
}

/// A probe that hangs (a driver dialog, a stuck GPU) must not hang the app: it is killed and reported.
#[test]
fn a_run_that_outlasts_its_timeout_is_killed() {
    let Some(ffmpeg) = tool_or_skip("ffmpeg", "a_run_that_outlasts_its_timeout_is_killed") else {
        return;
    };
    let started = Instant::now();
    // -re paces the endless synthetic source at real time: this never ends on its own
    let killed = encoder_probe::run_ffmpeg(
        &ffmpeg,
        [
            "-hide_banner",
            "-nostdin",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=30",
            "-f",
            "null",
            "-",
        ],
        Duration::from_millis(1500),
    )
    .expect("ffmpeg starts");
    assert_eq!(killed.exit_code, -1);
    assert!(
        killed
            .output
            .trim_end()
            .ends_with("(timed out after 1500 ms)"),
        "{}",
        killed.output
    );
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "killing took {:?}",
        started.elapsed()
    );

    // and a run that ends by itself gives its whole output and its own exit code
    let version = encoder_probe::run_ffmpeg(
        &ffmpeg,
        ["-hide_banner", "-version"],
        Duration::from_secs(15),
    )
    .unwrap();
    assert_eq!(version.exit_code, 0);
    assert!(
        version.output.starts_with("ffmpeg version"),
        "{}",
        version.output
    );
    assert!(
        version.output.contains("libavcodec"),
        "the tail of the output is there too: {}",
        version.output
    );
    let bad = encoder_probe::run_ffmpeg(
        &ffmpeg,
        ["-hide_banner", "-i", "hdrcast-no-such-file.ts"],
        Duration::from_secs(15),
    )
    .unwrap();
    assert_ne!(bad.exit_code, 0);
    assert!(
        bad.output.contains("hdrcast-no-such-file.ts"),
        "{}",
        bad.output
    );
    note("run_ffmpeg: an endless ffmpeg was killed after its timeout; -version and a failing run reported faithfully");
}
