//! Port of tests/SrtUrlTests.cs: SrtUrlTests, SrtBufferSizingTests, ArgQuoteTests, ToolLocatorTests.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use hdrcast_core::arg_quote;
use hdrcast_core::srt_buffer_sizing::{self, SrtBufferPlan};
use hdrcast_core::srt_url::{self, SrtRole, SrtTopology, SrtUrlOptions, SrtUrlParts};
use hdrcast_core::tool_locator::{self, Platform};

// ---- SrtUrlTests --------------------------------------------------------------------------------

/// The two URLs P2P mode builds: a viewer's remux child (listener) and mpv (caller), both on loopback.
fn child() -> SrtUrlOptions {
    SrtUrlOptions {
        topology: SrtTopology::Listener,
        role: SrtRole::Publish,
        listen_address: Some("127.0.0.1".to_string()),
        port: 40000,
        passphrase: Some("0123456789abcdef".to_string()),
        latency_ms: 1500,
        bitrate_bps: 40_000_000,
        ..SrtUrlOptions::default()
    }
}

fn mpv() -> SrtUrlOptions {
    SrtUrlOptions {
        topology: SrtTopology::Caller,
        role: SrtRole::Read,
        host: "127.0.0.1".to_string(),
        port: 40123,
        passphrase: Some("0123456789abcdef".to_string()),
        latency_ms: 1500,
        bitrate_bps: 40_000_000,
        ..SrtUrlOptions::default()
    }
}

fn built(o: &SrtUrlOptions) -> SrtUrlParts {
    srt_url::parse(&srt_url::build(o).unwrap()).unwrap()
}

#[test]
fn the_remux_child_listens_on_loopback_with_a_send_buffer() {
    let p = built(&child());
    assert_eq!(p.host, "127.0.0.1");
    assert_eq!(p.port, 40000);
    assert_eq!(p.get("mode"), Some("listener"));
    assert_eq!(p.get("listen_timeout"), Some("-1"));
    assert!(p.has("sndbuf"));
    assert!(!p.has("rcvbuf"));
    assert!(!p.has("recv_buffer_size"));
    assert!(!p.has("connect_timeout"));
    assert!(!p.has("streamid"));
}

#[test]
fn mpv_calls_loopback_with_a_receive_buffer() {
    let p = built(&mpv());
    assert_eq!(p.get("mode"), Some("caller"));
    assert_eq!(p.host, "127.0.0.1");
    assert_eq!(p.get("connect_timeout"), Some("5000"));
    assert!(p.has("rcvbuf"));
    assert_eq!(p.get("recv_buffer_size"), Some(srt_buffer_sizing::RECV_SOCKET_BUFFER_BYTES.to_string().as_str()));
}

#[test]
fn a_listener_without_an_address_binds_ipv4_everywhere() {
    let mut o = child();
    o.listen_address = None;
    assert_eq!(built(&o).host, srt_url::LISTENER_HOST);
}

#[test]
fn latency_is_expressed_in_microseconds() {
    // ffmpeg's srt protocol takes latency in microseconds (libsrt.c divides by 1000 before SRTO_LATENCY).
    assert_eq!(built(&child()).get("latency"), Some("1500000"));
}

#[test]
fn passphrase_adds_pbkeylen() {
    let p = built(&mpv());
    assert_eq!(srt_url::decode(p.get("passphrase").unwrap()), "0123456789abcdef");
    assert_eq!(p.get("pbkeylen"), Some("32"));
}

#[test]
fn no_passphrase_means_no_encryption_parameters() {
    let mut o = mpv();
    o.passphrase = None;
    let p = built(&o);
    assert!(!p.has("passphrase"));
    assert!(!p.has("pbkeylen"));
}

#[test]
fn invalid_passphrase_is_rejected() {
    let mut o = mpv();
    o.passphrase = Some("short".to_string()); // < 10 characters
    assert!(srt_url::build(&o).is_err());
    // empty means "no encryption", which is allowed
    o.passphrase = Some(String::new());
    assert!(!srt_url::build(&o).unwrap().contains("passphrase"));
}

#[test]
fn rate_control_emits_maxbw_inputbw_oheadbw() {
    let mut o = child();
    o.rate_control = true;
    o.overhead_percent = 50;
    let p = built(&o);
    assert_eq!(p.get("maxbw"), Some("0"));
    assert_eq!(p.get("inputbw"), Some((40_000_000 / 8).to_string().as_str())); // inputbw is bytes/second
    assert_eq!(p.get("oheadbw"), Some("50"));
}

