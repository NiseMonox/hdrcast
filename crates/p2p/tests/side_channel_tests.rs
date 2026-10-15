//! The side channel at the level of the links (docs/design.md §8.9): datagrams reach the handler both ways over
//! a punched loopback path; one from an unconfirmed address, with an unknown session id or with a malformed
//! body is dropped and never reaches the SRT side (the remux child / mpv); a side datagram counts as traffic
//! for the session's timers. The capability flags that decide whether anything is sent at all are tested with
//! the real signaling server in signal_integration_tests.rs.

mod common;

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hdrcast_core::group::keys;
use hdrcast_core::GroupKeys;
use hdrcast_p2p::control_packet::{self, ControlType, PeerRole};
use hdrcast_p2p::{mono_clock, side_channel, HostLink, LinkState, PeerSession, ViewerLink};

use common::{loopback, loopback_v4, random_keys, wait_for, EchoChild, StatefulFirewall};

type Seen<T> = Arc<Mutex<Vec<T>>>;

fn seen<T>() -> Seen<T> {
    Arc::new(Mutex::new(Vec::new()))
}

/// Long enough for a datagram on loopback to have arrived and been acted on, had anything acted on it.
const SETTLE: Duration = Duration::from_millis(300);

// ---- both ways over a punched path ----------------------------------------------------------------------

#[test]
fn side_datagrams_reach_the_handler_both_ways_over_a_punched_path() {
    let keys = random_keys();
    let (fw_h6, fw_h4, fw_v6, fw_v4) = (StatefulFirewall::new(), StatefulFirewall::new(), StatefulFirewall::new(), StatefulFirewall::new());
    let child = EchoChild::new();
    let host = HostLink::new(&keys, &loopback(&fw_h6, &fw_h4)).unwrap();
    let viewer = ViewerLink::new(&keys, &loopback(&fw_v6, &fw_v4), 0).unwrap();
    let at_host: Seen<(String, Vec<u8>)> = seen();
    let at_viewer: Seen<Vec<u8>> = seen();
    let sink = at_host.clone();
    host.set_side_handler(Arc::new(move |sid, payload| sink.lock().unwrap().push((sid.to_string(), payload.to_vec()))));
    let sink = at_viewer.clone();
    viewer.set_side_handler(Arc::new(move |payload| sink.lock().unwrap().push(payload.to_vec())));

    let sid = keys::new_id();
    // not connected yet: nothing can be sent, either way
    assert!(!viewer.send_side(b"too early"));
    assert!(!host.send_side(&sid, b"too early"));
    host.add_session(&sid, &viewer.candidates(), child.port(), None).unwrap();
    viewer.start_session(&sid, &host.candidates()).unwrap();
    assert!(wait_for(|| viewer.state() == LinkState::Connected, 5000), "{:?} {}", viewer.state(), viewer.reason());
    assert!(wait_for(|| host.snapshot()[0].state == LinkState::Connected, 5000));

    // viewer → host
    assert!(viewer.send_side(b"pointer 100,200"));
    assert!(wait_for(|| at_host.lock().unwrap().len() == 1, 2000), "the host's handler was not called");
    assert_eq!(at_host.lock().unwrap()[0], (sid.clone(), b"pointer 100,200".to_vec()));
    // host → viewer
    assert!(host.send_side(&sid, b"granted"));
    assert!(wait_for(|| at_viewer.lock().unwrap().len() == 1, 2000), "the viewer's handler was not called");
    assert_eq!(at_viewer.lock().unwrap()[0], b"granted".to_vec());

    // the size limits: 1..=1200 bytes
    let largest = vec![0xA5u8; side_channel::MAX_PAYLOAD];
    assert!(viewer.send_side(&largest));
    assert!(host.send_side(&sid, &largest));
    assert!(wait_for(|| at_host.lock().unwrap().len() == 2 && at_viewer.lock().unwrap().len() == 2, 2000));
    assert_eq!(at_host.lock().unwrap()[1].1, largest);
    assert_eq!(at_viewer.lock().unwrap()[1], largest);
    for bad in [&[][..], &vec![0u8; side_channel::MAX_PAYLOAD + 1][..]] {
        assert!(!viewer.send_side(bad), "{} bytes", bad.len());
        assert!(!host.send_side(&sid, bad), "{} bytes", bad.len());
    }
    assert!(!host.send_side(&keys::new_id(), b"nobody"), "an unknown session");
    assert!(!host.send_side("not an id", b"nobody"));

    // none of it went to the SRT side, and the stream's own path still works next to it
    thread::sleep(SETTLE);
    assert_eq!(child.received(), 0, "a side datagram reached the remux child: {:?}", String::from_utf8_lossy(&child.last()));
    assert_eq!(common::round_trip(viewer.local_port(), "srt"), "RE:srt");
    assert_eq!(child.received(), 1);
    assert_eq!((at_host.lock().unwrap().len(), at_viewer.lock().unwrap().len()), (2, 2));

    // a replaced handler takes over; an ended session carries nothing
    let replaced: Seen<Vec<u8>> = seen();
    let sink = replaced.clone();
    viewer.set_side_handler(Arc::new(move |payload| sink.lock().unwrap().push(payload.to_vec())));
    assert!(host.send_side(&sid, b"again"));
    assert!(wait_for(|| replaced.lock().unwrap().len() == 1, 2000));
    assert_eq!(at_viewer.lock().unwrap().len(), 2);
    host.remove_session(&sid, "done");
    assert!(!host.send_side(&sid, b"gone"));
    assert!(wait_for(|| viewer.state() == LinkState::Closed, 2000));
    assert!(!viewer.send_side(b"gone"));
}

