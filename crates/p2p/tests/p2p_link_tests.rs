//! Port of tests/P2pLinkTests.cs: ControlPacketTests, PeerSessionTests, LocalAddressTests, P2pLinkSocketTests;
//! plus the echo entries of the golden vectors.

mod common;

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use hdrcast_core::endpoint;
use hdrcast_core::group::keys;
use hdrcast_p2p::control_packet::{self, ControlType, PeerRole};
use hdrcast_p2p::local_addresses::{self, AddressCandidate};
use hdrcast_p2p::peer_session::{PUNCH_DEADLINE_US, SILENCE_TIMEOUT_US};
use hdrcast_p2p::{echo_protocol, HostLink, HostSessionInfo, LinkAddresses, LinkState, Outgoing, PeerSession, ViewerLink};

use common::{hex, loopback, loopback_v4, random_keys, round_trip, unhex, vectors, wait_for, EchoChild, StatefulFirewall};

// ---- ControlPacketTests -----------------------------------------------------------------------------

#[test]
fn packets_match_the_reference_bytes() {
    let keys = vectors::keys();
    let sid = vectors::s("sid");
    let sid_bytes = keys::id_bytes_of(&sid).unwrap();
    let punch = control_packet::build(ControlType::Punch, PeerRole::Viewer, &sid_bytes, 123456789, 0, &keys.punch_key(&sid));
    assert_eq!(hex(&punch), vectors::s("punchHex"));
    let ack = control_packet::build(ControlType::PunchAck, PeerRole::Host, &sid_bytes, 123999999, 123456789, &keys.punch_key(&sid));
    assert_eq!(hex(&ack), vectors::s("punchAckHex"));
}

#[test]
fn fields_read_back_and_tampering_is_caught() {
    let keys = vectors::keys();
    let sid = vectors::s("sid");
    let p = unhex(&vectors::s("punchAckHex"));
    assert!(control_packet::looks_like_control(&p));
    assert_eq!(control_packet::type_of(&p), Some(ControlType::PunchAck));
    assert_eq!(control_packet::role_of(&p), Some(PeerRole::Host));
    assert_eq!(control_packet::sid_of(&p), sid);
    assert_eq!(control_packet::ts_of(&p), 123999999);
    assert_eq!(control_packet::echo_of(&p), 123456789);
    assert!(control_packet::verify_mac(&p, &keys.punch_key(&sid)));
    assert!(!control_packet::verify_mac(&p, &keys.punch_key(&keys::new_id())));

    let mut t = p.clone();
    t[30] ^= 1;
    assert!(!control_packet::verify_mac(&t, &keys.punch_key(&sid)));
    assert!(!control_packet::looks_like_control(&p[..57]));
    assert!(!control_packet::looks_like_control(&[0u8; 58])); // SRT of the same length
}

/// The "echo" entries of tests/vectors/group-v1.json, which the signaling server's tests read too.
#[test]
fn the_udp_echo_matches_the_reference_bytes() {
    let request = unhex(json_str(&["echo", "requestHex"]).as_str());
    let nonce: [u8; 16] = request[8..].try_into().unwrap();
    assert_eq!(nonce, *b"0123456789:;<=>?");
    assert_eq!(echo_protocol::build_request(&nonce).to_vec(), request);

    let v4: SocketAddr = "203.0.113.9:40001".parse().unwrap();
    let reply_v4 = unhex(&json_str(&["echo", "replyV4Hex"]));
    assert_eq!(echo_protocol::build_reply(&nonce, v4), reply_v4);
    assert!(echo_protocol::looks_like_reply(&reply_v4));
    assert_eq!(echo_protocol::parse_reply(&reply_v4, &nonce), Some(v4));

    let v6: SocketAddr = "[2001:db8::9]:40002".parse().unwrap();
    let reply_v6 = unhex(&json_str(&["echo", "replyV6Hex"]));
    assert_eq!(echo_protocol::build_reply(&nonce, v6), reply_v6);
    assert_eq!(echo_protocol::parse_reply(&reply_v6, &nonce), Some(v6));

    assert_eq!(echo_protocol::parse_reply(&reply_v4, &[0u8; 16]), None, "an answer to somebody else's question");
    assert!(!echo_protocol::looks_like_reply(&request));
}

