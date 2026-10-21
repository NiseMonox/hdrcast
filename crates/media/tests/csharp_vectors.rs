//! The Rust port against the C# it was ported from.
//!
//! `tests/vectors/csharp-0.2.4.json` holds what the 0.2.4 sources in shared/Sender and shared/Receiver produce
//! for some 950 inputs, generated on .NET Framework 4.8 (the runtime the product ran on) by
//! `tests/vectors/generator`. Every argument list, URL, parsed value and text must come out of this crate the
//! same on Windows. The few places where the port deliberately differs are spelled out in the tests below, so
//! that nothing else can differ unnoticed.

use std::collections::BTreeSet;
use std::path::Path;

use hdrcast_capture::{CaptureFormat, OutputInfo};
use hdrcast_media::receiver::mpv_command::{self, MpvLaunchPlan};
use hdrcast_media::receiver::mpv_ipc::{
    self, observed, MpvIpcMessage, MpvStatusModel, MpvVideoParams,
};
use hdrcast_media::receiver::mpv_log;
use hdrcast_media::receiver::{DisplayMode, HwDecMode, MpvExitReason, ReceiverSettings};
use hdrcast_media::sender::encoder_probe;
use hdrcast_media::sender::encoder_profiles::{self, EncoderSettings, QualityPreset, VideoEncoder};
use hdrcast_media::sender::ffmpeg_command::{self, AudioCodec, FfmpegPlan};
use hdrcast_media::sender::{FfmpegProgressParser, ScaleMode, SenderSettings};
use hdrcast_media::Platform;
use serde_json::{json, Value};

// ---- reading the vectors ----------------------------------------------------------------------------

fn section(name: &str) -> Vec<(Value, Value)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("vectors")
        .join("csharp-0.2.4.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let all: Value = serde_json::from_str(&text).expect("the vector file is JSON");
    assert!(
        all["generator"]
            .as_str()
            .unwrap()
            .contains(".NET Framework 4.8"),
        "{}",
        all["generator"]
    );
    let vectors: Vec<(Value, Value)> = all[name]
        .as_array()
        .unwrap_or_else(|| panic!("no section {name}"))
        .iter()
        .map(|pair| (pair["in"].clone(), pair["out"].clone()))
        .collect();
    assert!(!vectors.is_empty(), "section {name} is empty");
    vectors
}

fn str_of(v: &Value) -> &str {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}"))
}

fn int_of(v: &Value) -> i64 {
    v.as_i64().unwrap_or_else(|| panic!("not an integer: {v}"))
}

fn u32_of(v: &Value) -> u32 {
    u32::try_from(int_of(v)).unwrap_or_else(|_| panic!("not a u32: {v}"))
}

fn float_of(v: &Value) -> f64 {
    v.as_f64().unwrap_or_else(|| panic!("not a number: {v}"))
}

fn bool_of(v: &Value) -> bool {
    v.as_bool().unwrap_or_else(|| panic!("not a flag: {v}"))
}

fn strings_of(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap_or_else(|| panic!("not a list: {v}"))
        .iter()
        .map(|s| str_of(s).to_string())
        .collect()
}

/// JSON equality where 2 and 2.0 are the same number (the C# writers drop the fraction of a whole double).
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

fn encoder_settings(v: &Value) -> EncoderSettings {
    EncoderSettings {
        encoder: VideoEncoder::from_number(int_of(&v["encoder"])).expect("a C# encoder"),
        preset: QualityPreset::from_number(int_of(&v["preset"])).expect("a C# preset"),
        hdr: bool_of(&v["hdr"]),
        bitrate_kbps: u32_of(&v["kbps"]),
        fps: u32_of(&v["fps"]),
        gop_seconds: float_of(&v["gop"]),
        buffer_seconds: float_of(&v["buffer"]),
        b_frames: bool_of(&v["b"]),
        lookahead: bool_of(&v["la"]),
        mastering_max_nits: u32_of(&v["nits"]),
    }
}