// ---- a peer made by hand --------------------------------------------------------------------------------

/// The other end as a plain socket plus the state machine: it punches like a real peer, and can then send
/// whatever a test wants from an address the link under test has confirmed.
struct RawPeer {
    socket: UdpSocket,
    session: PeerSession,
    sid: String,
    /// What arrived that was not a control packet, and how many KEEPALIVEs.
    data: Vec<Vec<u8>>,
    keepalives: usize,
}

impl RawPeer {
    fn new(keys: &GroupKeys, sid: &str, role: PeerRole) -> RawPeer {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        // polled, not read with a timeout: on Windows a datagram arriving as a receive timeout expires is lost
        socket.set_nonblocking(true).unwrap();
        let session = PeerSession::new(keys, sid, role, mono_clock::now_us()).unwrap();
        RawPeer { socket, session, sid: sid.to_string(), data: Vec::new(), keepalives: 0 }
    }

    fn candidate(&self) -> String {
        self.socket.local_addr().unwrap().to_string()
    }

    fn sid_bytes(&self) -> [u8; 16] {
        *self.session.sid_bytes()
    }

    /// Reads what is there for `time`; with `ticking`, also runs the state machine's timers (punches,
    /// keepalives). Control packets are answered either way.
    fn pump(&mut self, time: Duration, ticking: bool) {
        let end = Instant::now() + time;
        let mut buffer = [0u8; 2048];
        loop {
            if ticking {
                for out in self.session.tick(mono_clock::now_us()) {
                    let _ = self.socket.send_to(&out.data, out.to);
                }
            }
            match self.socket.recv_from(&mut buffer) {
                Ok((n, from)) => self.receive(&buffer[..n], from),
                Err(_) => thread::sleep(Duration::from_millis(1)), // nothing there yet
            }
            if Instant::now() >= end {
                return;
            }
        }
    }

    fn receive(&mut self, datagram: &[u8], from: SocketAddr) {
        let ours = control_packet::looks_like_control(datagram) && control_packet::sid_of(datagram) == self.sid && control_packet::verify_mac(datagram, self.session.punch_key());
        if !ours {
            self.data.push(datagram.to_vec());
            return;
        }
        if control_packet::type_of(datagram) == Some(ControlType::Keepalive) {
            self.keepalives += 1;
        }
        for out in self.session.on_control(datagram, from, mono_clock::now_us()) {
            let _ = self.socket.send_to(&out.data, out.to);
        }
    }