fn json_str(path: &[&str]) -> String {
    vectors::get(path).as_str().expect("a string in the vectors").to_string()
}

// ---- PeerSessionTests -------------------------------------------------------------------------------
// The state machine on a fake clock, packets carried by an in-memory network with stateful firewalls.

fn addr(text: &str) -> SocketAddr {
    text.parse().unwrap()
}

fn h6() -> SocketAddr {
    addr("[2400::1]:5000")
}
fn h4() -> SocketAddr {
    addr("198.51.100.1:5001")
}
fn v6() -> SocketAddr {
    addr("[2400::2]:6000")
}
fn v4() -> SocketAddr {
    addr("198.51.100.2:6001")
}

#[derive(Default)]
struct Net {
    /// (from, to) pinholes: outbound seen.
    opened: HashSet<(SocketAddr, SocketAddr)>,
    block_v6: bool,
    block_v4: bool,
    delivered: usize,
}

impl Net {
    fn pass(&mut self, from: SocketAddr, to: SocketAddr) -> bool {
        if (self.block_v6 && from.is_ipv6()) || (self.block_v4 && from.is_ipv4()) {
            return false;
        }
        self.opened.insert((from, to)); // the sender's firewall opens for replies
        self.opened.contains(&(to, from)) // the receiver's firewall needs its own outbound first
    }
}

fn pair(now: i64) -> (PeerSession, PeerSession) {
    let keys = vectors::keys();
    let sid = keys::new_id();
    let host = PeerSession::new(&keys, &sid, PeerRole::Host, now).unwrap();
    let viewer = PeerSession::new(&keys, &sid, PeerRole::Viewer, now).unwrap();
    host.add_remote_candidates(&[endpoint::format(&v6()), endpoint::format(&v4())]);
    viewer.add_remote_candidates(&[endpoint::format(&h6()), endpoint::format(&h4())]);
    (host, viewer)
}

fn source_for(to: SocketAddr, from_host: bool) -> SocketAddr {
    match (to.is_ipv6(), from_host) {
        (true, true) => h6(),
        (true, false) => v6(),
        (false, true) => h4(),
        (false, false) => v4(),
    }
}

struct InFlight {
    data: [u8; control_packet::LENGTH],
    from: SocketAddr,
    to: SocketAddr,
    to_host: bool,
}

/// Runs both sides for `duration_us` in 10 ms steps; returns the time it stopped at.
fn run(host: &PeerSession, viewer: &PeerSession, net: &mut Net, mut now: i64, duration_us: i64, stop: impl Fn() -> bool) -> i64 {
    let end = now + duration_us;
    let mut in_flight: Vec<InFlight> = Vec::new();
    while now < end {
        in_flight.extend(host.tick(now).into_iter().map(|o| InFlight { data: o.data, from: source_for(o.to, true), to: o.to, to_host: false }));
        in_flight.extend(viewer.tick(now).into_iter().map(|o| InFlight { data: o.data, from: source_for(o.to, false), to: o.to, to_host: true }));
        for packet in std::mem::take(&mut in_flight) {
            if !net.pass(packet.from, packet.to) {
                continue;
            }
            net.delivered += 1;
            let target = if packet.to_host { host } else { viewer };
            for reply in target.on_control(&packet.data, packet.from, now) {
                in_flight.push(InFlight { data: reply.data, from: source_for(reply.to, packet.to_host), to: reply.to, to_host: !packet.to_host });
            }
        }
        if stop() {
            break;
        }
        now += 10_000;
    }
    now
}