fn capture_format(name: &str) -> CaptureFormat {
    match name {
        "p010" => CaptureFormat::P010,
        "nv12" => CaptureFormat::Nv12,
        "auto" => CaptureFormat::Auto,
        other => panic!("format {other}"),
    }
}

/// The C# built `\\.\pipe\<name>` itself; here the capture engine says where its pipes are. The same on
/// Windows, and on other hosts this keeps the comparison meaningful.
fn with_this_hosts_pipes(expected: &str, pipe_names: &[&str]) -> String {
    let mut text = expected.to_string();
    for name in pipe_names {
        text = text.replace(
            &format!(r"\\.\pipe\{name}"),
            &hdrcast_capture::pipe_path(name),
        );
    }
    text
}

// ---- sender -----------------------------------------------------------------------------------------

#[test]
fn the_catalogue_and_the_constants_are_unchanged() {
    let mut encoders = Vec::new();
    let mut suggested = 0;
    for (input, out) in section("catalogue") {
        match input.as_str() {
            Some("encoder") => {
                let info =
                    encoder_profiles::info(VideoEncoder::from_number(int_of(&out["id"])).unwrap());
                assert_eq!(info.ffmpeg_name, str_of(&out["ffmpegName"]));
                assert_eq!(info.display_name, str_of(&out["displayName"]));
                assert_eq!(info.vendor, str_of(&out["vendor"]));
                assert_eq!(info.is_hardware, bool_of(&out["isHardware"]));
                assert_eq!(info.supports_10bit, bool_of(&out["supports10Bit"]));
                assert_eq!(
                    encoder_profiles::pixel_format(info.id, true),
                    str_of(&out["pixelFormatHdr"])
                );
                assert_eq!(
                    encoder_profiles::pixel_format(info.id, false),
                    str_of(&out["pixelFormatSdr"])
                );
                // what the UI gets is the same record
                assert!(same(
                    &serde_json::to_value(info).unwrap()["ffmpegName"],
                    &out["ffmpegName"]
                ));
                encoders.push(info.id);
            }
            Some("auto_order") => {
                let order: Vec<i64> = encoder_profiles::auto_order(Platform::Windows)
                    .map(|e| i64::from(e.number()))
                    .collect();
                assert_eq!(json!(order), out);
            }
            Some("color_tags_hdr") => {
                assert_eq!(encoder_profiles::color_tags(true), strings_of(&out))
            }
            Some("color_tags_sdr") => {
                assert_eq!(encoder_profiles::color_tags(false), strings_of(&out))
            }
            Some("latency_presets") => {
                // "Fast" was added in front; the three regions are the C# ones
                let regions: Vec<Value> = ReceiverSettings::LATENCY_PRESETS[1..]
                    .iter()
                    .map(|p| json!({"name": p.name, "latencyMs": p.latency_ms, "text": p.to_string()}))
                    .collect();
                assert_eq!(json!(regions), out);
            }
            Some("tone_mapping_algorithms") => assert_eq!(
                ReceiverSettings::TONE_MAPPING_ALGORITHMS.to_vec(),
                strings_of(&out)
            ),
            Some("hwdec_values") => {
                let values: Vec<&str> = HwDecMode::ALL
                    .iter()
                    .map(|m| mpv_command::hwdec_value(*m, Platform::Windows))
                    .collect();
                assert_eq!(values, strings_of(&out));
            }
            Some("colorspace_hint_values") => {
                let values: Vec<&str> = DisplayMode::ALL
                    .iter()
                    .map(|m| mpv_command::colorspace_hint_value(*m))
                    .collect();
                assert_eq!(values, strings_of(&out));
            }
            Some("pipe_name_4321") => assert_eq!(mpv_command::pipe_name(4321), str_of(&out)),
            Some("pipe_prefix") => assert_eq!(mpv_command::PIPE_PREFIX, str_of(&out)),
            Some("ffmpeg_pipe_path_foo") => {
                if cfg!(windows) {
                    assert_eq!(ffmpeg_command::pipe_path("foo"), str_of(&out));
                }
            }
            Some("exit_reason_texts") => {
                let reasons = [
                    MpvExitReason::Unknown,
                    MpvExitReason::NoPublisher,
                    MpvExitReason::ConnectionFailed,
                    MpvExitReason::AuthFailed,
                    MpvExitReason::ProtocolUnsupported,
                    MpvExitReason::EndOfStream,
                    MpvExitReason::Quit,
                ];
                assert_eq!(reasons.map(mpv_log::describe).to_vec(), strings_of(&out));
            }
            Some("observed_properties") => {
                // two were added for remote control; the first twelve keep their ids
                let ours: Vec<Value> = observed::ALL[..12]
                    .iter()
                    .map(|(id, name)| json!([id, name]))
                    .collect();
                assert_eq!(json!(ours), out);
                assert_eq!(observed::ALL.len(), 14);
            }
            Some("observe_commands") => {
                assert_eq!(
                    observed::observe_commands().take(12).collect::<Vec<_>>(),
                    strings_of(&out)
                );
            }
            Some("file_names") => {
                let ours = [
                    SenderSettings::FILE_NAME,
                    ReceiverSettings::FILE_NAME,
                    ReceiverSettings::DEFAULT_TONE_MAPPING,
                    mpv_command::WINDOW_TITLE,
                    mpv_command::DEMUXER_MAX_BYTES,
                ];
                assert_eq!(ours.to_vec(), strings_of(&out));
            }
            None => {
                let size = &input["suggested_mbps"];
                assert_eq!(
                    i64::from(encoder_profiles::suggested_mbps(
                        u32_of(&size[0]),
                        u32_of(&size[1]),
                        u32_of(&size[2])
                    )),
                    int_of(&out),
                    "{size}"
                );
                suggested += 1;
            }
            Some(other) => panic!("unknown catalogue entry {other}"),
        }
    }
    // the four C# encoders are exactly what Windows still offers, in the same order
    let windows: Vec<VideoEncoder> = encoder_profiles::for_platform(Platform::Windows)
        .iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(encoders, windows);
    assert!(suggested >= 10);
}

