//! Port of tests/SignalIntegrationTests.cs: the client against the real Python server (the two halves of
//! docs/design.md §8.5), and everything but the GUI and the capture engine: real signaling server, real HEVC
//! Main10 PQ encoder, P2pHost, ViewerSessions, ffprobe standing in for mpv. Plus the capability flags of the
//! side channel (docs/design.md §8.9) through the same real server.
//!
//! The server is signal/hdrcast_signal.py, started on free loopback ports with whatever Python 3 is on PATH
//! ("python3", then "python"); without one these tests print a note and pass. The media tests also need ffmpeg
//! and ffprobe (HDRCAST_TOOLS, then PATH). No test here maps a port on the real router: every host and viewer
//! gets the fake router's options or has the mapping turned off.

mod common;

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hdrcast_core::group::device::{self, DeviceKey, Exchange};
use hdrcast_core::group::keys;
use hdrcast_core::group::now_ms;
use hdrcast_core::messages::{self, JoinAnswer, JoinRequest};
use hdrcast_core::{GroupConfig, Invite, LiveRecord, SignedEnvelope};
use hdrcast_p2p::upnp::PortMappingState;
use hdrcast_p2p::{
    CancelToken, JoinTicket, LinkAddresses, P2pHost, P2pHostOptions, SignalingClient, SignalingError, SwitchableStream, ViewerLink, ViewerSession,
    ViewerSessionOptions,
    ViewerSessionState, ViewerState,
};

use common::{loopback_firewalled, loopback_v4, random_keys, wait_for, FakeRouter, SignalServer};

fn new_group(server: &SignalServer, me: &str) -> GroupConfig {
    let invite = Invite::create("integration", Some(&server.url())).unwrap();
    GroupConfig::default().normalize().adopt(&invite, me)
}

fn never() -> CancelToken {
    CancelToken::never()
}

fn status_of<T: std::fmt::Debug>(result: Result<T, SignalingError>) -> u16 {
    match result {
        Err(e @ SignalingError::Failed { .. }) => e.status(),
        other => panic!("expected the server to refuse, got {other:?}"),
    }
}

// ---- SignalingClientIntegrationTests ----------------------------------------------------------------

#[test]
fn register_live_join_answer_round_trip() {
    let Some(server) = SignalServer::try_start("register_live_join_answer_round_trip") else { return };
    let host = new_group(&server, "小明");
    let viewer = GroupConfig::default().normalize().adopt(&host.to_invite().unwrap(), "小红");
    let keys = host.keys().unwrap();
    let hc = Arc::new(SignalingClient::for_group(&host).unwrap());
    let vc = Arc::new(SignalingClient::for_group(&viewer).unwrap());
    let ct = never();
    assert_eq!(hc.host(), "127.0.0.1");
    assert_eq!(hc.base_url(), server.url());

    // nobody has registered the group yet
    assert_eq!(status_of(vc.get_live(&ct)), 404);

    hc.register_group(&ct).unwrap();
    hc.register_group(&ct).unwrap(); // idempotent
    assert!(vc.get_live(&ct).unwrap().is_empty());

    let record = LiveRecord {
        host_id: host.peer_id.clone(),
        name: host.display_name.clone(),
        time_ms: now_ms(),
        since_ms: now_ms(),
        candidates: vec!["[2001:db8::1]:5000".to_string(), "192.168.1.5:5001".to_string()],
        mbps: 40,
        max_viewers: 4,
        ..LiveRecord::default()
    };
    hc.put_live(&host.peer_id, &SignedEnvelope::seal(&keys, messages::KIND_LIVE, &host.group_id, &host.peer_id, &record.to_payload()), &ct).unwrap();
    let live = vc.get_live(&ct).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].record.name, "小明");
    assert_eq!(live[0].record.candidates.len(), 2);
    assert_eq!(live[0].record, record);
    assert!((0..5000).contains(&live[0].age_ms), "{}", live[0].age_ms);
    assert!(!live[0].remote_control());

    // a record signed with the wrong key is dropped by the reader, even though the server stored it
    let fake = keys::new_id();
    let forged = SignedEnvelope::seal(
        &random_keys(),
        messages::KIND_LIVE,
        &host.group_id,
        &fake,
        &LiveRecord { host_id: fake.clone(), name: "evil".to_string(), ..LiveRecord::default() }.to_payload(),
    );
    hc.put_live(&fake, &forged, &ct).unwrap();
    assert_eq!(vc.get_live(&ct).unwrap().len(), 1);

    // join → long-poll wakes up → answer → long-poll wakes up
    let sid = keys::new_id();
    let join = JoinRequest {
        session_id: sid.clone(),
        viewer_id: viewer.peer_id.clone(),
        name: "小红".to_string(),
        time_ms: now_ms(),
        candidates: vec!["[2001:db8::2]:6000".to_string()],
        latency_ms: 3500,
        overhead_percent: 50,
        ..JoinRequest::default()
    };
    let join_envelope = SignedEnvelope::seal(&keys, messages::KIND_JOIN, &host.group_id, &host.peer_id, &join.to_payload());
    let poll = {
        let (hc, host_id) = (hc.clone(), host.peer_id.clone());
        thread::spawn(move || hc.poll_joins(&host_id, 0, 10, &never()))
    };
    thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let seq = vc.post_join(&host.peer_id, &join_envelope, &ct).unwrap();
    assert!(seq >= 1);
    let (joins, next) = poll.join().unwrap().unwrap();
    assert!(started.elapsed() < Duration::from_millis(3000), "the long-poll should wake up at once");
    assert_eq!(joins.len(), 1);
    assert_eq!((joins[0].0, next), (seq, seq));
    let opened = joins[0].1.try_open(&keys, messages::KIND_JOIN, &host.group_id, &host.peer_id).expect("the join verifies");
    assert_eq!(JoinRequest::from_payload(&opened), Some(join));
    // nothing after it: the poll comes back empty when its wait is over
    let (none, same) = hc.poll_joins(&host.peer_id, next, 0, &ct).unwrap();
    assert!(none.is_empty());
    assert_eq!(same, next);

    assert_eq!(vc.wait_answer(&sid, 0, &ct).unwrap(), None); // nothing yet
    let answer = JoinAnswer { session_id: sid.clone(), ok: true, time_ms: now_ms(), candidates: vec!["[2001:db8::1]:5000".to_string()], ..JoinAnswer::default() };
    let wait = {
        let (vc, sid) = (vc.clone(), sid.clone());
        thread::spawn(move || vc.wait_answer(&sid, 10, &never()))
    };
    thread::sleep(Duration::from_millis(300));
    hc.put_answer(&sid, &SignedEnvelope::seal(&keys, messages::KIND_ANSWER, &host.group_id, &sid, &answer.to_payload()), &ct).unwrap();
    let got = wait.join().unwrap().unwrap().expect("the answer arrives");
    let opened = got.try_open(&keys, messages::KIND_ANSWER, &host.group_id, &sid).expect("the answer verifies");
    assert!(JoinAnswer::from_payload(&opened, &sid).unwrap().ok);

    // offline: joins are refused
    hc.delete_live(&host.peer_id, &ct).unwrap();
    assert!(!vc.get_live(&ct).unwrap().iter().any(|e| e.record.host_id == host.peer_id));
    assert_eq!(status_of(vc.post_join(&host.peer_id, &join_envelope, &ct)), 404);
}