    /// Punches until `connected` says the link under test is connected to us too.
    fn punch(&mut self, connected: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !(self.session.state() == LinkState::Connected && connected()) {
            assert!(Instant::now() < deadline, "the hand-made peer never connected ({:?})", self.session.state());
            self.pump(Duration::from_millis(20), true);
        }
    }

    fn send(&self, datagram: &[u8], to: SocketAddr) {
        self.socket.send_to(datagram, to).unwrap();
    }
}

fn first_candidate(candidates: &[String]) -> SocketAddr {
    candidates[0].parse().unwrap()
}

/// Side datagrams that must go nowhere: (what is wrong with it, the bytes).
fn malformed(sid_bytes: &[u8; 16]) -> Vec<(&'static str, Vec<u8>)> {
    let header = [&side_channel::MAGIC[..], &sid_bytes[..]].concat();
    vec![
        ("no payload", header.clone()),
        ("a payload one byte too long", [&header[..], &[7u8; side_channel::MAX_PAYLOAD + 1][..]].concat()),
        ("the magic and nothing else", side_channel::MAGIC.to_vec()),
        ("half a session id", header[..16].to_vec()),
    ]
}

// ---- the host's side --------------------------------------------------------------------------------------

#[test]
fn the_host_drops_side_datagrams_it_cannot_place_and_never_hands_them_to_the_remux_child() {
    let keys = random_keys();
    let child = EchoChild::new();
    let host = HostLink::new(&keys, &loopback_v4()).unwrap();
    let delivered: Seen<(String, Vec<u8>)> = seen();
    let sink = delivered.clone();
    host.set_side_handler(Arc::new(move |sid, payload| sink.lock().unwrap().push((sid.to_string(), payload.to_vec()))));

    let sid = keys::new_id();
    let mut viewer = RawPeer::new(&keys, &sid, PeerRole::Viewer);
    viewer.session.add_remote_candidates(&host.candidates());
    host.add_session(&sid, &[viewer.candidate()], child.port(), None).unwrap();
    let host_addr = first_candidate(&host.candidates());
    viewer.punch(|| host.snapshot()[0].state == LinkState::Connected);

    // the good one, so that everything below is known to fail for its own reason
    viewer.send(&side_channel::build(&viewer.sid_bytes(), b"click").unwrap(), host_addr);
    assert!(wait_for(|| delivered.lock().unwrap().len() == 1, 2000));
    assert_eq!(delivered.lock().unwrap()[0], (sid.clone(), b"click".to_vec()));

    // a session id nobody has, from the confirmed address
    let other_sid = keys::id_bytes_of(&keys::new_id()).unwrap();
    viewer.send(&side_channel::build(&other_sid, b"wrong session").unwrap(), host_addr);
    // the right session id, from an address that never punched
    let stranger = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    stranger.send_to(&side_channel::build(&viewer.sid_bytes(), b"stranger").unwrap(), host_addr).unwrap();
    // the right session and address, but not a side datagram anybody could have built
    for (what, datagram) in malformed(&viewer.sid_bytes()) {
        assert!(side_channel::has_magic(&datagram), "{what}");
        viewer.send(&datagram, host_addr);
    }
    // a session that has ended: its id is no longer live
    let ended_sid = keys::new_id();
    let mut ended = RawPeer::new(&keys, &ended_sid, PeerRole::Viewer);
    ended.session.add_remote_candidates(&host.candidates());
    host.add_session(&ended_sid, &[ended.candidate()], child.port(), None).unwrap();
    ended.punch(|| host.snapshot().iter().any(|s| s.sid == ended_sid && s.state == LinkState::Connected));
    host.remove_session(&ended_sid, "over");
    ended.send(&side_channel::build(&ended.sid_bytes(), b"too late").unwrap(), host_addr);

    thread::sleep(SETTLE);
    let got = delivered.lock().unwrap().clone(); // (a copy: a failing assertion must not print through a lock it holds)
    assert_eq!(got.len(), 1, "delivered although it should have been dropped: {got:?}");
    assert_eq!(child.received(), 0, "the remux child got {:?}", String::from_utf8_lossy(&child.last()));

    // the same address does carry the stream: plain data goes to the child, and only that
    viewer.send(b"srt handshake", host_addr);
    assert!(wait_for(|| child.received() == 1, 2000));
    assert_eq!(child.last(), b"srt handshake");
    // and the child's answer comes back to the viewer, unlike anything of the side channel
    viewer.pump(SETTLE, false);
    assert_eq!(viewer.data, [b"RE:srt handshake".to_vec()]);
}