#[test]
fn every_encoder_option_list_is_the_one_the_csharp_built() {
    let vectors = section("video_args");
    assert!(vectors.len() > 300);
    for (input, out) in vectors {
        assert_eq!(
            encoder_profiles::build_video_args(&encoder_settings(&input)),
            strings_of(&out),
            "{input}"
        );
    }
}

fn plan_of(v: &Value) -> FfmpegPlan {
    FfmpegPlan {
        video_pipe_name: str_of(&v["videoPipe"]).to_string(),
        audio_pipe_name: v["audioPipe"].as_str().map(str::to_string),
        pixel_format: capture_format(str_of(&v["format"])),
        width: u32_of(&v["width"]),
        height: u32_of(&v["height"]),
        fps: u32_of(&v["fps"]),
        encoder: encoder_settings(&v["encoder"]),
        audio: AudioCodec::from_number(int_of(&v["audio"])).unwrap(),
        audio_bitrate_kbps: u32_of(&v["audioKbps"]),
        output_url: str_of(&v["outputUrl"]).to_string(),
        progress: bool_of(&v["progress"]),
        progress_url: str_of(&v["progressUrl"]).to_string(),
        log_level: str_of(&v["logLevel"]).to_string(),
    }
}

#[test]
fn every_ffmpeg_command_line_is_the_one_the_csharp_built() {
    let (mut built, mut refused, mut command_lines) = (0, 0, 0);
    for (input, out) in section("ffmpeg_build") {
        let plan = plan_of(&input);
        let pipes: Vec<&str> = [
            Some(plan.video_pipe_name.as_str()),
            plan.audio_pipe_name.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        match ffmpeg_command::build(&plan) {
            Ok(args) => {
                let expected: Vec<String> = strings_of(&out["args"])
                    .iter()
                    .map(|a| with_this_hosts_pipes(a, &pipes))
                    .collect();
                assert_eq!(args, expected, "{input}");
                built += 1;
                if let Some(expected) = out["commandLine"].as_str() {
                    let line = ffmpeg_command::command_line(
                        Path::new(r"C:\Program Files\HdrCast\tools\ffmpeg.exe"),
                        &plan,
                    )
                    .unwrap();
                    assert_eq!(line, with_this_hosts_pipes(expected, &pipes), "{input}");
                    command_lines += 1;
                }
            }
            Err(error) => {
                assert_eq!(error.to_string(), str_of(&out["error"]), "{input}");
                refused += 1;
            }
        }
    }
    assert!(
        built >= 50 && refused >= 10 && command_lines >= 6,
        "{built} built, {refused} refused, {command_lines} command lines"
    );
}

#[test]
fn every_probe_command_is_the_one_the_csharp_built() {
    for (input, out) in section("probe") {
        let settings = encoder_settings(&input["encoder"]);
        let args = match input["size"].as_array() {
            None => ffmpeg_command::build_probe(&settings),
            Some(size) => {
                ffmpeg_command::build_probe_sized(&settings, u32_of(&size[0]), u32_of(&size[1]))
            }
        };
        assert_eq!(args, strings_of(&out), "{input}");
    }
}

#[test]
fn progress_is_parsed_like_the_csharp_parsed_it() {
    let mut blocks = 0;
    for (input, out) in section("progress") {
        let mut parser = FfmpegProgressParser::new();
        for (line, expected) in strings_of(&input).iter().zip(out.as_array().unwrap()) {
            let block = parser.feed(line);
            assert_eq!(block.is_some(), !expected.is_null(), "{line:?} in {input}");
            let Some(b) = block else { continue };
            let ours = json!({
                "frame": b.frame, "fps": b.fps, "bitrateKbps": b.bitrate_kbps, "totalSizeBytes": b.total_size_bytes,
                "outTimeUs": b.out_time_us, "dupFrames": b.dup_frames, "dropFrames": b.drop_frames, "speed": b.speed,
                "ended": b.ended,
            });
            assert!(
                same(&ours, expected),
                "after {line:?} in {input}:\n ours {ours}\n C#   {expected}"
            );
            blocks += 1;
        }
    }
    assert!(blocks >= 10);

    for (line, expected) in section("progress_line") {
        assert_eq!(
            FfmpegProgressParser::is_progress_line(str_of(&line)),
            bool_of(&expected),
            "{line}"
        );
    }
}

#[test]
fn the_reason_of_a_failed_probe_is_picked_like_the_csharp_picked_it() {
    for (output, expected) in section("first_error") {
        assert_eq!(
            encoder_probe::first_error(str_of(&output)),
            str_of(&expected),
            "{output}"
        );
    }
}

/// The settings as the C# serializer would write them: a string nobody set is `null` there and "" here.
fn nulls_as_empty_strings(v: &Value) -> Value {
    Value::Object(
        v.as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), if v.is_null() { json!("") } else { v.clone() }))
            .collect(),
    )
}