/// A full live list (32 hosts, 8 candidates each) is ~30 KB of JSON: the reader used to stop at 16 KB.
#[test]
fn a_full_live_list_is_read_completely() {
    let Some(server) = SignalServer::try_start("a_full_live_list_is_read_completely") else { return };
    let g = new_group(&server, "tester");
    let keys = g.keys().unwrap();
    let c = SignalingClient::for_group(&g).unwrap();
    let ct = never();
    c.register_group(&ct).unwrap();
    for i in 0..32 {
        let host = keys::new_id();
        let record = LiveRecord {
            host_id: host.clone(),
            name: format!("host number {i}"),
            time_ms: now_ms(),
            since_ms: now_ms(),
            mbps: 40,
            max_viewers: 4,
            candidates: (0..8).map(|k| format!("[2400:aaaa:bbbb:cccc:1122:33ff:fe44:{:x}]:{}", 0x9000 + k, 50000 + k)).collect(),
            ..LiveRecord::default()
        };
        c.put_live(&host, &SignedEnvelope::seal(&keys, messages::KIND_LIVE, &g.group_id, &host, &record.to_payload()), &ct).unwrap();
    }
    let live = c.get_live(&ct).unwrap();
    assert_eq!(live.len(), 32);
    assert!(live.iter().all(|e| e.record.candidates.len() == 8));
}

#[test]
fn wrong_keys_and_unreachable_servers_are_reported_distinctly() {
    if let Some(server) = SignalServer::try_start("wrong_keys_and_unreachable_servers_are_reported_distinctly") {
        let g = new_group(&server, "tester");
        SignalingClient::for_group(&g).unwrap().register_group(&never()).unwrap();

        // same group id, different key: the server refuses to re-register it and rejects requests
        let impostor = SignalingClient::new(&server.url(), &g.group_id, &random_keys(), 8000);
        let conflict = impostor.register_group(&never()).unwrap_err();
        assert_eq!(conflict.status(), 409);
        assert!(!conflict.is_unreachable());
        assert!(conflict.to_string().starts_with("signaling 409: "), "{conflict}");
        let denied = impostor.get_live(&never()).unwrap_err();
        assert_eq!(denied.status(), 401);
        assert!(!denied.is_unreachable() && !denied.is_cancelled());
        assert!(impostor.health(&never()), "the health check needs no key");
    }
    // nothing listens on the discard port of this PC
    let nowhere = SignalingClient::new("http://127.0.0.1:9", &keys::new_id(), &random_keys(), 1500);
    let down = nowhere.get_live(&never()).unwrap_err();
    assert!(down.is_unreachable(), "{down:?}");
    assert_eq!(down.status(), 0);
    assert!(down.to_string().starts_with("signaling server "), "{down}");
    assert!(!nowhere.health(&never()));

    let nonsense = SignalingClient::new("not a url", &keys::new_id(), &random_keys(), 1500);
    let bad = nonsense.get_live(&never()).unwrap_err();
    assert_eq!(bad.to_string(), "bad signaling address: not a url");
    assert!(bad.is_unreachable());
    assert_eq!(nonsense.host(), "");
    assert_eq!(SignalingClient::new("http://[2001:db8::5]:18650/", &keys::new_id(), &random_keys(), 1500).host(), "2001:db8::5");
}

/// Something that answers HTTP but is not the signaling server.
#[test]
fn an_answer_that_is_not_ours_is_told_apart_from_no_answer() {
    let router = FakeRouter::new(); // any HTTP server will do: this one answers 404 with an XML-free empty body
    let client = SignalingClient::new(&router.description_url().replace("/desc.xml", ""), &keys::new_id(), &random_keys(), 3000);
    let refused = client.get_live(&never()).unwrap_err();
    assert_eq!(refused.status(), 404);
    assert!(!refused.is_unreachable());
    assert_eq!(refused.to_string(), "signaling 404: Not Found", "the status text stands in for a missing error field");
    assert!(!client.health(&never()));
}

#[test]
fn cancellation_aborts_a_long_poll_quickly() {
    let Some(server) = SignalServer::try_start("cancellation_aborts_a_long_poll_quickly") else { return };
    let g = new_group(&server, "tester");
    let c = Arc::new(SignalingClient::for_group(&g).unwrap());
    c.register_group(&never()).unwrap();

    let ct = CancelToken::new();
    let started = Instant::now();
    let (done_tx, done_rx) = mpsc::channel();
    {
        let (c, ct) = (c.clone(), ct.clone());
        thread::spawn(move || {
            let _ = done_tx.send(c.wait_answer(&keys::new_id(), 20, &ct));
        });
    }
    thread::sleep(Duration::from_millis(300));
    assert!(done_rx.try_recv().is_err(), "the long-poll is still waiting");
    let cancelled_at = Instant::now();
    ct.cancel();
    let result = done_rx.recv_timeout(Duration::from_secs(5)).expect("a cancelled long-poll returns");
    assert_eq!(result, Err(SignalingError::Cancelled));
    assert!(cancelled_at.elapsed() < Duration::from_millis(1000), "returning took {:?}", cancelled_at.elapsed());
    assert!(started.elapsed() < Duration::from_millis(5000));

    // a token that is already cancelled never reaches the network
    assert_eq!(c.get_live(&ct), Err(SignalingError::Cancelled));
    // and the client is as usable as before for whoever is not cancelled
    assert!(c.get_live(&never()).unwrap().is_empty());
}

#[test]
fn the_udp_echo_reports_the_sockets_address() {
    let Some(server) = SignalServer::try_start("the_udp_echo_reports_the_sockets_address") else { return };
    let link = ViewerLink::new(&random_keys(), &loopback_v4(), 0).unwrap();
    let seen = link.discover_public_ipv4(server.echo_addr(), 2000).expect("the echo answers");
    assert_eq!(seen.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(Some(seen.port()), link.ipv4_port());

    // nobody answers on a port nothing is bound to; and an IPv6 server address is not asked at all
    let silent = std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, common::free_udp_port()));
    let started = Instant::now();
    assert_eq!(link.discover_public_ipv4(silent, 300), None);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(link.discover_public_ipv4("[::1]:9".parse().unwrap(), 300), None);
}