fn both_connected<'a>(host: &'a PeerSession, viewer: &'a PeerSession) -> impl Fn() -> bool + 'a {
    move || host.state() == LinkState::Connected && viewer.state() == LinkState::Connected
}

#[test]
fn both_firewalls_open_and_the_viewer_prefers_ipv6() {
    let (host, viewer) = pair(1_000_000);
    let mut net = Net::default();
    run(&host, &viewer, &mut net, 1_000_000, 3_000_000, both_connected(&host, &viewer));
    assert_eq!(viewer.state(), LinkState::Connected);
    assert_eq!(host.state(), LinkState::Connected);
    assert_eq!(viewer.selected(), Some(h6()));
    assert!(viewer.rtt_ms() >= 0.0);
    assert!(viewer.is_confirmed(h6()));
    assert!(host.is_confirmed(v6()));
}

#[test]
fn ipv4_is_used_when_ipv6_cannot_get_through() {
    let (host, viewer) = pair(0);
    let mut net = Net { block_v6: true, ..Net::default() };
    run(&host, &viewer, &mut net, 0, 3_000_000, both_connected(&host, &viewer));
    assert_eq!(viewer.selected(), Some(h4()));
    assert_eq!(host.state(), LinkState::Connected);
}

#[test]
fn an_ipv4_answer_waits_briefly_for_ipv6() {
    let keys = vectors::keys();
    let sid = keys::new_id();
    let viewer = PeerSession::new(&keys, &sid, PeerRole::Viewer, 0).unwrap();
    viewer.add_remote_candidates(&[endpoint::format(&h6()), endpoint::format(&h4())]);
    let ack = control_packet::build(ControlType::PunchAck, PeerRole::Host, &keys::id_bytes_of(&sid).unwrap(), 5, 0, &keys.punch_key(&sid));
    viewer.on_control(&ack, h4(), 100_000);
    viewer.tick(150_000);
    assert_eq!(viewer.selected(), None); // still inside the IPv6 grace window
    viewer.on_control(&ack, h6(), 250_000);
    assert_eq!(viewer.selected(), Some(h6()));

    let viewer2 = PeerSession::new(&keys, &sid, PeerRole::Viewer, 0).unwrap();
    viewer2.on_control(&ack, h4(), 100_000);
    viewer2.tick(320_000);
    assert_eq!(viewer2.selected(), Some(h4())); // no IPv6 answer: IPv4 it is
    assert_eq!(viewer2.state(), LinkState::Connected);
}

#[test]
fn nothing_answering_fails_after_the_deadline_with_a_reason() {
    let (host, viewer) = pair(0);
    let mut net = Net { block_v4: true, block_v6: true, ..Net::default() };
    run(&host, &viewer, &mut net, 0, PUNCH_DEADLINE_US + 500_000, || false);
    assert_eq!(viewer.state(), LinkState::Failed);
    assert_eq!(host.state(), LinkState::Failed);
    assert!(viewer.reason().contains("no candidate"), "{}", viewer.reason());
}

#[test]
fn keepalives_flow_while_idle_and_silence_closes() {
    let (host, viewer) = pair(0);
    let mut net = Net::default();
    let mut now = run(&host, &viewer, &mut net, 0, 2_000_000, both_connected(&host, &viewer));
    let before = net.delivered;
    now = run(&host, &viewer, &mut net, now, 6_000_000, || false);
    assert!(net.delivered - before >= 4, "expected keepalives, got {}", net.delivered - before);
    assert_eq!(viewer.state(), LinkState::Connected);

    // the host vanishes: the viewer gives up after the silence timeout
    let mut t = now;
    while t < now + SILENCE_TIMEOUT_US + 200_000 {
        viewer.tick(t);
        t += 100_000;
    }
    assert_eq!(viewer.state(), LinkState::Closed);
    assert!(viewer.reason().contains("silent"), "{}", viewer.reason());
}

