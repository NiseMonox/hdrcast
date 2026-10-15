//! Port of the P2P parts of tests/P2pPlumbingTests.cs: SwitchableStreamTests and P2pDiagnosticsTests.
//! (ProgressOnStderrTests is about the sender's and receiver's settings and command lines: hdrcast-media.)

mod common;

use std::io::{self, Cursor, Read};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use hdrcast_p2p::{p2p_diagnostics, P2pHost, SwitchableStream};

// ---- SwitchableStreamTests --------------------------------------------------------------------------

/// Reads once on another thread; the result arrives on the channel.
fn read_in_background(stream: &SwitchableStream) -> mpsc::Receiver<(usize, Vec<u8>)> {
    let (tx, rx) = mpsc::channel();
    let mut stream = stream.clone();
    thread::spawn(move || {
        let mut buffer = [0u8; 16];
        let n = stream.read(&mut buffer).expect("a switchable stream never fails");
        let _ = tx.send((n, buffer[..n].to_vec()));
    });
    rx
}

const A_MOMENT: Duration = Duration::from_millis(100);
const LONG: Duration = Duration::from_secs(5);

#[test]
fn reads_across_a_source_change_and_ends_only_when_closed() {
    let s = SwitchableStream::new();
    let first = read_in_background(&s);
    assert!(first.recv_timeout(A_MOMENT).is_err()); // nothing attached yet: waits

    s.attach(Box::new(Cursor::new(b"abc".to_vec())));
    assert_eq!(first.recv_timeout(LONG).unwrap(), (3, b"abc".to_vec()));

    // the first encoder ended: the next read waits for the restarted one instead of returning 0
    let second = read_in_background(&s);
    assert!(second.recv_timeout(A_MOMENT).is_err());
    s.attach(Box::new(Cursor::new(b"defg".to_vec())));
    assert_eq!(second.recv_timeout(LONG).unwrap(), (4, b"defg".to_vec()));

    let third = read_in_background(&s);
    assert!(third.recv_timeout(A_MOMENT).is_err());
    s.close();
    assert_eq!(third.recv_timeout(LONG).unwrap().0, 0);
    let mut buffer = [0u8; 16];
    assert_eq!(s.clone().read(&mut buffer).unwrap(), 0);
    assert!(s.is_closed());

    // attaching after the end changes nothing
    s.attach(Box::new(Cursor::new(b"late".to_vec())));
    assert_eq!(s.clone().read(&mut buffer).unwrap(), 0);
}

struct ThrowingStream;

impl Read for ThrowingStream {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe"))
    }
}

#[test]
fn a_broken_source_counts_as_ended() {
    let s = SwitchableStream::new();
    s.attach(Box::new(ThrowingStream));
    let read = read_in_background(&s);
    assert!(read.recv_timeout(A_MOMENT).is_err());
    s.attach(Box::new(Cursor::new(vec![1u8, 2])));
    assert_eq!(read.recv_timeout(LONG).unwrap(), (2, vec![1u8, 2]));
}

/// A source that blocks in `read` until told to deliver, like a pipe from an encoder that produces nothing.
struct GatedSource {
    gate: mpsc::Receiver<Vec<u8>>,
}

impl Read for GatedSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.gate.recv() {
            Ok(data) => {
                buf[..data.len()].copy_from_slice(&data);
                Ok(data.len())
            }
            Err(_) => Ok(0), // the other end is gone: end of stream
        }
    }
}

/// Not in the C# tests: a new source can be attached while the reader is blocked inside the old one. What the
/// old one still delivers counts; when it ends, reading carries on with the new one.
#[test]
fn a_source_can_be_replaced_while_a_read_is_blocked_inside_it() {
    let s = SwitchableStream::new();
    let (old_tx, old_rx) = mpsc::channel();
    s.attach(Box::new(GatedSource { gate: old_rx }));
    let blocked = read_in_background(&s);
    assert!(blocked.recv_timeout(A_MOMENT).is_err(), "the read is inside the old source");

    s.attach(Box::new(Cursor::new(b"new".to_vec()))); // must not wait for the blocked read
    assert!(blocked.recv_timeout(A_MOMENT).is_err(), "attaching does not interrupt the old source");
    old_tx.send(b"old".to_vec()).unwrap();
    assert_eq!(blocked.recv_timeout(LONG).unwrap(), (3, b"old".to_vec()));
    assert_eq!(read_in_background(&s).recv_timeout(LONG).unwrap(), (3, b"new".to_vec()));

    // and when the old source simply ends while replaced, the read moves on by itself
    let (gone_tx, gone_rx) = mpsc::channel::<Vec<u8>>();
    s.attach(Box::new(GatedSource { gate: gone_rx }));
    let blocked = read_in_background(&s);
    assert!(blocked.recv_timeout(A_MOMENT).is_err());
    s.attach(Box::new(Cursor::new(b"next".to_vec())));
    drop(gone_tx);
    assert_eq!(blocked.recv_timeout(LONG).unwrap(), (4, b"next".to_vec()));

    // closing wakes a reader that waits for a source
    let waiting = read_in_background(&s);
    assert!(waiting.recv_timeout(A_MOMENT).is_err());
    s.close();
    assert_eq!(waiting.recv_timeout(LONG).unwrap().0, 0);
}