// ---- the viewer's side ------------------------------------------------------------------------------------

#[test]
fn the_viewer_drops_side_datagrams_it_cannot_place_and_never_hands_them_to_mpv() {
    let keys = random_keys();
    let viewer = ViewerLink::new(&keys, &loopback_v4(), 0).unwrap();
    let delivered: Seen<Vec<u8>> = seen();
    let sink = delivered.clone();
    viewer.set_side_handler(Arc::new(move |payload| sink.lock().unwrap().push(payload.to_vec())));

    let sid = keys::new_id();
    let mut host = RawPeer::new(&keys, &sid, PeerRole::Host);
    host.session.add_remote_candidates(&viewer.candidates());
    viewer.start_session(&sid, &[host.candidate()]).unwrap();
    let viewer_addr = first_candidate(&viewer.candidates());
    host.punch(|| viewer.state() == LinkState::Connected);

    // "mpv": the viewer link sends the host's data to wherever mpv's packets last came from
    let mpv = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    mpv.set_read_timeout(Some(SETTLE)).unwrap();
    mpv.send_to(b"srt hello", (Ipv4Addr::LOCALHOST, viewer.local_port())).unwrap();
    host.pump(SETTLE, false);
    assert_eq!(host.data, [b"srt hello".to_vec()]);
    host.data.clear();

    host.send(&side_channel::build(&host.sid_bytes(), b"granted").unwrap(), viewer_addr);
    assert!(wait_for(|| delivered.lock().unwrap().len() == 1, 2000));
    assert_eq!(delivered.lock().unwrap()[0], b"granted".to_vec());

    let other_sid = keys::id_bytes_of(&keys::new_id()).unwrap();
    host.send(&side_channel::build(&other_sid, b"wrong session").unwrap(), viewer_addr);
    let stranger = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    stranger.send_to(&side_channel::build(&host.sid_bytes(), b"stranger").unwrap(), viewer_addr).unwrap();
    for (what, datagram) in malformed(&host.sid_bytes()) {
        assert!(side_channel::has_magic(&datagram), "{what}");
        host.send(&datagram, viewer_addr);
    }
    let mut buffer = [0u8; 2048];
    assert!(mpv.recv_from(&mut buffer).is_err(), "mpv got {:?}", String::from_utf8_lossy(&buffer[..32]));
    let got = delivered.lock().unwrap().clone(); // (a copy: a failing assertion must not print through a lock it holds)
    assert_eq!(got.len(), 1, "delivered although it should have been dropped: {got:?}");

    // plain data from the same address does reach mpv
    host.send(b"srt data", viewer_addr);
    let (n, _) = mpv.recv_from(&mut buffer).expect("the stream's data reaches mpv");
    assert_eq!(&buffer[..n], b"srt data");

    // what the viewer sends on the side channel is exactly the datagram of the format, on the stream's path
    assert!(viewer.send_side(b"move"));
    host.pump(SETTLE, false);
    assert_eq!(host.data, [side_channel::build(&host.sid_bytes(), b"move").unwrap()]);
    assert_eq!(host.data[0], [&b"HDRCSIDE"[..], &keys::id_bytes_of(&sid).unwrap()[..], &b"move"[..]].concat());

    // after a re-join the old session's id is no longer live, from the very same address
    let sid2 = keys::new_id();
    let mut host2 = RawPeer::new(&keys, &sid2, PeerRole::Host);
    host2.session.add_remote_candidates(&viewer.candidates());
    viewer.start_session(&sid2, &[host2.candidate()]).unwrap();
    host2.punch(|| viewer.state() == LinkState::Connected);
    host.send(&side_channel::build(&host.sid_bytes(), b"old session").unwrap(), viewer_addr);
    host2.send(&side_channel::build(&host2.sid_bytes(), b"new session").unwrap(), viewer_addr);
    assert!(wait_for(|| delivered.lock().unwrap().len() == 2, 2000));
    thread::sleep(SETTLE);
    assert_eq!(*delivered.lock().unwrap(), [b"granted".to_vec(), b"new session".to_vec()]);
    assert_eq!(viewer.session_id().as_deref(), Some(sid2.as_str()));
}