/// The server stores envelopes without reading them, so a payload field it has never heard of ("rc",
/// docs/design.md §8.9) goes through unchanged, and a record or a join without it reads as before.
#[test]
fn the_server_carries_the_remote_control_flag_without_reading_it() {
    let Some(server) = SignalServer::try_start("the_server_carries_the_remote_control_flag_without_reading_it") else { return };
    let host = new_group(&server, "小明");
    let keys = host.keys().unwrap();
    let c = SignalingClient::for_group(&host).unwrap();
    let ct = never();
    c.register_group(&ct).unwrap();

    let offering = LiveRecord {
        host_id: host.peer_id.clone(),
        name: host.display_name.clone(),
        time_ms: now_ms(),
        since_ms: now_ms(),
        candidates: vec!["[2001:db8::1]:5000".to_string()],
        mbps: 40,
        max_viewers: 4,
        remote_control: Some(true),
        ..LiveRecord::default()
    };
    c.put_live(&host.peer_id, &SignedEnvelope::seal(&keys, messages::KIND_LIVE, &host.group_id, &host.peer_id, &offering.to_payload()), &ct).unwrap();
    let plain_host = keys::new_id();
    let plain = LiveRecord { host_id: plain_host.clone(), name: "0.2.4".to_string(), remote_control: None, ..offering.clone() };
    c.put_live(&plain_host, &SignedEnvelope::seal(&keys, messages::KIND_LIVE, &host.group_id, &plain_host, &plain.to_payload()), &ct).unwrap();

    let live = c.get_live(&ct).unwrap();
    assert_eq!(live.len(), 2);
    let of = |id: &str| live.iter().find(|e| e.record.host_id == id).expect("listed");
    assert!(of(&host.peer_id).remote_control());
    assert_eq!(of(&host.peer_id).record, offering);
    assert!(!of(&plain_host).remote_control());
    assert_eq!(of(&plain_host).record.remote_control, None);

    let sid = keys::new_id();
    let asking = JoinRequest {
        session_id: sid,
        viewer_id: keys::new_id(),
        name: "小红".to_string(),
        time_ms: now_ms(),
        candidates: vec!["[2001:db8::2]:6000".to_string()],
        latency_ms: 1500,
        overhead_percent: 25,
        remote_control: true,
        device: None,
    };
    c.post_join(&host.peer_id, &SignedEnvelope::seal(&keys, messages::KIND_JOIN, &host.group_id, &host.peer_id, &asking.to_payload()), &ct).unwrap();
    let (joins, _) = c.poll_joins(&host.peer_id, 0, 0, &ct).unwrap();
    let opened = joins[0].1.try_open(&keys, messages::KIND_JOIN, &host.group_id, &host.peer_id).unwrap();
    assert_eq!(JoinRequest::from_payload(&opened), Some(asking));
}

// ---- P2pFullStackTests --------------------------------------------------------------------------------

type Log = Arc<Mutex<Vec<String>>>;

fn logger(log: &Log) -> impl Fn(&str) + Send + Sync + 'static {
    let log = log.clone();
    move |line| log.lock().unwrap().push(line.to_string())
}

fn dump(log: &Log) -> String {
    log.lock().unwrap().join("\n")
}

fn probe(ffprobe: &Path, session: &ViewerSession) -> String {
    let url = format!("srt://127.0.0.1:{}?passphrase={}&pbkeylen=32&latency=300000&connect_timeout=4000", session.local_port(), session.srt_passphrase());
    let mut result = String::new();
    for _attempt in 0..3 {
        result = common::run(ffprobe, &["-hide_banner", "-v", "error", "-show_entries", "stream=codec_name,profile,color_transfer", "-of", "compact", &url], 15_000).0;
        if result.contains("codec_name=") {
            break;
        }
    }
    result
}

/// The heavy tests of this file run one at a time: in parallel they starve each other of CPU, and a remux
/// child opens its SRT listener only after its first input arrives.
static MEDIA: Mutex<()> = Mutex::new(());

