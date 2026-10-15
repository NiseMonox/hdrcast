//! sender.json and receiver.json stay the files the C# apps wrote (DataContractJsonSerializer): camelCase
//! names, enums as numbers, a missing field = its default, unknown fields ignored. Loading never fails.

use std::path::PathBuf;

use hdrcast_media::receiver::{DisplayMode, HwDecMode, ReceiverFrontend, ReceiverSettings};
use hdrcast_media::sender::{
    AudioCodec, HdrMode, QualityPreset, ScaleMode, SenderSettings, VideoEncoder,
};
use hdrcast_media::Platform;
use serde_json::{json, Value};

/// A directory of its own under the system temp directory, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!("hdrcast-media-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn keys(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

/// A sender.json laid out as 0.2.4 wrote it, with a user's choices in it. The layout was compared with the
/// file 0.2.4 left on the development PC: two-space indent, members in [DataMember(Order)] order, whole
/// doubles without a fraction, CRLF line ends, no byte order mark.
const SENDER_0_2_4: &str = r#"{
  "outputIndex": 1,
  "outputDeviceName": "\\\\.\\DISPLAY2",
  "outputFriendlyName": "AW3225QF",
  "fps": 120,
  "scale": 1,
  "hdrMode": 1,
  "cursor": false,
  "sdrWhite": 200,
  "encoder": 5,
  "quality": 2,
  "bitrateMbps": 80,
  "gopSeconds": 1.5,
  "bufferSeconds": 2,
  "bFrames": false,
  "lookahead": true,
  "audio": true,
  "audioDevice": "{0.0.0.00000000}.{6f1b7a3c-0000-4000-8000-000000000001}",
  "audioCodec": 2,
  "audioBitrateKbps": 128,
  "audioExcludeApp": "Discord.exe",
  "maxViewers": 6,
  "uploadMbps": 1000,
  "ffmpegPath": "D:\\HdrCast-0.2.4\\tools\\ffmpeg.exe"
}"#;

const RECEIVER_0_2_4: &str = r#"{
  "latencyMs": 3500,
  "expectedBitrateMbps": 80,
  "displayMode": 2,
  "toneMapping": "spline",
  "hwDec": 2,
  "bufferSeconds": 2.5,
  "fullscreen": true,
  "fullscreenScreen": 1,
  "frontend": 1,
  "mpvPath": "D:\\HdrCast-0.2.4\\tools\\mpv.exe",
  "ffmpegPath": "",
  "autoCatchUp": false,
  "targetPeakNits": 400,
  "lastHost": "q83vEjRWeJA_dzSLwjkZeg"
}"#;

#[test]
fn a_sender_file_written_by_0_2_4_is_read_whole() {
    let dir = TempDir::new("sender-024");
    let path = dir.file(SenderSettings::FILE_NAME);
    std::fs::write(&path, SENDER_0_2_4.replace('\n', "\r\n")).unwrap();
    let s = SenderSettings::load(&path);
    // loading normalises for this machine; LibX265 exists everywhere, so nothing is repaired here
    let expected = SenderSettings {
        output_index: 1,
        output_device_name: r"\\.\DISPLAY2".to_string(),
        output_friendly_name: "AW3225QF".to_string(),
        fps: 120,
        scale: ScaleMode::H1440,
        hdr_mode: HdrMode::ForceSdr,
        cursor: false,
        sdr_white: 200.0,
        encoder: VideoEncoder::LibX265,
        quality: QualityPreset::Quality,
        bitrate_mbps: 80,
        gop_seconds: 1.5,
        buffer_seconds: 2.0,
        b_frames: false,
        lookahead: true,
        audio: true,
        audio_device: "{0.0.0.00000000}.{6f1b7a3c-0000-4000-8000-000000000001}".to_string(),
        audio_codec: AudioCodec::Opus,
        audio_bitrate_kbps: 128,
        audio_exclude_app: "Discord.exe".to_string(),
        max_viewers: 6,
        upload_mbps: 1000,
        ffmpeg_path: r"D:\HdrCast-0.2.4\tools\ffmpeg.exe".to_string(),
    };
    assert_eq!(s, expected);
}