// ---- timers -------------------------------------------------------------------------------------------------

/// A side datagram is traffic: receiving one feeds the silence timer (15 s) like SRT data does, and sending
/// one resets the keepalive timer (2 s). Takes 20 s: one silence timeout and a bit.
#[test]
fn side_datagrams_count_as_traffic_for_the_silence_and_keepalive_timers() {
    let keys = random_keys();
    let child = EchoChild::new();
    let host = HostLink::new(&keys, &loopback_v4()).unwrap();
    let delivered: Seen<Vec<u8>> = seen();
    let sink = delivered.clone();
    host.set_side_handler(Arc::new(move |_, payload| sink.lock().unwrap().push(payload.to_vec())));

    let sid = keys::new_id();
    let mut viewer = RawPeer::new(&keys, &sid, PeerRole::Viewer);
    viewer.session.add_remote_candidates(&host.candidates());
    host.add_session(&sid, &[viewer.candidate()], child.port(), None).unwrap();
    let host_addr = first_candidate(&host.candidates());
    viewer.punch(|| host.snapshot()[0].state == LinkState::Connected);

    // A control: an idle host does send keepalives (one every 2 s), which this peer counts. Three seconds: one
    // has come, the next is a second away.
    viewer.pump(Duration::from_millis(3000), false);
    assert!(viewer.keepalives >= 1, "an idle host sends keepalives");
    viewer.keepalives = 0;
    viewer.data.clear();

    // From here on the hand-made viewer sends no control packet at all, only a side datagram every 500 ms,
    // and the host sends one back every 500 ms: no SRT, no PUNCH, no KEEPALIVE from the viewer for 17 s.
    let mut sent = 0u32;
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(17_000) {
        viewer.send(&side_channel::build(&viewer.sid_bytes(), &sent.to_be_bytes()).unwrap(), host_addr);
        assert!(host.send_side(&sid, &sent.to_be_bytes()), "after {:?}: {:?}", started.elapsed(), host.snapshot());
        sent += 1;
        viewer.pump(Duration::from_millis(500), false);
    }
    let session = &host.snapshot()[0];
    assert_eq!(session.state, LinkState::Connected, "the session went silent although side datagrams kept arriving: {}", session.reason);
    let got: Vec<u32> = delivered.lock().unwrap().iter().map(|p| u32::from_be_bytes(p[..4].try_into().unwrap())).collect();
    assert_eq!(got, (0..sent).collect::<Vec<u32>>(), "every side datagram of the viewer arrived, once and in order");
    let back: Vec<u32> = viewer.data.iter().map(|p| u32::from_be_bytes(p[24..28].try_into().unwrap())).collect();
    assert_eq!(back, (0..sent).collect::<Vec<u32>>(), "every side datagram of the host arrived, once and in order");
    assert_eq!(viewer.keepalives, 0, "sending side datagrams is sending: no keepalive is due");
    assert_eq!(child.received(), 0);
}