#[test]
fn special_characters_in_the_passphrase_are_percent_encoded() {
    let mut o = mpv();
    o.passphrase = Some("pass&phrase=with#specials".to_string());
    let url = srt_url::build(&o).unwrap();
    assert!(!url.contains('#'));
    assert_eq!(srt_url::decode(srt_url::parse(&url).unwrap().get("passphrase").unwrap()), "pass&phrase=with#specials");
}

#[test]
fn ipv6_host_is_bracketed() {
    let mut o = mpv();
    o.host = "2001:db8::1".to_string();
    assert!(srt_url::build(&o).unwrap().starts_with("srt://[2001:db8::1]:40123?"));
}

#[test]
fn pkt_size_is_1316_so_srt_payload_is_seven_ts_packets() {
    assert_eq!(built(&child()).get("pkt_size"), Some("1316"));
    assert_eq!(srt_buffer_sizing::PAYLOAD_BYTES % 188, 0);
}

// ---- SrtBufferSizingTests -----------------------------------------------------------------------

fn plan(latency_ms: u32, bitrate_bps: u64) -> SrtBufferPlan {
    srt_buffer_sizing::compute(latency_ms, bitrate_bps).unwrap()
}

#[test]
fn buffer_covers_latency_times_bitrate() {
    // 50 Mbps for 2 s = 100 Mbit = 12.5 MB ≈ 9500 packets of 1316 bytes.
    let plan = plan(2000, 50_000_000);
    assert!((9000..=10000).contains(&plan.packets), "{}", plan.packets);
    assert!(plan.flow_window >= plan.packets, "flow window must cover the packets in flight");
    assert!(plan.buffer_bytes >= u64::from(plan.packets) * u64::from(srt_buffer_sizing::PAYLOAD_BYTES));
}

#[test]
fn flow_window_never_below_libsrt_default() {
    assert!(plan(200, 1_000_000).flow_window >= srt_buffer_sizing::DEFAULT_FLOW_WINDOW);
}

#[test]
fn buffer_bytes_stay_within_flow_window_capacity() {
    // libsrt requires SRTO_RCVBUF <= FC * (MSS - 28); violating it makes srt_setsockopt fail.
    for latency in [500, 1500, 3500] {
        for bitrate in [10_000_000, 50_000_000, 100_000_000] {
            let plan = plan(latency, bitrate);
            assert!(
                plan.buffer_bytes <= u64::from(plan.flow_window) * u64::from(srt_buffer_sizing::PACKET_SLOT_BYTES),
                "{latency} ms / {bitrate} bps exceeds the flow window capacity"
            );
        }
    }
}

#[test]
fn higher_latency_or_bitrate_needs_more_buffer() {
    assert!(plan(3000, 50_000_000).packets > plan(1500, 50_000_000).packets);
    assert!(plan(1500, 100_000_000).packets > plan(1500, 50_000_000).packets);
}

// ---- ArgQuoteTests ------------------------------------------------------------------------------

#[test]
fn quotes_windows_style() {
    let cases = [
        ("simple", "simple"),
        ("with space", "\"with space\""),
        ("", "\"\""),
        ("has\"quote", "\"has\\\"quote\""),
        (r"C:\path\to\file", r"C:\path\to\file"),
        (r"C:\path with space\", "\"C:\\path with space\\\\\""),
    ];
    for (input, expected) in cases {
        assert_eq!(arg_quote::quote(input), expected, "{input:?}");
    }
}

#[test]
fn join_separates_with_spaces() {
    assert_eq!(arg_quote::join(["a", "b c", "d"]), "a \"b c\" d");
}

#[test]
fn srt_url_with_ampersands_is_quoted_for_cmd() {
    let url = "srt://h:8890?streamid=publish:screen&latency=1500000";
    let quoted = arg_quote::quote(url);
    assert!(quoted.starts_with('"'));
    assert!(quoted.ends_with('"'));
}

// ---- ToolLocatorTests ---------------------------------------------------------------------------

/// A fake file system. Paths are compared component by component, so it does not matter which separator the
/// platform joins with (the C# version of this test compared strings and failed on Windows for that reason).
fn files(paths: &[&str]) -> HashSet<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
}