fn keys(v: &Value) -> Vec<&String> {
    v.as_object().unwrap().keys().collect()
}

/// Files DataContractJsonSerializer read although it would never have written them; they are unreadable here
/// (see src/settings_file.rs) and the app starts from the defaults.
const SENDER_FILES_ONLY_THE_CSHARP_READ: [&str; 14] = [
    r#"{"fps":6e1}"#,
    r#"{"fps":"60"}"#,
    r#"{"cursor":1}"#,
    r#"{"cursor":"true"}"#,
    r#"{"encoder":2.0}"#,
    r#"{"encoder":"2"}"#,
    r#"{"fps":60.0}"#,
    r#"{"fps":" 60 "}"#,
    r#"{"cursor":0}"#,
    r#"{"audioDevice":5}"#,
    r#"{"audioDevice":true}"#,
    r#"{"gopSeconds":"2.5"}"#,
    r#"{"fps":30,}"#,
    r#"{"fps": 30,"#,
];

#[test]
fn sender_settings_files_are_read_normalised_and_used_like_the_csharp_did() {
    let vectors = section("sender_settings");
    let csharp_defaults = vectors
        .iter()
        .find(|(file, _)| file == "{}")
        .map(|(_, out)| out.clone())
        .expect("the empty file");
    let mut only_csharp_read = Vec::new();
    let mut compared = 0;

    for (file, out) in &vectors {
        let file = str_of(file);
        let read = SenderSettings::from_json(file);
        let csharp_read = out.get("unreadable").is_none();
        // `[1,2,3]` is no settings file: the C# reader shrugged and returned an object of defaults
        let csharp_read_something = csharp_read && file.trim_start().starts_with('{');
        if read.is_none() && csharp_read_something {
            only_csharp_read.push(file);
            continue;
        }
        assert!(
            read.is_none() || csharp_read,
            "{file}: unreadable for the C#, read here"
        );

        // What `load` gives: the file's settings or the defaults, normalised. (Windows: the C# knew no other.)
        let raw = read.unwrap_or_else(|| SenderSettings::defaults_for(Platform::Windows));
        let out = if csharp_read { out } else { &csharp_defaults };
        // An encoder number that names nothing is already the default once read, so only the C# could complain
        // about it before normalising (and 6, which meant nothing to the C#, is VideoToolbox here).
        let without_encoder = |problems: Vec<String>| -> Vec<String> {
            problems
                .into_iter()
                .filter(|problem| problem != "Pick an encoder.")
                .collect()
        };
        assert_eq!(
            without_encoder(raw.validate_for(Platform::Windows)),
            without_encoder(strings_of(&out["validateBeforeNormalize"])),
            "{file}"
        );

        let mut s = raw;
        s.normalize_for(Platform::Windows);
        let ours = serde_json::to_value(&s).unwrap();
        let theirs: Value = serde_json::from_str(str_of(&out["normalized"])).unwrap();
        assert_eq!(
            keys(&ours),
            keys(&theirs),
            "{file}: the members and their order"
        );
        assert!(
            same(&ours, &nulls_as_empty_strings(&theirs)),
            "{file}:\n ours {ours}\n C#   {theirs}"
        );

        assert_eq!(
            s.validate_for(Platform::Windows),
            strings_of(&out["validate"]),
            "{file}"
        );
        assert_eq!(s.to_string(), str_of(&out["text"]), "{file}");
        assert_eq!(
            i64::try_from(s.bitrate_bps()).unwrap(),
            int_of(&out["bitrateBps"]),
            "{file}"
        );
        assert_eq!(
            i64::from(s.scale_height()),
            int_of(&out["scaleHeight"]),
            "{file}"
        );
        assert_eq!(
            s.resolve_format(true),
            capture_format(str_of(&out["formatOnHdrMonitor"])),
            "{file}"
        );
        assert_eq!(
            s.resolve_format(false),
            capture_format(str_of(&out["formatOnSdrMonitor"])),
            "{file}"
        );
        assert_eq!(
            encoder_profiles::build_video_args(&s.to_encoder_settings(true, 1015)),
            strings_of(&out["videoArgsHdr1015"]),
            "{file}"
        );
        assert_eq!(
            encoder_profiles::build_video_args(&s.to_encoder_settings(false, 0)),
            strings_of(&out["videoArgsSdr"]),
            "{file}"
        );
        compared += 1;
    }
    assert_eq!(only_csharp_read, SENDER_FILES_ONLY_THE_CSHARP_READ);
    assert!(compared >= 35, "{compared}");
}