#[test]
fn a_receiver_file_written_by_0_2_4_is_read_whole() {
    let dir = TempDir::new("receiver-024");
    let path = dir.file(ReceiverSettings::FILE_NAME);
    // Notepad's byte order mark must not matter
    std::fs::write(&path, format!("\u{feff}{RECEIVER_0_2_4}")).unwrap();
    let expected = ReceiverSettings {
        latency_ms: 3500,
        expected_bitrate_mbps: 80,
        display_mode: DisplayMode::SdrToneMap,
        tone_mapping: "spline".to_string(),
        hw_dec: HwDecMode::D3d11vaCopy,
        buffer_seconds: 2.5,
        fullscreen: true,
        fullscreen_screen: 1,
        frontend: ReceiverFrontend::FfmpegPipe,
        mpv_path: r"D:\HdrCast-0.2.4\tools\mpv.exe".to_string(),
        ffmpeg_path: String::new(),
        auto_catch_up: false,
        target_peak_nits: 400,
        last_host: "q83vEjRWeJA_dzSLwjkZeg".to_string(),
    };
    assert_eq!(ReceiverSettings::load(&path), expected);
}

#[test]
fn the_field_names_and_their_order_are_the_data_member_names() {
    let sender = serde_json::to_value(SenderSettings::defaults_for(Platform::Windows)).unwrap();
    assert_eq!(
        keys(&sender),
        keys(&serde_json::from_str::<Value>(SENDER_0_2_4).unwrap())
    );
    assert_eq!(
        keys(&sender),
        [
            "outputIndex",
            "outputDeviceName",
            "outputFriendlyName",
            "fps",
            "scale",
            "hdrMode",
            "cursor",
            "sdrWhite",
            "encoder",
            "quality",
            "bitrateMbps",
            "gopSeconds",
            "bufferSeconds",
            "bFrames",
            "lookahead",
            "audio",
            "audioDevice",
            "audioCodec",
            "audioBitrateKbps",
            "audioExcludeApp",
            "maxViewers",
            "uploadMbps",
            "ffmpegPath",
        ]
    );
    let receiver = serde_json::to_value(ReceiverSettings::default()).unwrap();
    assert_eq!(
        keys(&receiver),
        keys(&serde_json::from_str::<Value>(RECEIVER_0_2_4).unwrap())
    );
    assert_eq!(
        keys(&receiver),
        [
            "latencyMs",
            "expectedBitrateMbps",
            "displayMode",
            "toneMapping",
            "hwDec",
            "bufferSeconds",
            "fullscreen",
            "fullscreenScreen",
            "frontend",
            "mpvPath",
            "ffmpegPath",
            "autoCatchUp",
            "targetPeakNits",
            "lastHost",
        ]
    );
}

#[test]
fn enums_are_written_as_their_numbers() {
    let sender = serde_json::to_value(SenderSettings {
        scale: ScaleMode::H720,
        hdr_mode: HdrMode::ForceSdr,
        encoder: VideoEncoder::HevcVideoToolbox,
        quality: QualityPreset::Quality,
        audio_codec: AudioCodec::Opus,
        ..SenderSettings::default()
    })
    .unwrap();
    assert_eq!(
        (&sender["scale"], &sender["hdrMode"], &sender["encoder"]),
        (&json!(3), &json!(1), &json!(6))
    );
    assert_eq!(
        (&sender["quality"], &sender["audioCodec"]),
        (&json!(2), &json!(2))
    );
    // the defaults of a Windows sender, as 0.2.4 would have written them
    let defaults = serde_json::to_value(SenderSettings::defaults_for(Platform::Windows)).unwrap();
    assert_eq!(
        defaults,
        json!({
            "outputIndex": 0, "outputDeviceName": "", "outputFriendlyName": "", "fps": 60, "scale": 0, "hdrMode": 0,
            "cursor": true, "sdrWhite": 80.0, "encoder": 0, "quality": 1, "bitrateMbps": 40, "gopSeconds": 2.0,
            "bufferSeconds": 2.0, "bFrames": true, "lookahead": true, "audio": true, "audioDevice": "default",
            "audioCodec": 1, "audioBitrateKbps": 192, "audioExcludeApp": "", "maxViewers": 4, "uploadMbps": 0,
            "ffmpegPath": ""
        })
    );

    let receiver = serde_json::to_value(ReceiverSettings {
        display_mode: DisplayMode::HdrPassthrough,
        hw_dec: HwDecMode::No,
        frontend: ReceiverFrontend::FfmpegPipe,
        ..ReceiverSettings::default()
    })
    .unwrap();
    assert_eq!(
        (
            &receiver["displayMode"],
            &receiver["hwDec"],
            &receiver["frontend"]
        ),
        (&json!(1), &json!(3), &json!(1))
    );
    assert_eq!(
        serde_json::to_value(ReceiverSettings::default()).unwrap(),
        json!({
            "latencyMs": 1500, "expectedBitrateMbps": 100, "displayMode": 0, "toneMapping": "bt.2390", "hwDec": 1,
            "bufferSeconds": 1.5, "fullscreen": false, "fullscreenScreen": 0, "frontend": 0, "mpvPath": "",
            "ffmpegPath": "", "autoCatchUp": true, "targetPeakNits": 0, "lastHost": ""
        })
    );
}