#[test]
fn prefers_app_directory_then_tools_then_path() {
    let existing = files(&["/app/tools/ffmpeg.exe", "/somewhere/ffmpeg.exe"]);
    for platform in [Platform::Windows, Platform::MacOs, Platform::Other] {
        let found = tool_locator::find_in(platform, "ffmpeg.exe", Path::new("/app"), Some("/somewhere"), |p| existing.contains(p));
        assert_eq!(found.as_deref(), Some(Path::new("/app/tools/ffmpeg.exe")), "{platform:?}");
    }
    // next to the exe beats tools/, and PATH is the last resort
    let all = files(&["/app/ffmpeg.exe", "/app/tools/ffmpeg.exe", "/somewhere/ffmpeg.exe"]);
    let found = tool_locator::find_in(Platform::CURRENT, "ffmpeg.exe", Path::new("/app"), Some("/somewhere"), |p| all.contains(p));
    assert_eq!(found.as_deref(), Some(Path::new("/app/ffmpeg.exe")));
    let on_path = files(&["/somewhere/ffmpeg.exe"]);
    let found = tool_locator::find_in(Platform::CURRENT, "ffmpeg.exe", Path::new("/app"), Some("/somewhere"), |p| on_path.contains(p));
    assert_eq!(found.as_deref(), Some(Path::new("/somewhere/ffmpeg.exe")));
}

#[test]
fn returns_none_when_nothing_exists() {
    assert_eq!(tool_locator::find_in(Platform::CURRENT, "ffmpeg.exe", Path::new("/app"), Some("/somewhere"), |_| false), None);
}

#[test]
fn candidates_start_with_the_app_directory() {
    let list = tool_locator::candidates(Platform::CURRENT, "mpv.exe", Path::new("/app"), None);
    assert!(!list.is_empty());
    assert_eq!(list[0], Path::new("/app/mpv.exe"));
}

// ---- beyond the C# tests: PATH parsing and the macOS locations ----------------------------------