#[test]
fn bye_closes_the_other_side() {
    let (host, viewer) = pair(0);
    let mut net = Net::default();
    let now = run(&host, &viewer, &mut net, 0, 2_000_000, both_connected(&host, &viewer));
    let byes: Vec<Outgoing> = host.close_with_bye("stop", now);
    assert_eq!(byes.iter().filter(|o| o.to == v6()).count(), 3, "a BYE is sent three times");
    for bye in byes.iter().filter(|o| o.to == v6()) {
        viewer.on_control(&bye.data, h6(), now);
    }
    assert_eq!(viewer.state(), LinkState::Closed);
    assert_eq!(host.state(), LinkState::Closed);
    assert_eq!(host.reason(), "stop");
    assert_eq!(viewer.reason(), "the other side ended the session");
}

#[test]
fn data_is_accepted_only_from_confirmed_addresses_and_the_host_follows_it() {
    let (host, viewer) = pair(0);
    let mut net = Net::default();
    let now = run(&host, &viewer, &mut net, 0, 2_000_000, both_connected(&host, &viewer));
    assert!(!host.on_data(addr("[2400::99]:6000"), now));
    assert!(host.on_data(v6(), now));
    assert_eq!(host.selected(), Some(v6()));
    assert!(host.on_data(v4(), now)); // V4 punched too: the viewer may switch paths
    assert_eq!(host.selected(), Some(v4()));
}

// ---- LocalAddressTests ------------------------------------------------------------------------------

fn candidate(address: &str, nic: &str) -> AddressCandidate {
    AddressCandidate::new(address.parse().unwrap(), nic, false, true, 3600)
}

fn temporary(address: &str, nic: &str) -> AddressCandidate {
    AddressCandidate { temporary: true, ..candidate(address, nic) }
}

fn v6addr(text: &str) -> std::net::Ipv6Addr {
    text.parse().unwrap()
}

#[test]
fn the_stable_address_on_the_route_interface_replaces_a_temporary_source() {
    let all = [
        temporary("2400:aaaa:bbbb:cccc:5e1:22:33:44", "eth"), // what Windows sends from
        candidate("2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566", "eth"), // the stable one
        candidate("fe80::1", "eth"),
        candidate("fd7a:115c:a1e0::1", "tailscale"),
        candidate("2400:9999::5", "vpn"),
    ];
    assert_eq!(
        local_addresses::pick_stable_ipv6(Some(v6addr("2400:aaaa:bbbb:cccc:5e1:22:33:44")), &all),
        Some(v6addr("2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566"))
    );
}

#[test]
fn falls_back_to_the_route_source_and_rejects_non_global_sources() {
    let only_temp = [temporary("2400::5", "eth")];
    assert_eq!(local_addresses::pick_stable_ipv6(Some(v6addr("2400::5")), &only_temp), Some(v6addr("2400::5")));
    let deprecated = [temporary("2400::5", "eth"), AddressCandidate { preferred: false, ..candidate("2400::6", "eth") }];
    assert_eq!(local_addresses::pick_stable_ipv6(Some(v6addr("2400::5")), &deprecated), Some(v6addr("2400::5")));
    assert_eq!(local_addresses::pick_stable_ipv6(None, &only_temp), None);
    assert_eq!(local_addresses::pick_stable_ipv6(Some(v6addr("2001:0:1::1")), &only_temp), None); // Teredo
    assert_eq!(local_addresses::pick_stable_ipv6(Some(v6addr("2400::7")), &[]), Some(v6addr("2400::7")));
}