#[test]
fn the_output_size_is_resolved_like_the_csharp_resolved_it() {
    for (input, out) in section("resolve_size") {
        let scale = ScaleMode::from_number(int_of(&input["scale"])).unwrap();
        let s = SenderSettings {
            scale,
            ..SenderSettings::default()
        };
        let (width, height) = s.resolve_size(
            int_of(&input["source"][0]) as i32,
            int_of(&input["source"][1]) as i32,
        );
        assert_eq!(json!([width, height]), out, "{input}");
    }
}

#[test]
fn the_capture_engine_gets_the_arguments_the_csharp_gave_it() {
    for (input, out) in section("capture_args") {
        let mut s = SenderSettings::from_json(str_of(&input["settings"])).unwrap();
        s.normalize_for(Platform::Windows);
        let output = OutputInfo {
            index: u32_of(&input["outputIndex"]),
            ..OutputInfo::default()
        };
        let args = s.to_capture_args(
            &output,
            u32_of(&input["width"]),
            u32_of(&input["height"]),
            capture_format(str_of(&input["format"])),
            str_of(&input["videoPipe"]),
            str_of(&input["audioPipe"]),
            u32_of(&input["parentPid"]),
        );
        assert_eq!(args.to_argv(), strings_of(&out), "{input}");
    }
}