// ---- P2pDiagnosticsTests ----------------------------------------------------------------------------

const HOST_V6: [&str; 2] = ["[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:51234", "192.168.1.5:51235"];

#[test]
fn explains_whose_side_lacks_ipv6() {
    let host_v4_only = ["192.168.1.5:51235", "203.0.113.7:40000"];
    let host_lan_only = ["192.168.1.5:51235"];
    let me = ["192.168.5.14:59252"];
    assert!(p2p_diagnostics::explain(false, &me, &HOST_V6, None).contains("This PC has no IPv6"));
    assert!(p2p_diagnostics::explain(true, &me, &host_v4_only, None).contains("host's PC has no IPv6"));
    assert!(p2p_diagnostics::explain(false, &me, &host_v4_only, None).contains("IPv4 attempt did not get through"));
    assert!(p2p_diagnostics::explain(false, &me, &host_lan_only, None).contains("Neither side has IPv6"));
    assert!(p2p_diagnostics::explain(true, &me, &HOST_V6, None).contains("path is blocked"));

    // word for word, as in the receiver's status line since 0.2.3
    assert_eq!(
        p2p_diagnostics::explain(true, &me, &HOST_V6, None),
        "Both sides have IPv6 but the path is blocked - usually a firewall or VPN that drops all incoming UDP"
    );
    assert_eq!(
        p2p_diagnostics::explain(true, &me, &host_v4_only, None),
        "The host's PC has no IPv6 internet connection, and the IPv4 attempt did not get through (this PC's public IPv4 address could not be found); \
         they need to turn IPv6 on (check at test-ipv6.com)"
    );
    assert_eq!(
        p2p_diagnostics::explain(false, &me, &host_lan_only, None),
        "Neither side has IPv6, and the host offered no public IPv4 address; at least one side needs IPv6 (check at test-ipv6.com)"
    );
    assert_eq!(p2p_diagnostics::SIGNALING_DOWN, "The signaling server cannot be reached - check this PC's internet connection");
}

#[test]
fn does_not_call_a_host_ipv6_only_when_its_public_ipv4_was_tried() {
    // the host's UDP echo got no answer: it offered IPv6 and its LAN address, so IPv4 was never tried
    let host_no_echo = ["[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:51234", "192.168.1.3:57546"];
    let no_echo = p2p_diagnostics::explain(false, &["192.168.5.14:59252"], &host_no_echo, None);
    assert!(no_echo.contains("the host offered no public IPv4 address"), "{no_echo}");

    // with its public mapping the IPv4 attempt did happen and failed
    let host_dual = ["[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:51234", "192.168.1.3:57546", "203.0.113.7:57546"];
    let me_blind = p2p_diagnostics::explain(false, &["192.168.5.14:59252"], &host_dual, None);
    assert!(me_blind.contains("IPv4 attempt did not get through"), "{me_blind}");
    assert!(me_blind.contains("this PC's public IPv4 address could not be found"), "{me_blind}");
    let both_known = p2p_diagnostics::explain(false, &["192.168.5.14:59252", "198.51.100.9:59252"], &host_dual, None);
    assert!(both_known.contains("IPv4 attempt did not get through"), "{both_known}");
    assert!(!both_known.contains("could not be found"), "{both_known}");
}

#[test]
fn tells_public_ipv4_candidates_from_private_ones() {
    for (candidate, public) in [
        ("203.0.113.7:40000", true),
        ("192.168.1.3:57546", false),
        ("10.1.2.3:1", false),
        ("172.20.0.1:1", false),
        ("100.64.0.1:40000", false), // shared (CGNAT) range
        ("169.254.1.1:1", false),
        ("[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:51234", false),
    ] {
        assert_eq!(p2p_diagnostics::has_public_ipv4(&[candidate]), public, "{candidate}");
    }
    // IPv6 candidates: global ones count, and loopback (for tests); what cannot be an address never does
    assert!(p2p_diagnostics::has_ipv6(&HOST_V6));
    assert!(p2p_diagnostics::has_ipv6(&["[::1]:5000"]));
    assert!(!p2p_diagnostics::has_ipv6(&["192.168.1.5:51235", "[fd00::1]:5", "[2001:db8::1]:5"]));
    assert!(!p2p_diagnostics::has_ipv6(&["host.example:5000", "", "not an address"]));
    assert!(!p2p_diagnostics::has_public_ipv4(&Vec::<String>::new()));
}

#[test]
fn upload_estimate_counts_every_viewer_and_the_retransmission_headroom() {
    assert!((P2pHost::upload_mbps_for(40_000_000, 3, 25) - 150.0).abs() < 0.001);
    assert!(P2pHost::upload_mbps_for(40_000_000, 0, 25).abs() < 0.001);
}