#[test]
fn missing_fields_are_defaults_and_unknown_ones_are_ignored() {
    let s: SenderSettings =
        serde_json::from_str(r#"{"bitrateMbps":25,"somethingNew":{"a":[1,2]},"mode":1}"#).unwrap();
    assert_eq!(
        s,
        SenderSettings {
            bitrate_mbps: 25,
            ..SenderSettings::default()
        }
    );
    assert_eq!(
        serde_json::from_str::<SenderSettings>("{}").unwrap(),
        SenderSettings::default()
    );

    let r: ReceiverSettings =
        serde_json::from_str(r#"{"hwDec":3,"host":"relay","password":"pw"}"#).unwrap();
    assert_eq!(
        r,
        ReceiverSettings {
            hw_dec: HwDecMode::No,
            ..ReceiverSettings::default()
        }
    );
    assert_eq!(
        serde_json::from_str::<ReceiverSettings>("{}").unwrap(),
        ReceiverSettings::default()
    );
}

#[test]
fn numbers_that_name_no_enum_value_do_not_cost_the_other_settings() {
    let s: SenderSettings = serde_json::from_str(
        r#"{"scale":9,"hdrMode":-1,"quality":7,"audioCodec":5,"encoder":3,"fps":30}"#,
    )
    .unwrap();
    assert_eq!(
        (s.scale, s.hdr_mode, s.quality, s.audio_codec),
        (
            ScaleMode::Native,
            HdrMode::Auto,
            QualityPreset::Balanced,
            AudioCodec::Aac
        )
    );
    assert_eq!(s.fps, 30);
    assert_eq!(s.normalized().encoder, SenderSettings::default().encoder);

    let r: ReceiverSettings =
        serde_json::from_str(r#"{"displayMode":9,"hwDec":42,"frontend":2,"latencyMs":2000}"#)
            .unwrap();
    assert_eq!(
        (r.display_mode, r.hw_dec, r.frontend),
        (
            DisplayMode::Auto,
            HwDecMode::D3d11va,
            ReceiverFrontend::MpvDirect
        )
    );
    assert_eq!(r.latency_ms, 2000);
}

#[test]
fn null_strings_are_repaired_like_the_csharp_normalize_did() {
    let s: SenderSettings = serde_json::from_str(
        r#"{"outputDeviceName":null,"outputFriendlyName":null,"audioDevice":null,"audioExcludeApp":null,"ffmpegPath":null,"fps":30}"#,
    )
    .unwrap();
    let s = s.normalized();
    assert_eq!(
        (
            s.audio_device.as_str(),
            s.audio_exclude_app.as_str(),
            s.ffmpeg_path.as_str(),
            s.fps
        ),
        ("default", "", "", 30)
    );

    let r: ReceiverSettings = serde_json::from_str(
        r#"{"toneMapping":null,"mpvPath":null,"ffmpegPath":null,"lastHost":null,"latencyMs":2000}"#,
    )
    .unwrap();
    let r = r.normalized();
    assert_eq!(
        (
            r.tone_mapping.as_str(),
            r.mpv_path.as_str(),
            r.last_host.as_str(),
            r.latency_ms
        ),
        ("bt.2390", "", "", 2000)
    );
}

#[test]
fn loading_never_fails() {
    let dir = TempDir::new("corrupt");
    let sender = dir.file("sender.json");
    let receiver = dir.file("receiver.json");
    // missing
    assert_eq!(SenderSettings::load(&sender), SenderSettings::default());
    assert_eq!(
        ReceiverSettings::load(&receiver),
        ReceiverSettings::default()
    );
    // corrupt, empty, the wrong shape, a field of the wrong type: the defaults, like the C# apps
    for text in [
        "{\"fps\": 30,",
        "",
        "null",
        "[1,2,3]",
        "\"sender\"",
        r#"{"fps":"sixty"}"#,
        r#"{"latencyMs":"x","fps":[]}"#,
        "\u{0}\u{1}garbage",
    ] {
        std::fs::write(&sender, text).unwrap();
        std::fs::write(&receiver, text).unwrap();
        assert_eq!(
            SenderSettings::load(&sender),
            SenderSettings::default(),
            "{text:?}"
        );
        assert_eq!(
            ReceiverSettings::load(&receiver),
            ReceiverSettings::default(),
            "{text:?}"
        );
    }
    // a directory where the file should be
    std::fs::remove_file(&sender).unwrap();
    std::fs::create_dir(&sender).unwrap();
    assert_eq!(SenderSettings::load(&sender), SenderSettings::default());
}

#[test]
fn loading_normalises_what_it_read() {
    let dir = TempDir::new("normalise");
    let sender = dir.file("sender.json");
    std::fs::write(&sender, r#"{"fps":0,"bitrateMbps":9999,"maxViewers":99,"audioExcludeApp":"  discord ","encoder":1}"#).unwrap();
    let s = SenderSettings::load(&sender);
    assert_eq!(
        (
            s.fps,
            s.bitrate_mbps,
            s.max_viewers,
            s.audio_exclude_app.as_str()
        ),
        (60, 500, 16, "discord.exe")
    );
    assert_eq!(s.encoder, SenderSettings::default().encoder);

    let receiver = dir.file("receiver.json");
    std::fs::write(
        &receiver,
        r#"{"latencyMs":5,"targetPeakNits":7,"bufferSeconds":0,"lastHost":" abc "}"#,
    )
    .unwrap();
    let r = ReceiverSettings::load(&receiver);
    assert_eq!(
        (
            r.latency_ms,
            r.target_peak_nits,
            r.buffer_seconds,
            r.last_host.as_str()
        ),
        (1500, 0, 1.5, "abc")
    );
}

#[test]
fn saving_is_atomic_and_round_trips() {
    let dir = TempDir::new("save");
    let sender_path = dir.file("nested").join("sender.json"); // the folder is created
    let sender = SenderSettings {
        output_friendly_name: "显示器 ディスプレイ".to_string(),
        encoder: VideoEncoder::LibX265,
        sdr_white: 203.5,
        bitrate_mbps: 60,
        audio_exclude_app: "Discord.exe".to_string(),
        ..SenderSettings::default()
    };
    sender.save(&sender_path).unwrap();
    sender.save(&sender_path).unwrap(); // replacing an existing file works too
    assert_eq!(SenderSettings::load(&sender_path), sender);
    let text = std::fs::read_to_string(&sender_path).unwrap();
    assert!(
        text.contains("\n  \"encoder\": 5,"),
        "indented, enums as numbers: {text}"
    );
    assert!(
        text.contains("显示器 ディスプレイ"),
        "non-ASCII written as is: {text}"
    );

    let receiver_path = dir.file("receiver.json");
    let receiver = ReceiverSettings {
        latency_ms: 300,
        display_mode: DisplayMode::HdrPassthrough,
        hw_dec: HwDecMode::AutoSafe,
        target_peak_nits: 1000,
        last_host: "q83vEjRWeJA_dzSLwjkZeg".to_string(),
        ..ReceiverSettings::default()
    };
    receiver.save(&receiver_path).unwrap();
    assert_eq!(ReceiverSettings::load(&receiver_path), receiver);

    // no temp file is left behind
    let left: Vec<String> = std::fs::read_dir(&dir.0)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(left.iter().all(|name| !name.ends_with(".tmp")), "{left:?}");
    assert!(SenderSettings::default_path()
        .ends_with(std::path::Path::new("HdrCast").join("sender.json")));
    assert_eq!(
        ReceiverSettings::default_path().file_name().unwrap(),
        "receiver.json"
    );
}

#[test]
fn the_ui_can_send_the_structs_back_as_json() {
    // what a web view would post: every field, numbers for enums, whole floats as integers
    let posted = json!({
        "latencyMs": 2000, "expectedBitrateMbps": 60, "displayMode": 1, "toneMapping": "hable", "hwDec": 0,
        "bufferSeconds": 2, "fullscreen": true, "fullscreenScreen": 0, "frontend": 0, "mpvPath": "", "ffmpegPath": "",
        "autoCatchUp": true, "targetPeakNits": 0, "lastHost": ""
    });
    let r: ReceiverSettings = serde_json::from_value(posted).unwrap();
    assert_eq!(
        (r.latency_ms, r.display_mode, r.hw_dec, r.buffer_seconds),
        (2000, DisplayMode::HdrPassthrough, HwDecMode::AutoSafe, 2.0)
    );
    let back: ReceiverSettings = serde_json::from_value(serde_json::to_value(&r).unwrap()).unwrap();
    assert_eq!(back, r);

    let s = SenderSettings {
        encoder: VideoEncoder::HevcQsv,
        scale: ScaleMode::H1080,
        ..SenderSettings::default()
    };
    let back: SenderSettings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(back, s);
}