#[test]
fn path_entries_are_split_per_platform_and_tidied() {
    let windows = tool_locator::candidates(Platform::Windows, "mpv.exe", Path::new(""), Some(r#"C:\a; "C:\b c" ;;D:\d"#));
    assert_eq!(windows, [PathBuf::from(r"C:\a").join("mpv.exe"), PathBuf::from(r"C:\b c").join("mpv.exe"), PathBuf::from(r"D:\d").join("mpv.exe")]);
    let unix = tool_locator::candidates(Platform::Other, "mpv", Path::new(""), Some("/usr/bin::/opt/x y/bin"));
    assert_eq!(unix, [PathBuf::from("/usr/bin/mpv"), PathBuf::from("/opt/x y/bin/mpv")]);
    assert!(tool_locator::candidates(Platform::Other, "", Path::new("/app"), Some("/usr/bin")).is_empty());
}

#[test]
fn tool_names_carry_exe_only_on_windows() {
    assert_eq!(tool_locator::exe_name_on(Platform::Windows, tool_locator::FFMPEG), "ffmpeg.exe");
    assert_eq!(tool_locator::exe_name_on(Platform::Windows, tool_locator::FFPROBE), "ffprobe.exe");
    assert_eq!(tool_locator::exe_name_on(Platform::MacOs, tool_locator::MPV), "mpv");
    assert_eq!(tool_locator::exe_name_on(Platform::Other, tool_locator::FFMPEG), "ffmpeg");
    assert_eq!(tool_locator::exe_name(tool_locator::MPV), if cfg!(windows) { "mpv.exe" } else { "mpv" });
}

#[test]
fn macos_looks_in_the_bundle_then_path_then_homebrew_then_the_mpv_app() {
    let app_dir = Path::new("/Applications/HdrCast.app/Contents/MacOS");
    let mpv = tool_locator::candidates(Platform::MacOs, "mpv", app_dir, Some("/usr/bin:/bin"));
    assert_eq!(
        mpv,
        [
            "/Applications/HdrCast.app/Contents/MacOS/mpv",
            "/Applications/HdrCast.app/Contents/MacOS/tools/mpv",
            "/Applications/HdrCast.app/Contents/Resources/tools/mpv",
            "/usr/bin/mpv",
            "/bin/mpv",
            "/opt/homebrew/bin/mpv",
            "/usr/local/bin/mpv",
            "/Applications/mpv.app/Contents/MacOS/mpv",
        ]
        .map(PathBuf::from)
    );
    // only mpv has an app of its own (ffmpeg has Homebrew's ffmpeg-full instead: see the next test)
    let ffmpeg = tool_locator::candidates(Platform::MacOs, "ffmpeg", app_dir, None);
    assert_eq!(ffmpeg.last(), Some(&PathBuf::from("/usr/local/bin/ffmpeg")));
    assert_eq!(ffmpeg.len(), 7);

    // a Homebrew ffmpeg is found even with the bare PATH an app started from Finder gets
    let brew = files(&["/opt/homebrew/bin/ffmpeg"]);
    let found = tool_locator::find_in(Platform::MacOs, "ffmpeg", app_dir, Some("/usr/bin:/bin:/usr/sbin:/sbin"), |p| brew.contains(p));
    assert_eq!(found, Some(PathBuf::from("/opt/homebrew/bin/ffmpeg")));
    // and a bundled one wins over it
    let both = files(&["/opt/homebrew/bin/ffmpeg", "/Applications/HdrCast.app/Contents/Resources/tools/ffmpeg"]);
    let found = tool_locator::find_in(Platform::MacOs, "ffmpeg", app_dir, None, |p| both.contains(p));
    assert_eq!(found, Some(PathBuf::from("/Applications/HdrCast.app/Contents/Resources/tools/ffmpeg")));

    // none of this is probed on the other platforms
    for platform in [Platform::Windows, Platform::Other] {
        assert_eq!(tool_locator::candidates(platform, "mpv", app_dir, None).len(), 2, "{platform:?}");
    }
}

#[test]
fn macos_takes_ffmpeg_from_homebrews_ffmpeg_full_before_path() {
    // Homebrew's plain ffmpeg has no libsrt; ffmpeg-full has it, but is keg-only, so it is on nobody's PATH
    let app_dir = Path::new("/Applications/HdrCast.app/Contents/MacOS");
    let ffmpeg = tool_locator::candidates(Platform::MacOs, "ffmpeg", app_dir, Some("/usr/bin"));
    assert_eq!(
        ffmpeg,
        [
            "/Applications/HdrCast.app/Contents/MacOS/ffmpeg",
            "/Applications/HdrCast.app/Contents/MacOS/tools/ffmpeg",
            "/Applications/HdrCast.app/Contents/Resources/tools/ffmpeg",
            "/opt/homebrew/opt/ffmpeg-full/bin/ffmpeg",
            "/usr/local/opt/ffmpeg-full/bin/ffmpeg",
            "/usr/bin/ffmpeg",
            "/opt/homebrew/bin/ffmpeg",
            "/usr/local/bin/ffmpeg",
        ]
        .map(PathBuf::from)
    );
    let ffprobe = tool_locator::candidates(Platform::MacOs, "ffprobe", app_dir, None);
    assert_eq!(ffprobe[3], PathBuf::from("/opt/homebrew/opt/ffmpeg-full/bin/ffprobe"));

    // with both installed ffmpeg-full is taken, even when a terminal's PATH has the plain one
    let both = files(&["/opt/homebrew/bin/ffmpeg", "/opt/homebrew/opt/ffmpeg-full/bin/ffmpeg"]);
    let found = tool_locator::find_in(Platform::MacOs, "ffmpeg", app_dir, Some("/opt/homebrew/bin:/usr/bin"), |p| both.contains(p));
    assert_eq!(found, Some(PathBuf::from("/opt/homebrew/opt/ffmpeg-full/bin/ffmpeg")));
    // and a bundled one still wins over it
    let bundled = files(&["/opt/homebrew/opt/ffmpeg-full/bin/ffmpeg", "/Applications/HdrCast.app/Contents/Resources/tools/ffmpeg"]);
    let found = tool_locator::find_in(Platform::MacOs, "ffmpeg", app_dir, None, |p| bundled.contains(p));
    assert_eq!(found, Some(PathBuf::from("/Applications/HdrCast.app/Contents/Resources/tools/ffmpeg")));

    // not for mpv, and not on the other platforms
    let has_full = |list: Vec<PathBuf>| list.iter().any(|p| p.to_string_lossy().contains("ffmpeg-full"));
    assert!(!has_full(tool_locator::candidates(Platform::MacOs, "mpv", app_dir, None)));
    assert!(!has_full(tool_locator::candidates(Platform::Windows, "ffmpeg.exe", app_dir, None)));
    assert!(!has_full(tool_locator::candidates(Platform::Other, "ffmpeg", app_dir, None)));
}