// ---- receiver ---------------------------------------------------------------------------------------

#[test]
fn receiver_settings_files_are_read_and_normalised_like_the_csharp_did() {
    let vectors = section("receiver_settings");
    let csharp_defaults = vectors
        .iter()
        .find(|(file, _)| file == "{}")
        .map(|(_, out)| out.clone())
        .expect("the empty file");
    let mut compared = 0;
    for (file, out) in &vectors {
        let file = str_of(file);
        let read = ReceiverSettings::from_json(file);
        let csharp_read = out.get("unreadable").is_none();
        assert!(
            read.is_none() || csharp_read,
            "{file}: unreadable for the C#, read here"
        );
        assert!(
            read.is_some() || !csharp_read || !file.trim_start().starts_with('{'),
            "{file}: only the C# read it"
        );

        // What `load` gives: the file's settings or the defaults. (Windows: the C# knew no other. A file without
        // "frontend" gets this platform's player input, which on a Mac is the ffmpeg pipe.)
        let mut raw = read.unwrap_or_else(|| ReceiverSettings::defaults_for(Platform::Windows));
        if !file.contains("\"frontend\"") {
            raw.frontend = ReceiverSettings::defaults_for(Platform::Windows).frontend;
        }
        let out = if csharp_read { out } else { &csharp_defaults };
        assert_eq!(
            raw.validate(),
            strings_of(&out["validateBeforeNormalize"]),
            "{file}"
        );
        assert_eq!(
            i64::from(raw.overhead_percent()),
            int_of(&out["overheadBeforeNormalize"]),
            "{file}"
        );

        let s = raw.normalized();
        let ours = serde_json::to_value(&s).unwrap();
        let theirs: Value = serde_json::from_str(str_of(&out["normalized"])).unwrap();
        assert_eq!(
            keys(&ours),
            keys(&theirs),
            "{file}: the members and their order"
        );
        assert!(
            same(&ours, &nulls_as_empty_strings(&theirs)),
            "{file}:\n ours {ours}\n C#   {theirs}"
        );
        assert_eq!(s.validate(), strings_of(&out["validate"]), "{file}");
        assert_eq!(
            i64::from(s.overhead_percent()),
            int_of(&out["overheadPercent"]),
            "{file}"
        );
        assert_eq!(
            mpv_command::build_url(&s, 40123, "0123456789abcdef")
                .ok()
                .as_deref(),
            out["url"].as_str(),
            "{file}"
        );
        compared += 1;
    }
    assert!(compared >= 20, "{compared}");
}

fn mpv_plan(input: &Value) -> Result<MpvLaunchPlan, String> {
    // the settings arrive as the C# serializer wrote them: this is the file format, too
    let mut s =
        ReceiverSettings::from_json(str_of(&input["settings"])).expect("settings the C# wrote");
    if bool_of(&input["normalize"]) {
        s.normalize();
    }
    let pid = u32_of(&input["pid"]);
    let endpoint = format!(
        "{}{}",
        mpv_command::PIPE_PREFIX,
        mpv_command::pipe_name(pid)
    );
    mpv_command::build(
        &s,
        u16::try_from(int_of(&input["port"])).unwrap(),
        str_of(&input["passphrase"]),
        &endpoint,
        Path::new(str_of(&input["mpv"])),
        input["ffmpeg"].as_str().map(Path::new),
        Platform::Windows,
    )
    .map_err(|e| e.to_string())
}