/// Not in the C# tests: the two tie-breakers of the choice (the route's own /64 first, then the lifetime).
#[test]
fn among_stable_addresses_the_routes_prefix_wins_then_the_longer_lifetime() {
    let source = v6addr("2400:1:2:3::99");
    let other_prefix_long_lived = AddressCandidate { preferred_lifetime_seconds: 86400, ..candidate("2400:1:2:4::1", "eth") };
    let same_prefix = AddressCandidate { preferred_lifetime_seconds: 60, ..candidate("2400:1:2:3::1", "eth") };
    let same_prefix_longer = AddressCandidate { preferred_lifetime_seconds: 600, ..candidate("2400:1:2:3::2", "eth") };
    let all = [temporary("2400:1:2:3::99", "eth"), other_prefix_long_lived.clone(), same_prefix.clone(), same_prefix_longer];
    assert_eq!(local_addresses::pick_stable_ipv6(Some(source), &all), Some(v6addr("2400:1:2:3::2")));
    let reordered = [same_prefix, other_prefix_long_lived, temporary("2400:1:2:3::99", "eth")];
    assert_eq!(local_addresses::pick_stable_ipv6(Some(source), &reordered), Some(v6addr("2400:1:2:3::1")));
}

#[test]
fn global_unicast_classification() {
    for (address, global) in [
        ("2400:aaaa::1", true),
        ("2001:4860:4860::8888", true),
        ("fe80::1", false),
        ("fd00::1", false),
        ("2001::1", false),
        ("2002:c000:204::1", false),
        ("2001:db8::1", false),
        ("::1", false),
        ("192.0.2.1", false),
    ] {
        assert_eq!(local_addresses::is_global_unicast_v6(&address.parse().unwrap()), global, "{address}");
    }
}

#[test]
fn reading_interfaces_works_on_this_platform() {
    let all = local_addresses::read();
    assert!(all.iter().any(|c| c.address.is_loopback()), "{all:?}");
    assert!(all.iter().all(|c| !c.interface_id.is_empty()), "{all:?}");
    // whatever this PC's addresses are, the detection must come back with something it could bind
    let detected = LinkAddresses::detect();
    if let Some(v6) = detected.ipv6 {
        assert!(local_addresses::is_global_unicast_v6(&IpAddr::V6(v6)));
        assert!(all.iter().any(|c| c.address == IpAddr::V6(v6)), "{v6} is not one of this PC's addresses");
        // (not bound here: the tests stay on loopback)
    }
    if let Some(v4) = detected.ipv4_advertise {
        assert!(all.iter().any(|c| c.address == IpAddr::V4(v4)), "{v4} is not one of this PC's addresses");
    }
}

// ---- P2pLinkSocketTests -----------------------------------------------------------------------------
// Real sockets on loopback, a stateful firewall in front of each WAN socket.

#[test]
fn punches_through_both_firewalls_and_carries_data_both_ways() {
    let keys = random_keys();
    let (fw_h6, fw_h4, fw_v6, fw_v4) = (StatefulFirewall::new(), StatefulFirewall::new(), StatefulFirewall::new(), StatefulFirewall::new());
    let child = EchoChild::new();
    let host = HostLink::new(&keys, &loopback(&fw_h6, &fw_h4)).unwrap();
    let viewer = ViewerLink::new(&keys, &loopback(&fw_v6, &fw_v4), 0).unwrap();

    let events: Arc<Mutex<Vec<HostSessionInfo>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = events.clone();
    host.on_session_changed(move |info| seen.lock().unwrap().push(info.clone()));
    let sid = keys::new_id();
    host.add_session(&sid, &viewer.candidates(), child.port(), Some("小红".to_string())).unwrap();
    viewer.start_session(&sid, &host.candidates()).unwrap();

    assert!(wait_for(|| viewer.state() == LinkState::Connected, 5000), "viewer: {:?} {}", viewer.state(), viewer.reason());
    if viewer.has_ipv6() {
        assert!(viewer.path().unwrap().is_ipv6());
    }
    assert_eq!(round_trip(viewer.local_port(), "hello"), "RE:hello");
    assert!(child.received() >= 1);
    assert!(wait_for(|| events.lock().unwrap().iter().any(|e| e.state == LinkState::Connected && e.tag.as_deref() == Some("小红")), 2000));
    assert!(viewer.describe_path().contains("ms"), "{}", viewer.describe_path());
    assert!(viewer.describe_path().starts_with(if viewer.has_ipv6() { "IPv6 [::1]:" } else { "IPv4 127.0.0.1:" }), "{}", viewer.describe_path());
    assert!(!viewer.path_learned(), "on loopback the path is an advertised candidate");
    assert_eq!(viewer.session_id().as_deref(), Some(sid.as_str()));
    assert!(viewer.bytes_from_host() >= 8);
    assert!(host.wan_bytes_in() > 0 && host.wan_bytes_out() > 0);
    let snapshot = host.snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].sid, sid);
    assert!(snapshot[0].bytes_to_viewer >= 8);
    assert!(!snapshot[0].path_learned);
}