#[test]
fn a_friend_goes_live_and_two_friends_watch_through_the_group() {
    const TEST: &str = "a_friend_goes_live_and_two_friends_watch_through_the_group";
    let Some((ffmpeg, ffprobe)) = common::media_tools() else {
        common::skip(TEST, "needs ffmpeg and ffprobe with libx265 (set HDRCAST_TOOLS to their directory)");
        return;
    };
    if !common::has_ipv6_loopback() {
        common::skip(TEST, "this machine has no IPv6 loopback");
        return;
    }
    let _one_at_a_time = MEDIA.lock().unwrap_or_else(|e| e.into_inner());
    let Some(server) = SignalServer::try_start(TEST) else { return };
    println!("{TEST}: using {} and the signaling server on port {}", ffmpeg.display(), server.http_port);
    let router = FakeRouter::new(); // never the real router: that would forward real ports
    let invite = Invite::create("full stack", Some(&server.url())).unwrap();
    let host_group = GroupConfig::default().normalize().adopt(&invite, "小明");
    let log: Log = Arc::new(Mutex::new(Vec::new()));

    let mut encoder = common::KillOnDrop(
        Command::new(&ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=30", "-t", "40"])
        .args(["-c:v", "libx265", "-pix_fmt", "yuv420p10le"])
        .args(["-x265-params", "keyint=30:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:log-level=error"])
        .args(["-f", "mpegts", "-mpegts_flags", "+resend_headers", "-muxdelay", "0", "-muxpreload", "0", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the encoder starts"),
    );
    let ts = SwitchableStream::new();
    ts.attach(Box::new(encoder.0.stdout.take().unwrap()));

    let host = P2pHost::new(
        P2pHostOptions {
            group: host_group.clone(),
            ffmpeg_path: ffmpeg.clone().into_os_string(),
            bitrate_bps: 1_000_000,
            max_viewers: 2,
            addresses: Some(loopback_firewalled()),
            echo_port: server.echo_port,
            heartbeat_ms: 1000,
            router_mapping: Some(router.options()),
            ..P2pHostOptions::default()
        },
        Box::new(ts.clone()),
        logger(&log),
    );
    let watch = |name: &str, addresses: LinkAddresses, mapped: bool| -> ViewerSession {
        let session = ViewerSession::new(
            ViewerSessionOptions {
                group: GroupConfig::default().normalize().adopt(&invite, name),
                host_id: host_group.peer_id.clone(),
                latency_ms: 300,
                overhead_percent: 25,
                addresses: Some(addresses),
                echo_port: server.echo_port,
                map_router_port: mapped,
                router_mapping: mapped.then(|| router.options()),
                ..ViewerSessionOptions::default()
            },
            logger(&log),
        );
        session.start().unwrap();
        session
    };

    host.start().unwrap();
    assert!(wait_for(|| host.status().starts_with("Live"), 8000), "{}\n{}", host.status(), dump(&log));
    let mapping = host.router_mapping().expect("the host asks its router");
    assert!(wait_for(|| mapping.state() == PortMappingState::Mapped, 8000), "{:?} {}", mapping.state(), mapping.problem());
    let host_port = mapping.port();
    assert!(router.mapping(host_port).is_some(), "the host's port was not forwarded");
    assert!(host.has_ipv6());
    assert_eq!(host.viewers(), []);

    let red = watch("小红", loopback_firewalled(), true);
    assert!(wait_for(|| red.state() == ViewerSessionState::Connected, 20_000), "小红: {}\n{}", red.status(), dump(&log));
    let blue = watch("小蓝", loopback_firewalled(), true);
    assert!(wait_for(|| blue.state() == ViewerSessionState::Connected, 20_000), "小蓝: {}\n{}", blue.status(), dump(&log));
    for result in [probe(&ffprobe, &red), probe(&ffprobe, &blue)] {
        assert!(result.contains("codec_name=hevc"), "{result}\n{}", dump(&log));
        assert!(result.contains("profile=Main 10"), "{result}");
        assert!(result.contains("color_transfer=smpte2084"), "{result}");
    }
    assert!(red.path_description().contains("IPv6"), "{}", red.path_description());
    assert!(red.status().starts_with("Connected to 小明 (IPv6 [::1]:"), "{}", red.status());
    assert_eq!(red.host_name(), "小明");
    assert!(red.ever_connected());
    assert!(!red.this_side_could_help());
    assert!(wait_for(|| host.viewers().iter().filter(|v| v.state == ViewerState::Watching).count() == 2, 5000));
    let viewers = host.viewers();
    assert!(viewers.iter().any(|v| v.name == "小红"));
    assert_eq!(viewers.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(), ["小红", "小蓝"], "oldest first");
    // (not asserted: which family the host lists. It is the one whose PUNCH arrived first, as in 0.2.4; the
    // viewer's choice, IPv6, is where the data goes, but the listed path is not updated for it.)
    assert!(viewers.iter().all(|v| !v.path.is_empty() && v.latency_ms == 300 && v.overhead_percent == 25 && !v.wants_control));
    assert!(host.upload_mbps() > 1.5);
    assert!(wait_for(|| host.status() == "Live in \"full stack\" - 2 viewers", 5000), "{}", host.status());
    // nothing of the side channel without the flags: neither side offered or asked
    assert!(!red.host_offers_control());
    assert!(!red.send_side(b"x"));
    assert!(!host.send_side(&viewers[0].sid, b"x"));

    // a third friend is refused: max viewers is 2
    let third = watch("小绿", loopback_firewalled(), false);
    assert!(wait_for(|| third.status().contains("full"), 15_000), "{}", third.status());
    assert_eq!(third.state(), ViewerSessionState::Waiting);

    // the host stops: viewers learn it (BYE) and wait for it to come back
    host.stop();
    assert_eq!(host.status(), "Offline");
    assert!(wait_for(|| red.state() == ViewerSessionState::Waiting, 5000), "{}", red.status());
    assert_eq!(red.status(), "The stream ended - waiting for 小明 to go live again");
    assert!(router.mapping(host_port).is_none(), "the host's forward outlived it");
    assert!(host.viewers().is_empty());

    for session in [&red, &blue, &third] {
        session.stop();
        assert_eq!(session.state(), ViewerSessionState::Stopped);
    }
    assert!(router.mappings().is_empty(), "a viewer's forward outlived it: {:?}", router.mappings());
    ts.close();
    drop(encoder);

    let lines = dump(&log);
    for expected in [
        "[p2p] candidates: [::1]:",
        "[p2p] this PC offers: [::1]:",
        "[p2p] 小红 is joining (latency 300 ms, candidates [::1]:",
        "[p2p] 小红 connected via IPv",
        "[p2p] connected to 小明: IPv6 [::1]:",
        "[p2p] path to 小明 closed: the other side ended the session",
        "[p2p] router: UDP ",
    ] {
        assert!(lines.contains(expected), "no line with {expected:?} in:\n{lines}");
    }
    assert!(!lines.contains(&host_group.group_key), "the group key is in the log");
    assert!(!lines.contains(red.srt_passphrase().as_str()), "the SRT passphrase is in the log");
}

// ---- what the host and the viewer say when things go wrong ---------------------------------------------------
// Not in the C# tests (these paths were only ever seen in friends' logs): the words are what the port must keep.

/// A group on a signaling server that is not there: a closed port on this PC.
fn unreachable_group(me: &str) -> GroupConfig {
    let invite = Invite::create("nowhere", Some(&format!("http://127.0.0.1:{}", common::free_tcp_port()))).unwrap();
    GroupConfig::default().normalize().adopt(&invite, me)
}

#[test]
fn without_a_signaling_server_both_sides_say_so_keep_trying_and_stop_promptly() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let group = unreachable_group("小明");
    let ts = SwitchableStream::new();
    let host = P2pHost::new(
        P2pHostOptions {
            group: group.clone(),
            addresses: Some(loopback_v4()),
            discover_public_ipv4: false,
            map_router_port: false, // never the real router
            ..P2pHostOptions::default()
        },
        Box::new(ts.clone()),
        logger(&log),
    );
    let changes = Arc::new(Mutex::new(0usize));
    let counter = changes.clone();
    host.on_changed(move || *counter.lock().unwrap() += 1);
    assert_eq!(host.status(), "Starting");
    assert_eq!(host.ipv4_open(), None);
    assert!(host.candidates().is_empty() && !host.has_ipv6() && host.router_mapping().is_none());
    host.start().unwrap();
    assert!(matches!(host.start(), Err(hdrcast_p2p::StartError::AlreadyStarted)));
    assert!(
        wait_for(|| host.status() == "Signaling server unreachable - retrying in 3 s (viewers already watching are not affected)", 15_000),
        "{}\n{}",
        host.status(),
        dump(&log)
    );
    assert!(*changes.lock().unwrap() >= 1);
    assert_eq!(host.candidates().len(), 1, "{:?}", host.candidates());
    assert!(host.router_mapping().is_none(), "the mapping was turned off");
    let lines = dump(&log);
    assert!(lines.contains("[p2p] candidates: 127.0.0.1:") && lines.contains("  (no IPv6 on this PC)"), "{lines}");
    assert!(lines.lines().any(|l| l.starts_with("[p2p] signaling server ") && l.ends_with(" - retrying")), "{lines}");

    let viewer = ViewerSession::new(
        ViewerSessionOptions {
            group: unreachable_group("小红"),
            host_id: group.peer_id.clone(),
            addresses: Some(loopback_v4()),
            discover_public_ipv4: false,
            map_router_port: false, // never the real router
            ..ViewerSessionOptions::default()
        },
        logger(&log),
    );
    assert_eq!((viewer.state(), viewer.status().as_str(), viewer.local_port()), (ViewerSessionState::Starting, "Starting", 0));
    viewer.start().unwrap();
    assert_ne!(viewer.local_port(), 0);
    assert!(
        wait_for(|| viewer.status() == "The signaling server cannot be reached - check this PC's internet connection (retrying in 3 s)", 15_000),
        "{}\n{}",
        viewer.status(),
        dump(&log)
    );
    assert_eq!(viewer.state(), ViewerSessionState::Waiting);
    assert!(!viewer.ever_connected() && !viewer.this_side_could_help() && viewer.sid().is_none());
    assert!(dump(&log).contains("[p2p] this PC offers: 127.0.0.1:"), "{}", dump(&log));

    // both are in the middle of a retry pause or a request that cannot succeed: stopping does not wait them out
    let started = Instant::now();
    viewer.stop();
    assert!(started.elapsed() < Duration::from_millis(3000), "stopping the viewer took {:?}", started.elapsed());
    assert_eq!((viewer.state(), viewer.status().as_str()), (ViewerSessionState::Stopped, "Stopped"));
    let started = Instant::now();
    host.stop();
    assert!(started.elapsed() < Duration::from_millis(6000), "stopping the host took {:?}", started.elapsed());
    assert_eq!(host.status(), "Offline");
    host.stop(); // again is nothing
    ts.close();
}

#[test]
fn a_group_id_registered_with_another_key_is_reported_and_not_retried() {
    let Some(server) = SignalServer::try_start("a_group_id_registered_with_another_key_is_reported_and_not_retried") else { return };
    let rightful = new_group(&server, "first");
    SignalingClient::for_group(&rightful).unwrap().register_group(&never()).unwrap();
    // the same group id with another key: what an invite edited by hand would give
    let squatter_invite = Invite::new(&server.url(), &rightful.group_id, &keys::random_bytes(32), "integration").unwrap();
    let squatter = GroupConfig::default().normalize().adopt(&squatter_invite, "second");

    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let ts = SwitchableStream::new();
    let host = P2pHost::new(
        P2pHostOptions { group: squatter, addresses: Some(loopback_v4()), discover_public_ipv4: false, map_router_port: false, heartbeat_ms: 200, ..P2pHostOptions::default() },
        Box::new(ts.clone()),
        logger(&log),
    );
    host.start().unwrap();
    assert!(
        wait_for(|| host.status() == "This group's id is registered with a different key on the signaling server. Create a new group.", 8000),
        "{}\n{}",
        host.status(),
        dump(&log)
    );
    thread::sleep(Duration::from_millis(700));
    let lines = dump(&log);
    assert_eq!(lines.matches("[p2p] signaling 409: ").count(), 1, "said once, and the heartbeat ended:\n{lines}");
    host.stop();
    ts.close();
}

#[test]
fn a_viewer_is_told_when_the_host_cannot_start_ffmpeg_for_it() {
    const TEST: &str = "a_viewer_is_told_when_the_host_cannot_start_ffmpeg_for_it";
    let Some(server) = SignalServer::try_start(TEST) else { return };
    let invite = Invite::create("no ffmpeg", Some(&server.url())).unwrap();
    let host_group = GroupConfig::default().normalize().adopt(&invite, "小明");
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let ts = SwitchableStream::new();
    let host = P2pHost::new(
        P2pHostOptions {
            group: host_group.clone(),
            ffmpeg_path: std::env::temp_dir().join("hdrcast-no-such-ffmpeg").join("ffmpeg.exe").into_os_string(),
            addresses: Some(loopback_firewalled()),
            echo_port: server.echo_port,
            heartbeat_ms: 500,
            map_router_port: false, // never the real router
            ..P2pHostOptions::default()
        },
        Box::new(ts.clone()),
        logger(&log),
    );
    host.start().unwrap();
    assert!(wait_for(|| host.status().starts_with("Live"), 8000), "{}\n{}", host.status(), dump(&log));
    let viewer = ViewerSession::new(
        ViewerSessionOptions {
            group: GroupConfig::default().normalize().adopt(&invite, "小红"),
            host_id: host_group.peer_id.clone(),
            addresses: Some(loopback_firewalled()),
            echo_port: server.echo_port,
            map_router_port: false, // never the real router
            ..ViewerSessionOptions::default()
        },
        logger(&log),
    );
    viewer.start().unwrap();
    assert!(wait_for(|| viewer.status() == "小明 could not take another viewer (retrying in 3 s)", 15_000), "{}\n{}", viewer.status(), dump(&log));
    assert_eq!(viewer.state(), ViewerSessionState::Waiting);
    assert!(dump(&log).contains("[p2p] cannot start ffmpeg for 小红: "), "{}", dump(&log));
    assert!(host.viewers().is_empty());
    // nobody outside this PC could join: the only IPv4 it advertises is loopback, and the status says so
    let reach = if common::has_ipv6_loopback() {
        " (no public IPv4 found: viewers without IPv6 can only join from this network)"
    } else {
        " (no IPv6 on this PC: only IPv4 viewers on the same network can join)"
    };
    assert_eq!(host.status(), format!("Live in \"no ffmpeg\" - 0 viewers{reach}"));
    // the echo of the signaling server answered both: each advertises what it saw (here, loopback itself)
    assert!(dump(&log).contains("[p2p] public IPv4 mapping: 127.0.0.1:"), "{}", dump(&log));
    viewer.stop();
    host.stop();
    ts.close();
}

/// The remux child here is the Python interpreter, which answers ffmpeg's "-hide_banner …" with its help text
/// and exits at once: a child that keeps exiting. Six replacements a minute, then the viewer is ended.
#[test]
fn a_remux_child_that_keeps_exiting_ends_its_viewer() {
    const TEST: &str = "a_remux_child_that_keeps_exiting_ends_its_viewer";
    let Some(server) = SignalServer::try_start(TEST) else { return };
    let python = common::python().expect("the server runs on it");
    let invite = Invite::create("restarts", Some(&server.url())).unwrap();
    let host_group = GroupConfig::default().normalize().adopt(&invite, "小明");
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let ts = SwitchableStream::new();
    let host = P2pHost::new(
        P2pHostOptions {
            group: host_group.clone(),
            ffmpeg_path: python.into(),
            addresses: Some(loopback_firewalled()),
            echo_port: server.echo_port,
            heartbeat_ms: 500,
            map_router_port: false, // never the real router
            ..P2pHostOptions::default()
        },
        Box::new(ts.clone()),
        logger(&log),
    );
    host.start().unwrap();
    assert!(wait_for(|| host.status().starts_with("Live"), 8000), "{}\n{}", host.status(), dump(&log));
    let viewer = ViewerSession::new(
        ViewerSessionOptions {
            group: GroupConfig::default().normalize().adopt(&invite, "小红"),
            host_id: host_group.peer_id.clone(),
            addresses: Some(loopback_firewalled()),
            echo_port: server.echo_port,
            map_router_port: false, // never the real router
            ..ViewerSessionOptions::default()
        },
        logger(&log),
    );
    viewer.start().unwrap();
    assert!(wait_for(|| dump(&log).contains("keeps exiting"), 30_000), "{}", dump(&log));
    let lines = dump(&log);
    assert_eq!(lines.matches("[p2p] ffmpeg for 小红 keeps exiting (code ").count(), 1, "{lines}");
    assert!(lines.contains("[p2p] 小红 is joining (latency 1500 ms, candidates "), "{lines}");
    assert!(wait_for(|| dump(&log).contains("[p2p] 小红 left (the host could not serve this viewer)"), 5000), "{}", dump(&log));
    assert!(wait_for(|| host.viewers().is_empty(), 5000));
    // the viewer is told (BYE) when its path already stood, or finds out when its punching runs out
    assert!(wait_for(|| viewer.state() == ViewerSessionState::Waiting, 25_000), "{}\n{}", viewer.status(), dump(&log));
    viewer.stop();
    host.stop();
    ts.close();
}

// ---- the side channel's capability flags (docs/design.md §8.9) ----------------------------------------------

type Received<T> = Arc<Mutex<Vec<T>>>;

/// Host and viewers through the real signaling server, no encoder (a remux child without input simply waits):
/// the side channel carries datagrams only between a host that offers remote control and viewers that asked
/// for it, in the direction whose receiver said it understands them.
#[test]
fn nothing_goes_over_the_side_channel_unless_the_other_side_said_it_speaks_it() {
    const TEST: &str = "nothing_goes_over_the_side_channel_unless_the_other_side_said_it_speaks_it";
    let Some(ffmpeg) = common::find_tool("ffmpeg") else {
        common::skip(TEST, "needs ffmpeg for the remux children (set HDRCAST_TOOLS to its directory)");
        return;
    };
    let _one_at_a_time = MEDIA.lock().unwrap_or_else(|e| e.into_inner());
    let Some(server) = SignalServer::try_start(TEST) else { return };
    let invite = Invite::create("remote control", Some(&server.url())).unwrap();
    let host_group = GroupConfig::default().normalize().adopt(&invite, "小明");
    let log: Log = Arc::new(Mutex::new(Vec::new()));

    let offering = Arc::new(AtomicBool::new(true));
    let offers = offering.clone();
    let ts = SwitchableStream::new();
    let host = P2pHost::new(
        P2pHostOptions {
            group: host_group.clone(),
            ffmpeg_path: ffmpeg.into_os_string(),
            bitrate_bps: 1_000_000,
            max_viewers: 4,
            addresses: Some(loopback_firewalled()),
            echo_port: server.echo_port,
            heartbeat_ms: 500,
            map_router_port: false, // never the real router
            offer_remote_control: Some(Arc::new(move || offers.load(Ordering::SeqCst))),
            ..P2pHostOptions::default()
        },
        Box::new(ts.clone()),
        logger(&log),
    );
    let at_host: Received<(String, Vec<u8>)> = Arc::new(Mutex::new(Vec::new()));
    let sink = at_host.clone();
    host.set_side_handler(Arc::new(move |sid, payload| sink.lock().unwrap().push((sid.to_string(), payload.to_vec())))); // before start
    assert!(!host.send_side(&keys::new_id(), b"x"), "not even started");
    host.start().unwrap();
    assert!(wait_for(|| host.status().starts_with("Live"), 8000), "{}\n{}", host.status(), dump(&log));

    let watcher = SignalingClient::for_group(&GroupConfig::default().normalize().adopt(&invite, "list")).unwrap();
    let live = watcher.get_live(&never()).unwrap();
    assert_eq!(live.len(), 1);
    assert!(live[0].remote_control(), "the live list shows the offer");

    let watch = |name: &str, request_control: bool| -> (ViewerSession, Received<Vec<u8>>) {
        let session = ViewerSession::new(
            ViewerSessionOptions {
                group: GroupConfig::default().normalize().adopt(&invite, name),
                host_id: host_group.peer_id.clone(),
                latency_ms: 300,
                addresses: Some(loopback_firewalled()),
                echo_port: server.echo_port,
                map_router_port: false, // never the real router
                request_control,
                ..ViewerSessionOptions::default()
            },
            logger(&log),
        );
        assert_eq!(session.sid(), None);
        assert!(!session.host_offers_control() && !session.send_side(b"x"), "not even started");
        session.start().unwrap();
        assert!(wait_for(|| session.state() == ViewerSessionState::Connected, 20_000), "{name}: {}\n{}", session.status(), dump(&log));
        let received: Received<Vec<u8>> = Arc::new(Mutex::new(Vec::new()));
        let sink = received.clone();
        session.set_side_handler(Arc::new(move |payload| sink.lock().unwrap().push(payload.to_vec()))); // after start
        (session, received)
    };
    let sid_at_host = |name: &str| host.viewers().into_iter().find(|v| v.name == name).unwrap_or_else(|| panic!("{name} is not a viewer")).sid;
    const SETTLE: Duration = Duration::from_millis(400);

    // 1. the host offers and the viewer asks: both directions work, under the session id both sides know
    let (asking, asking_got) = watch("asks", true);
    assert!(asking.host_offers_control());
    let sid = asking.sid().expect("a connected session has an id");
    assert_eq!(sid_at_host("asks"), sid);
    assert!(host.viewers().iter().any(|v| v.name == "asks" && v.wants_control));
    assert!(asking.send_side(b"pointer"));
    assert!(wait_for(|| at_host.lock().unwrap().len() == 1, 2000), "the host's handler was not called");
    assert_eq!(at_host.lock().unwrap()[0], (sid.clone(), b"pointer".to_vec()));
    // (the host's half of the session connects a moment after the viewer's: until then there is no path)
    assert!(wait_for(|| host.send_side(&sid, b"granted"), 2000), "{:?}", host.viewers());
    assert!(wait_for(|| asking_got.lock().unwrap().len() == 1, 2000), "the viewer's handler was not called");
    assert_eq!(asking_got.lock().unwrap()[0], b"granted".to_vec());

    // 2. a viewer whose join has no "rc" (what a 0.2.4 viewer sends): the host sends it nothing
    let (plain, plain_got) = watch("plain", false);
    let plain_sid = sid_at_host("plain");
    assert!(host.viewers().iter().any(|v| v.name == "plain" && !v.wants_control));
    assert!(!host.send_side(&plain_sid, b"granted"), "sent to a viewer that never said it understands side datagrams");
    assert!(host.send_side(&sid, b"still granted"), "the other viewer is not affected");
    thread::sleep(SETTLE);
    assert!(plain_got.lock().unwrap().is_empty());
    assert_eq!(asking_got.lock().unwrap().len(), 2);

    // 3. the host stops offering: the next heartbeat's record is what 0.2.4 sends, and a viewer that joins
    //    now sends nothing
    offering.store(false, Ordering::SeqCst);
    // (asked four times a second, not a hundred: see wait_for_every)
    assert!(common::wait_for_every(250, || watcher.get_live(&never()).is_ok_and(|live| live.len() == 1 && !live[0].remote_control()), 5000), "the offer is still listed");
    assert_eq!(watcher.get_live(&never()).unwrap()[0].record.remote_control, None, "absent, not false");
    let (late, late_got) = watch("late", true);
    assert!(!late.host_offers_control());
    let before = at_host.lock().unwrap().len();
    assert!(!late.send_side(b"pointer"), "sent to a host whose live record does not offer the side channel");
    thread::sleep(SETTLE);
    let got = at_host.lock().unwrap().clone();
    assert_eq!(got.len(), before, "{got:?}");
    // (that viewer did ask, so the host may still talk to it; whether it should is the app's decision)
    assert!(host.viewers().iter().any(|v| v.name == "late" && v.wants_control));
    assert!(late_got.lock().unwrap().is_empty());

    // out of range, unknown or ended: refused before anything is sent
    assert!(!asking.send_side(&[]));
    assert!(!asking.send_side(&[0u8; 1201]));
    assert!(!host.send_side(&sid, &[]));
    assert!(!host.send_side(&keys::new_id(), b"nobody"));
    asking.stop();
    assert!(!asking.send_side(b"after stop"));
    assert!(wait_for(|| !host.viewers().iter().any(|v| v.name == "asks"), 5000), "the host still lists a viewer that left");
    assert!(!host.send_side(&sid, b"after it left"));

    host.stop();
    for session in [&plain, &late] {
        session.stop();
    }
    ts.close();
    assert!(!dump(&log).contains("keeps exiting"), "a remux child without input must simply wait:\n{}", dump(&log));
}

// ---- device keys and the admission hook (docs/design.md §8.10) ----------------------------------------------------

/// A host that decides about each join, through the real signaling server (no encoder: a remux child without
/// input simply waits). A viewer whose device signed its join is shown as waiting until the host lets it in,
/// late, and both sides then agree on a remote-control key nobody else in the group can work out. A join whose
/// signature does not hold is refused without the hook ever seeing it. A viewer without a device key (what 0.3.0
/// sends) is refused, says so, and stops trying.
#[test]
fn a_host_lets_signed_viewers_in_when_it_decides_and_they_share_a_private_control_key() {
    const TEST: &str = "a_host_lets_signed_viewers_in_when_it_decides_and_they_share_a_private_control_key";
    let Some(ffmpeg) = common::find_tool("ffmpeg") else {
        common::skip(TEST, "needs ffmpeg for the remux children (set HDRCAST_TOOLS to its directory)");
        return;
    };
    let _one_at_a_time = MEDIA.lock().unwrap_or_else(|e| e.into_inner());
    let Some(server) = SignalServer::try_start(TEST) else { return };
    let invite = Invite::create("on request", Some(&server.url())).unwrap();
    let host_group = GroupConfig::default().normalize().adopt(&invite, "desktop");
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let host_device = Arc::new(DeviceKey::generate());
    let (to_test, tickets) = mpsc::channel::<JoinTicket>();
    let to_test = Mutex::new(to_test);
    let ts = SwitchableStream::new();
    let host = P2pHost::new(
        P2pHostOptions {
            group: host_group.clone(),
            ffmpeg_path: ffmpeg.into_os_string(),
            bitrate_bps: 1_000_000,
            addresses: Some(loopback_firewalled()),
            echo_port: server.echo_port,
            heartbeat_ms: 500,
            map_router_port: false, // never the real router
            offer_remote_control: Some(Arc::new(|| true)),
            device: Some(host_device.clone()),
            admit: Some(Arc::new(move |ticket| {
                let _ = to_test.lock().unwrap().send(ticket);
            })),
            on_demand: Some(Arc::new(|| true)),
            ..P2pHostOptions::default()
        },
        Box::new(ts.clone()),
        logger(&log),
    );
    host.start().unwrap();
    assert!(wait_for(|| host.status().starts_with("Available in \"on request\""), 8000), "{}\n{}", host.status(), dump(&log));

    // the live record names the host's device and says it streams on request
    let watcher = SignalingClient::for_group(&GroupConfig::default().normalize().adopt(&invite, "list")).unwrap();
    let live = watcher.get_live(&never()).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].record.device_key.as_deref(), Some(host_device.public_key()));
    assert!(live[0].record.on_demand);

    // 1. a signed viewer waits, is let in late, and gets a key of its own
    let laptop = Arc::new(DeviceKey::generate());
    let viewer = ViewerSession::new(
        ViewerSessionOptions {
            group: GroupConfig::default().normalize().adopt(&invite, "laptop"),
            host_id: host_group.peer_id.clone(),
            latency_ms: 300,
            addresses: Some(loopback_firewalled()),
            echo_port: server.echo_port,
            map_router_port: false, // never the real router
            request_control: true,
            device: Some(laptop.clone()),
            ..ViewerSessionOptions::default()
        },
        logger(&log),
    );
    viewer.start().unwrap();
    let ticket = tickets.recv_timeout(Duration::from_secs(15)).expect("the hook was asked");
    assert_eq!(ticket.device_key.as_deref(), Some(laptop.public_key()));
    assert_eq!(ticket.name, "laptop");
    assert!(ticket.wants_control);
    assert_eq!(host.pending(), vec![ticket.clone()]);
    assert!(wait_for(|| viewer.state() == ViewerSessionState::Approval, 8000), "{}\n{}", viewer.status(), dump(&log));
    assert_eq!(viewer.status(), "Waiting for desktop to answer");
    assert!(host.viewers().is_empty(), "nobody is served before the hook decides");
    // longer than the viewer's first punches stay useful: the yes arrives after they gave up
    thread::sleep(Duration::from_secs(16));
    let host_key = host.admit(&ticket.sid).expect("let in");
    assert!(host_key.is_some(), "a signed viewer gets a key of its own");
    assert!(host.pending().is_empty());
    assert!(wait_for(|| viewer.state() == ViewerSessionState::Connected, 20_000), "{}\n{}", viewer.status(), dump(&log));
    let sid = viewer.sid().unwrap();
    assert_eq!(sid, ticket.sid);
    assert_eq!(viewer.control_key(&sid), host_key, "both sides agree");
    assert_eq!(host.control_key(&sid), host_key);
    assert_ne!(host_key, Some(host_group.keys().unwrap().control_key(&sid)), "not the key everyone in the group can derive");
    assert_eq!(viewer.control_key(&keys::new_id()), None, "only for its own session");
    assert_eq!(viewer.host_device_key().as_deref(), Some(host_device.public_key()));
    assert!(viewer.host_on_demand());
    let info = host.viewers().into_iter().find(|v| v.sid == sid).expect("listed");
    assert_eq!(info.device_key.as_deref(), Some(laptop.public_key()));
    assert!(host.admit(&sid).is_err(), "let in once");

    // 2. a join signed for another session (somebody posing as the laptop): refused, the hook never sees it
    let impostor = GroupConfig::default().normalize().adopt(&invite, "laptop");
    let mut forged = JoinRequest {
        session_id: keys::new_id(),
        viewer_id: impostor.peer_id.clone(),
        name: "laptop".into(),
        time_ms: now_ms(),
        candidates: vec!["127.0.0.1:9".into()],
        latency_ms: 300,
        overhead_percent: 25,
        remote_control: true,
        device: None,
    };
    device::sign_join(&laptop, &Exchange::new(), &impostor.group_id, &host_group.peer_id, &mut forged);
    forged.session_id = keys::new_id();
    let impostor_keys = impostor.keys().unwrap();
    let client = SignalingClient::for_group(&impostor).unwrap();
    let envelope = SignedEnvelope::seal(&impostor_keys, messages::KIND_JOIN, &impostor.group_id, &host_group.peer_id, &forged.to_payload());
    client.post_join(&host_group.peer_id, &envelope, &never()).unwrap();
    let answer = client
        .wait_answer(&forged.session_id, 10, &never())
        .unwrap()
        .and_then(|e| e.try_open(&impostor_keys, messages::KIND_ANSWER, &impostor.group_id, &forged.session_id))
        .and_then(|p| JoinAnswer::from_payload(&p, &forged.session_id))
        .expect("answered");
    assert!(!answer.ok);
    assert_eq!(answer.reason, JoinAnswer::REASON_DENIED);
    assert!(device::verify_answer(&impostor.group_id, host_device.public_key(), "", &answer), "the refusal is signed by the host's device");
    assert!(dump(&log).contains("[p2p] laptop wanted to join with a device signature that does not hold: refused"), "{}", dump(&log));

    // 3. an unsigned viewer: no device in its ticket; refused, it says so and stops trying
    let old = ViewerSession::new(
        ViewerSessionOptions {
            group: GroupConfig::default().normalize().adopt(&invite, "old"),
            host_id: host_group.peer_id.clone(),
            addresses: Some(loopback_firewalled()),
            echo_port: server.echo_port,
            map_router_port: false, // never the real router
            ..ViewerSessionOptions::default()
        },
        logger(&log),
    );
    old.start().unwrap();
    let ticket = tickets.recv_timeout(Duration::from_secs(15)).expect("the hook was asked");
    assert_eq!((ticket.name.as_str(), ticket.device_key.as_deref(), ticket.wants_control), ("old", None, false));
    assert!(host.refuse(&ticket.sid, JoinAnswer::REASON_DENIED));
    assert!(!host.refuse(&ticket.sid, JoinAnswer::REASON_DENIED), "answered once");
    assert!(wait_for(|| old.state() == ViewerSessionState::Refused, 10_000), "{}\n{}", old.status(), dump(&log));
    assert_eq!(old.status(), "desktop did not let you in");
    thread::sleep(Duration::from_secs(4));
    assert!(tickets.try_recv().is_err(), "a refused viewer does not ask again (and the forged join never asked)");
    assert_eq!(old.state(), ViewerSessionState::Refused);

    viewer.stop();
    old.stop();
    host.stop();
    ts.close();
}

