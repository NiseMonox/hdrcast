//! The one assertion of tests/P2pPlumbingTests.cs that is about this crate: the URL mpv is given
//! ("Receiver_settings_become_a_local_caller"), built from the options the receiver's settings turn into.
//! Also the C# SmokeTests.

use hdrcast_core::srt_url::{self, SrtRole, SrtTopology, SrtUrlOptions};

#[test]
fn a_viewer_becomes_a_local_caller() {
    // what the C# ReceiverSettings { LatencyMs = 3500 }.ToSrtOptions(40123, passphrase) produced
    // (its default expected bitrate is 100 Mbps)
    let options = SrtUrlOptions {
        topology: SrtTopology::Caller,
        role: SrtRole::Read,
        host: "127.0.0.1".to_string(),
        port: 40123,
        passphrase: Some("a77MWmBi3_603E3kH2abfOI5O-dIU5dn".to_string()),
        pb_key_len: 32,
        latency_ms: 3500,
        bitrate_bps: 100_000_000,
        rate_control: false,
        connect_timeout_ms: 5000,
        include_recv_socket_buffer: true,
        ..SrtUrlOptions::default()
    };
    let url = srt_url::build(&options).unwrap();
    assert!(url.starts_with("srt://127.0.0.1:40123?mode=caller"), "{url}");
    assert!(url.contains("latency=3500000"));
    assert!(url.contains("pbkeylen=32"));
    assert_eq!(
        url,
        "srt://127.0.0.1:40123?mode=caller&passphrase=a77MWmBi3_603E3kH2abfOI5O-dIU5dn&pbkeylen=32&pkt_size=1316\
         &transtype=live&latency=3500000&rcvbuf=96468992&ffs=65536&recv_buffer_size=8000000&connect_timeout=5000"
    );
}

/// tests/SmokeTests.cs: the crate builds and its public names are where dependants look for them.
#[test]
fn skeleton_compiles() {
    use hdrcast_core::{
        app_paths, arg_quote, base64url, config, endpoint, firewall, invite, invite_flow, json, keys, line_process,
        messages, process_job, secret_redactor, shared_file, srt_buffer_sizing, tool_locator,
    };
    let _ = (app_paths::APP_FOLDER_NAME, config::FILE_NAME, invite::PREFIX, firewall::ALLOW_VERB, messages::KIND_LIVE);
    let _ = (keys::ID_LENGTH, tool_locator::TOOLS_FOLDER_NAME, invite_flow::TITLE, secret_redactor::MASK);
    let _ = (srt_buffer_sizing::PAYLOAD_BYTES, base64url::encode(b""), arg_quote::quote(""), json::try_parse("1"));
    let _ = (endpoint::normalize(""), std::mem::size_of::<line_process::LineProcess>());
    let _ = (std::mem::size_of::<process_job::ProcessJob>(), std::mem::size_of::<shared_file::SharedGroupFile>());
}