#[test]
fn falls_back_to_ipv4_when_ipv6_is_blocked() {
    if !common::has_ipv6_loopback() {
        common::skip("falls_back_to_ipv4_when_ipv6_is_blocked", "this machine has no IPv6 loopback");
        return;
    }
    let keys = random_keys();
    let child = EchoChild::new();
    let host = HostLink::new(&keys, &loopback(&StatefulFirewall::blocking(), &StatefulFirewall::new())).unwrap();
    let viewer = ViewerLink::new(&keys, &loopback(&StatefulFirewall::blocking(), &StatefulFirewall::new()), 0).unwrap();
    let sid = keys::new_id();
    host.add_session(&sid, &viewer.candidates(), child.port(), None).unwrap();
    viewer.start_session(&sid, &host.candidates()).unwrap();
    assert!(wait_for(|| viewer.state() == LinkState::Connected, 5000));
    assert!(viewer.path().unwrap().is_ipv4());
    assert_eq!(round_trip(viewer.local_port(), "v4"), "RE:v4");
}

#[test]
fn strangers_and_wrong_keys_never_reach_the_child() {
    let keys = random_keys();
    let child = EchoChild::new();
    let host = HostLink::new(&keys, &loopback_v4()).unwrap();
    let intruder = ViewerLink::new(&random_keys(), &loopback_v4(), 0).unwrap();
    let raw = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();

    let sid = keys::new_id();
    host.add_session(&sid, &intruder.candidates(), child.port(), None).unwrap();
    intruder.start_session(&sid, &host.candidates()).unwrap(); // same session id, different group key
    let host_addr = endpoint::parse(&host.candidates()[0]).unwrap().to_socket_addr(0).unwrap();
    for _ in 0..20 {
        raw.send_to(&[0u8; 1316], host_addr).unwrap(); // SRT-sized junk from nowhere
    }
    thread::sleep(Duration::from_millis(800));
    round_trip(intruder.local_port(), "let me in");
    assert_eq!(child.received(), 0);
    assert_ne!(intruder.state(), LinkState::Connected);
    assert_eq!(host.snapshot()[0].state, LinkState::Punching);
}