#[test]
fn every_mpv_command_line_is_the_one_the_csharp_built() {
    let (mut built, mut refused, mut command_lines) = (0, 0, 0);
    for (input, out) in section("mpv") {
        match mpv_plan(&input) {
            Ok(plan) => {
                assert_eq!(plan.mpv_args, strings_of(&out["mpvArgs"]), "{input}");
                assert_eq!(plan.url, str_of(&out["url"]), "{input}");
                assert_eq!(
                    plan.ffmpeg_args,
                    out["ffmpegArgs"]
                        .as_array()
                        .map(|_| strings_of(&out["ffmpegArgs"])),
                    "{input}"
                );
                assert_eq!(
                    plan.uses_ffmpeg_pipe(),
                    bool_of(&out["usesFfmpegPipe"]),
                    "{input}"
                );
                // the plan now carries the whole endpoint; it ends in what the C# called the pipe name
                assert_eq!(
                    plan.ipc_endpoint,
                    format!(r"\\.\pipe\{}", str_of(&out["pipeName"])),
                    "{input}"
                );
                built += 1;
                if let Some(expected) = out.get("commandLine") {
                    assert_eq!(
                        plan.mpv_command_line(),
                        str_of(&out["mpvCommandLine"]),
                        "{input}"
                    );
                    assert_eq!(
                        plan.ffmpeg_command_line().as_deref(),
                        out["ffmpegCommandLine"].as_str(),
                        "{input}"
                    );
                    assert_eq!(plan.to_command_line(), str_of(expected), "{input}");
                    command_lines += 1;
                }
            }
            Err(error) => {
                assert_eq!(error, str_of(&out["error"]), "{input}");
                refused += 1;
            }
        }
    }
    assert!(
        built >= 120 && refused == 3 && command_lines >= 7,
        "{built} built, {refused} refused, {command_lines} command lines"
    );
}

#[test]
fn ipc_lines_are_understood_like_the_csharp_understood_them() {
    for (line, out) in section("ipc_message") {
        let line = str_of(&line);
        let Some(m) = MpvIpcMessage::try_parse_line(line) else {
            assert!(out.is_null(), "{line}: the C# read {out}");
            continue;
        };
        assert!(!out.is_null(), "{line}: not a message for the C#");
        let ours = json!({
            "kind": m.kind as i32, "event": m.event, "name": m.name, "id": m.id, "requestId": m.request_id, "error": m.error,
            "reason": m.reason, "isPropertyChange": m.is_property_change(), "isSuccess": m.is_success(),
            "rawKeys": m.raw.keys().collect::<Vec<_>>(),
        });
        let mut theirs = out.clone();
        let data: Value = serde_json::from_str(str_of(
            &theirs.as_object_mut().unwrap().remove("data").unwrap(),
        ))
        .unwrap();
        assert!(
            same(&ours, &theirs),
            "{line}:\n ours {ours}\n C#   {theirs}"
        );
        assert!(same(&m.data, &data), "{line}: data {} / {data}", m.data);
    }
}