/// A viewer that signs its joins, watching a host without a device key or an admission hook (what 0.3.0 is):
/// the host ignores the extra fields and lets it in at once, its answer carries no device, and the viewer
/// accepts that answer and falls back to the group's remote-control key, as 0.3.0 expects.
#[test]
fn a_signed_viewer_watches_a_host_that_knows_nothing_of_device_keys() {
    const TEST: &str = "a_signed_viewer_watches_a_host_that_knows_nothing_of_device_keys";
    let Some(ffmpeg) = common::find_tool("ffmpeg") else {
        common::skip(TEST, "needs ffmpeg for the remux children (set HDRCAST_TOOLS to its directory)");
        return;
    };
    let _one_at_a_time = MEDIA.lock().unwrap_or_else(|e| e.into_inner());
    let Some(server) = SignalServer::try_start(TEST) else { return };
    let invite = Invite::create("older host", Some(&server.url())).unwrap();
    let host_group = GroupConfig::default().normalize().adopt(&invite, "host");
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let ts = SwitchableStream::new();
    let host = P2pHost::new(
        P2pHostOptions {
            group: host_group.clone(),
            ffmpeg_path: ffmpeg.into_os_string(),
            bitrate_bps: 1_000_000,
            addresses: Some(loopback_firewalled()),
            echo_port: server.echo_port,
            heartbeat_ms: 500,
            map_router_port: false, // never the real router
            offer_remote_control: Some(Arc::new(|| true)),
            ..P2pHostOptions::default()
        },
        Box::new(ts.clone()),
        logger(&log),
    );
    host.start().unwrap();
    assert!(wait_for(|| host.status().starts_with("Live"), 8000), "{}\n{}", host.status(), dump(&log));
    let viewer = ViewerSession::new(
        ViewerSessionOptions {
            group: GroupConfig::default().normalize().adopt(&invite, "viewer"),
            host_id: host_group.peer_id.clone(),
            latency_ms: 300,
            addresses: Some(loopback_firewalled()),
            echo_port: server.echo_port,
            map_router_port: false, // never the real router
            request_control: true,
            device: Some(Arc::new(DeviceKey::generate())),
            ..ViewerSessionOptions::default()
        },
        logger(&log),
    );
    viewer.start().unwrap();
    assert!(wait_for(|| viewer.state() == ViewerSessionState::Connected, 20_000), "{}\n{}", viewer.status(), dump(&log));
    let sid = viewer.sid().unwrap();
    assert_eq!(viewer.host_device_key(), None);
    assert!(!viewer.host_on_demand());
    assert!(viewer.host_offers_control());
    assert_eq!(viewer.control_key(&sid), None, "no key of its own: the app uses the group's");
    assert_eq!(host.control_key(&sid), None);
    let info = host.viewers().into_iter().find(|v| v.sid == sid).expect("listed");
    // the signature is still checked (that needs no key of this host's own); only the exchange is left out
    assert!(info.device_key.is_some(), "the device is known all the same");
    assert!(info.wants_control);
    viewer.stop();
    host.stop();
    ts.close();
}