#[test]
fn removing_a_session_tells_the_viewer_and_ending_one_tells_the_host() {
    let keys = random_keys();
    let child = EchoChild::new();
    let host = HostLink::new(&keys, &loopback_v4()).unwrap();
    let viewer = ViewerLink::new(&keys, &loopback_v4(), 0).unwrap();
    let (closed_tx, closed_rx) = mpsc::channel();
    let closed_tx = Mutex::new(closed_tx);
    host.on_session_changed(move |info| {
        if info.state == LinkState::Closed {
            let _ = closed_tx.lock().unwrap().send(info.clone());
        }
    });
    let states: Arc<Mutex<Vec<LinkState>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = states.clone();
    viewer.on_state_changed(move |state| seen.lock().unwrap().push(state));

    let sid = keys::new_id();
    host.add_session(&sid, &viewer.candidates(), child.port(), None).unwrap();
    viewer.start_session(&sid, &host.candidates()).unwrap();
    assert!(wait_for(|| viewer.state() == LinkState::Connected, 5000));
    host.remove_session(&sid, "stream ended");
    assert!(wait_for(|| viewer.state() == LinkState::Closed, 2000));
    assert_eq!(viewer.reason(), "the other side ended the session");
    assert_eq!(host.session_count(), 0);
    let removed = closed_rx.recv_timeout(Duration::from_secs(2)).expect("removing a session is reported");
    assert_eq!((removed.sid.as_str(), removed.reason.as_str()), (sid.as_str(), "stream ended"));
    assert!(wait_for(|| *states.lock().unwrap() == [LinkState::Connected, LinkState::Closed], 2000), "{:?}", states.lock().unwrap());

    // a re-join with a new session id on the same local port
    let sid2 = keys::new_id();
    let port = viewer.local_port();
    host.add_session(&sid2, &viewer.candidates(), child.port(), None).unwrap();
    viewer.start_session(&sid2, &host.candidates()).unwrap();
    assert!(wait_for(|| viewer.state() == LinkState::Connected, 5000));
    assert_eq!(viewer.local_port(), port);
    assert_eq!(round_trip(viewer.local_port(), "again"), "RE:again");
    while closed_rx.try_recv().is_ok() {}
    viewer.end_session("bye");
    let ended = closed_rx.recv_timeout(Duration::from_secs(2)).expect("the viewer's BYE closes the host's session");
    assert_eq!((ended.sid.as_str(), ended.reason.as_str()), (sid2.as_str(), "the other side ended the session"));
    assert_eq!(viewer.reason(), "bye");
}

/// Not in the C# tests: closing a link is prompt (the socket threads are woken, not waited out) and says goodbye.
#[test]
fn closing_a_link_is_quick_and_tells_the_other_side() {
    let keys = random_keys();
    let child = EchoChild::new();
    let host = HostLink::new(&keys, &loopback_v4()).unwrap();
    let viewer = ViewerLink::new(&keys, &loopback_v4(), 0).unwrap();
    let sid = keys::new_id();
    host.add_session(&sid, &viewer.candidates(), child.port(), None).unwrap();
    viewer.start_session(&sid, &host.candidates()).unwrap();
    assert!(wait_for(|| viewer.state() == LinkState::Connected, 5000));

    let started = std::time::Instant::now();
    host.dispose();
    host.dispose();
    assert!(started.elapsed() < Duration::from_millis(1500), "closing took {:?}", started.elapsed());
    assert!(wait_for(|| viewer.state() == LinkState::Closed, 2000));
    assert_eq!(viewer.reason(), "the other side ended the session");
    assert_eq!(host.session_count(), 0);
    assert!(!host.send_side(&sid, b"x"));
    drop(viewer); // sends its own goodbye into the void; must not hang
    assert!(started.elapsed() < Duration::from_secs(8));
}

/// Not in the C# tests: the receiver gives mpv a fixed local port. A stopped link must have let go of it, so the
/// next one (the user clicks Watch again) can bind it while the old object is still around.
#[test]
fn a_closed_viewer_link_frees_mpvs_port_for_the_next_one() {
    let keys = random_keys();
    let first = ViewerLink::new(&keys, &loopback_v4(), 0).unwrap();
    let port = first.local_port();
    let taken = ViewerLink::new(&keys, &loopback_v4(), port);
    assert!(taken.is_err(), "two links cannot share mpv's port");
    first.dispose();
    let second = ViewerLink::new(&keys, &loopback_v4(), port).expect("the port is free once the first link is closed");
    assert_eq!(second.local_port(), port);
    assert_eq!(first.local_port(), port, "a closed link still knows where it was");
    assert_eq!(first.state(), LinkState::Closed);
    assert!(!first.send_side(b"x"));
}