#[test]
fn ipc_commands_are_written_like_the_csharp_wrote_them() {
    let mut seen = BTreeSet::new();
    for (call, out) in section("ipc_command") {
        let call = str_of(&call);
        let ours = match call {
            "observe_property 3 demuxer-cache-duration" => {
                mpv_ipc::observe_property(3, "demuxer-cache-duration")
            }
            "get_property mouse-pos 4" => mpv_ipc::get_property("mouse-pos", 4),
            "set_property fullscreen true 9" => mpv_ipc::set_property("fullscreen", true, Some(9)),
            "set_property fullscreen false" => mpv_ipc::set_property("fullscreen", false, None),
            "set_property speed 1.01" => mpv_ipc::set_property("speed", 1.01, None),
            "set_property volume 50 2" => mpv_ipc::set_property("volume", 50, Some(2)),
            "set_property title text" => {
                mpv_ipc::set_property("title", "a \"quoted\"\nline / ünï \\ tab\t", Some(3))
            }
            "set_property aid null" => mpv_ipc::set_property("aid", Value::Null, None),
            "cycle pause 5" => mpv_ipc::cycle_property("pause", Some(5)),
            "cycle pause" => mpv_ipc::cycle_property("pause", None),
            "quit" => mpv_ipc::quit(None),
            "quit 6" => mpv_ipc::quit(Some(6)),
            "command keypress MBTN_LEFT 21" => {
                mpv_ipc::command(Some(21), &[json!("keypress"), json!("MBTN_LEFT")])
            }
            "command show-text text 1500" => {
                mpv_ipc::command(None, &[json!("show-text"), json!("hello"), json!(1500)])
            }
            "command (empty)" => mpv_ipc::command(None, &[]),
            other => panic!("unknown command vector {other}"),
        };
        assert_eq!(ours, str_of(&out), "{call}");
        seen.insert(call.to_string());
    }
    assert_eq!(seen.len(), 15);
}

fn params_json(p: Option<&MpvVideoParams>) -> Value {
    match p {
        None => Value::Null,
        Some(p) => json!({
            "describe": p.describe(), "isHdr": p.is_hdr(), "pixelFormat": p.pixel_format, "width": p.width, "height": p.height,
            "colorMatrix": p.color_matrix, "primaries": p.primaries, "gamma": p.gamma, "colorLevels": p.color_levels,
            "maxLuma": p.max_luma, "minLuma": p.min_luma, "sigPeak": p.sig_peak,
        }),
    }
}

#[test]
fn the_status_and_its_texts_follow_mpv_like_the_csharp_did() {
    let vectors = section("status");
    assert!(vectors.len() > 50);
    for (lines, out) in vectors {
        let mut status = MpvStatusModel::new();
        let applied: Vec<bool> = strings_of(&lines)
            .iter()
            .map(|line| MpvIpcMessage::try_parse_line(line).is_some_and(|m| status.apply(&m)))
            .collect();
        let ours = json!({
            "applied": applied, "cacheSeconds": status.cache_seconds, "pausedForCache": status.paused_for_cache,
            "bufferingPercent": status.buffering_percent, "frameDrops": status.frame_drops,
            "decoderFrameDrops": status.decoder_frame_drops, "fps": status.fps, "hwDec": status.hw_dec,
            "source": params_json(status.source.as_ref()), "target": params_json(status.target.as_ref()),
            "videoBitrateBps": status.video_bitrate_bps, "speed": status.speed, "fullscreen": status.fullscreen,
            "updated": status.last_update_utc.is_some(),
            "describeBitrate": status.describe_bitrate(), "describeCache": status.describe_cache(),
        });
        assert!(same(&ours, &out), "{lines}:\n ours {ours}\n C#   {out}");
    }
}

#[test]
fn mpv_log_lines_are_classified_like_the_csharp_classified_them() {
    for (line, out) in section("log_line") {
        let line = str_of(&line);
        assert_eq!(
            i64::from(mpv_log::classify_line(line) as i32),
            int_of(&out["reason"]),
            "{line}"
        );
        assert_eq!(
            mpv_log::strip_urls(line),
            str_of(&out["stripped"]),
            "{line}"
        );
        assert_eq!(
            mpv_log::is_stream_unavailable(line),
            bool_of(&out["unavailable"]),
            "{line}"
        );
    }
    for (lines, out) in section("log_lines") {
        assert_eq!(
            i64::from(mpv_log::classify(strings_of(&lines)) as i32),
            int_of(&out),
            "{lines}"
        );
    }
}
