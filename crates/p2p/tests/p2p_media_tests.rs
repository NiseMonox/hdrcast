//! Port of tests/P2pMediaTests.cs: TsFanOutTests, and the whole P2P media path on one machine
//! (libx265 Main10 PQ → TsFanOut → remux children (SRT listeners) → HostLink → stateful firewalls → ViewerLink →
//! ffprobe as the viewer). The media test needs ffmpeg and ffprobe (with libx265 and libsrt): in the directory
//! named by HDRCAST_TOOLS, or on PATH; without them it prints a note and passes.

mod common;

use std::io::{self, Cursor, Write};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use hdrcast_core::group::keys;
use hdrcast_core::srt_url;
use hdrcast_core::GroupKeys;
use hdrcast_p2p::{HostLink, LinkAddresses, LinkState, RemuxChild, TsFanOut, ViewerLink};

use common::{loopback, random_keys, wait_for, StatefulFirewall};

// ---- TsFanOutTests ----------------------------------------------------------------------------------

/// A sink whose writes block until released: a remux child still waiting for its viewer.
struct StuckSink {
    released: Arc<(Mutex<bool>, Condvar)>,
    written: Arc<AtomicU64>,
}

impl Write for StuckSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let (flag, cond) = &*self.released;
        let mut released = flag.lock().unwrap();
        while !*released {
            released = cond.wait(released).unwrap();
        }
        self.written.fetch_add(buf.len() as u64, Ordering::SeqCst);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct CountingSink(Arc<AtomicU64>);

impl Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.fetch_add(buf.len() as u64, Ordering::SeqCst);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct BrokenSink;

impl Write for BrokenSink {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_stuck_viewer_loses_only_its_own_data() {
    const TOTAL: u64 = 8 * 1024 * 1024;
    let fan = TsFanOut::new(Box::new(Cursor::new(vec![0u8; TOTAL as usize])));
    let released = Arc::new((Mutex::new(false), Condvar::new()));
    let (stuck_written, fast_written) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    // An in-memory source is far faster than any writer, so the fast sink gets a queue that can hold
    // everything; the point is that the stuck one (small queue, never drains) does not hold it back.
    fan.add_sink("stuck", Box::new(StuckSink { released: released.clone(), written: stuck_written.clone() }), 256 * 1024).unwrap();
    fan.add_sink("fast", Box::new(CountingSink(fast_written.clone())), 16 * 1024 * 1024).unwrap();
    let (ended_tx, ended_rx) = mpsc::channel();
    let ended_tx = Mutex::new(ended_tx);
    fan.on_input_ended(move || {
        let _ = ended_tx.lock().unwrap().send(());
    });
    fan.start().unwrap();
    assert!(ended_rx.recv_timeout(Duration::from_secs(5)).is_ok(), "the input never ended");
    assert!(wait_for(|| fast_written.load(Ordering::SeqCst) == TOTAL, 5000));
    assert_eq!(fast_written.load(Ordering::SeqCst), TOTAL);
    assert_eq!(fan.bytes_in(), TOTAL);
    assert_eq!(fan.stats("fast"), Some((TOTAL, 0)));
    let (_, stuck_clears) = fan.stats("stuck").unwrap();
    assert!(stuck_clears > 0, "the stuck sink's queue should have been cleared");
    assert_eq!(stuck_written.load(Ordering::SeqCst), 0);
    assert_eq!(fan.stats("nobody"), None);
    assert_eq!(fan.sink_count(), 2);
    fan.remove_sink("fast");
    assert_eq!(fan.sink_count(), 1);

    let (flag, cond) = &*released;
    *flag.lock().unwrap() = true;
    cond.notify_all();
}

#[test]
fn a_broken_sink_is_reported_and_removed() {
    let fan = TsFanOut::new(Box::new(Cursor::new(vec![0u8; 1024 * 1024])));
    let (failed_tx, failed_rx) = mpsc::channel();
    let failed_tx = Mutex::new(failed_tx);
    fan.on_sink_failed(move |id| {
        let _ = failed_tx.lock().unwrap().send(id.to_string());
    });
    fan.add_sink("gone", Box::new(BrokenSink), 1024 * 1024).unwrap();
    fan.start().unwrap();
    assert_eq!(failed_rx.recv_timeout(Duration::from_secs(5)).as_deref(), Ok("gone"));
    assert_eq!(fan.sink_count(), 0);
}

#[test]
fn queue_size_scales_with_bitrate() {
    assert_eq!(TsFanOut::queue_bytes_for(2_000_000), hdrcast_p2p::ts_fan_out::MIN_QUEUE_BYTES);
    assert_eq!(TsFanOut::queue_bytes_for(40_000_000), 40_000_000 / 8 / 2);
    assert_eq!(hdrcast_p2p::ts_fan_out::MIN_QUEUE_BYTES, 512 * 1024);
    assert_eq!(hdrcast_p2p::ts_fan_out::READ_CHUNK_BYTES, 188 * 7 * 32);
}

#[test]
fn remux_child_arguments_copy_and_listen_on_loopback_only() {
    let url = RemuxChild::build_listener_url(40000, "a77MWmBi3_603E3kH2abfOI5O-dIU5dn", 3500, 50, 40_000_000).unwrap();
    assert!(url.starts_with("srt://127.0.0.1:40000?mode=listener"), "{url}");
    let p = srt_url::parse(&url).unwrap();
    assert_eq!(p.get("latency"), Some("3500000"));
    assert_eq!(p.get("oheadbw"), Some("50"));
    // SRT is told what the stream takes on the wire, not the video bitrate alone (40 Mbps of video: 43.7 Mbps)
    assert_eq!(RemuxChild::wire_bps(40_000_000), 43_724_255);
    assert_eq!(p.get("inputbw"), Some("5465531"));
    assert_eq!(p.get("pbkeylen"), Some("32"));
    assert_eq!(p.get("listen_timeout"), Some("-1"));
    let args = RemuxChild::build_arguments(&url);
    assert_eq!(args.last(), Some(&url));
    for expected in ["copy", "+resend_headers", "pipe:0"] {
        assert!(args.iter().any(|a| a == expected), "{expected} missing from {args:?}");
    }
    // the whole command line, as docs/design.md §8.8 has it
    assert_eq!(
        args[..args.len() - 1].join(" "),
        "-hide_banner -loglevel warning -probesize 2000000 -analyzeduration 500000 -fflags +discardcorrupt -f mpegts -i pipe:0 \
         -map 0 -c copy -f mpegts -mpegts_flags +resend_headers -muxdelay 0 -muxpreload 0"
    );
    // an overhead outside what SRT takes is brought into range rather than refused
    let low = RemuxChild::build_listener_url(40000, "a77MWmBi3_603E3kH2abfOI5O-dIU5dn", 1500, 1, 40_000_000).unwrap();
    assert_eq!(srt_url::parse(&low).unwrap().get("oheadbw"), Some("5"));
    let high = RemuxChild::build_listener_url(40000, "a77MWmBi3_603E3kH2abfOI5O-dIU5dn", 1500, 400, 40_000_000).unwrap();
    assert_eq!(srt_url::parse(&high).unwrap().get("oheadbw"), Some("100"));
}

/// A sink added twice under one id replaces the first; removing an unknown id is nothing.
#[test]
fn a_sink_with_the_same_id_replaces_the_old_one() {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    struct Gated(Mutex<mpsc::Receiver<Vec<u8>>>);
    impl io::Read for Gated {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.lock().unwrap().recv() {
                Ok(data) => {
                    buf[..data.len()].copy_from_slice(&data);
                    Ok(data.len())
                }
                Err(_) => Ok(0),
            }
        }
    }
    let fan = TsFanOut::new(Box::new(Gated(Mutex::new(rx))));
    let (first, second) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    fan.add_sink("v", Box::new(CountingSink(first.clone())), 1 << 20).unwrap();
    fan.start().unwrap();
    tx.send(vec![1u8; 188]).unwrap();
    assert!(wait_for(|| first.load(Ordering::SeqCst) == 188, 5000));
    fan.add_sink("v", Box::new(CountingSink(second.clone())), 1 << 20).unwrap();
    assert_eq!(fan.sink_count(), 1);
    tx.send(vec![2u8; 376]).unwrap();
    assert!(wait_for(|| second.load(Ordering::SeqCst) == 376, 5000));
    assert_eq!(first.load(Ordering::SeqCst), 188, "the replaced sink gets nothing more");
    fan.remove_sink("nobody");
    fan.dispose();
    assert_eq!(fan.sink_count(), 0);
    tx.send(vec![3u8; 188]).unwrap(); // lets the reader thread notice and end
    thread::sleep(Duration::from_millis(50));
    assert_eq!(second.load(Ordering::SeqCst), 376);
}

// ---- P2pMediaEndToEndTests --------------------------------------------------------------------------

struct Viewer {
    link: ViewerLink,
    child: RemuxChild,
    sid: String,
}

/// What ffprobe sees as the viewer: like mpv's retry loop in the app, a child that has not seen input yet is
/// not listening yet.
fn probe(ffprobe: &Path, local_port: u16, keys: &GroupKeys, latency_ms: u32, entries: &str) -> String {
    let url = format!("srt://127.0.0.1:{local_port}?passphrase={}&pbkeylen=32&latency={}&connect_timeout=4000", keys.srt_passphrase(), latency_ms * 1000);
    let mut result = String::new();
    for _attempt in 0..3 {
        result = common::run(ffprobe, &["-hide_banner", "-v", "error", "-show_entries", entries, "-of", "compact", &url], 12_000).0;
        if result.contains("codec_name=") {
            break;
        }
    }
    result
}

/// libx265 Main10 PQ at 320x180, MPEG-TS on stdout, in real time for `seconds`.
fn encoder_command(ffmpeg: &Path, seconds: u32) -> Command {
    let mut command = Command::new(ffmpeg);
    command
        .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=30", "-t"])
        .arg(seconds.to_string())
        .args(["-c:v", "libx265", "-pix_fmt", "yuv420p10le"])
        .args(["-x265-params", "keyint=30:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:log-level=error"])
        .args(["-f", "mpegts", "-mpegts_flags", "+resend_headers", "-muxdelay", "0", "-muxpreload", "0", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

#[test]
fn hdr10_hevc_reaches_several_viewers_through_punched_links() {
    const TEST: &str = "hdr10_hevc_reaches_several_viewers_through_punched_links";
    let Some((ffmpeg, ffprobe)) = common::media_tools() else {
        common::skip(TEST, "needs ffmpeg and ffprobe with libx265 (set HDRCAST_TOOLS to their directory)");
        return;
    };
    if !common::has_ipv6_loopback() {
        common::skip(TEST, "this machine has no IPv6 loopback");
        return;
    }
    println!("{TEST}: using {}", ffmpeg.display());

    let keys = random_keys();
    const LATENCY_MS: u32 = 300;
    let mut encoder = common::KillOnDrop(encoder_command(&ffmpeg, 30).spawn().expect("the encoder starts"));
    let encoder_out = encoder.0.stdout.take().unwrap();

    let (host_fw6, host_fw4) = (StatefulFirewall::new(), StatefulFirewall::new());
    let fan = TsFanOut::new(Box::new(encoder_out));
    let host = HostLink::new(&keys, &loopback(&host_fw6, &host_fw4)).unwrap();
    fan.start().unwrap();

    let join = |name: &str| -> Viewer {
        let sid = keys::new_id();
        let child_port = RemuxChild::free_loopback_udp_port().unwrap();
        let mut child = RemuxChild::new(&ffmpeg, &RemuxChild::build_listener_url(child_port, keys.srt_passphrase(), LATENCY_MS, 25, 1_000_000).unwrap(), None);
        child.start().expect("the remux child starts");
        fan.add_sink(&sid, Box::new(child.input().unwrap()), TsFanOut::queue_bytes_for(1_000_000)).unwrap();
        let link = ViewerLink::new(
            &keys,
            &LinkAddresses {
                ipv6: Some(Ipv6Addr::LOCALHOST),
                ipv4_bind: Some(Ipv4Addr::LOCALHOST),
                ipv4_advertise: None,
                filter_v6: Some(StatefulFirewall::new()),
                filter_v4: Some(StatefulFirewall::new()),
            },
            0,
        )
        .unwrap();
        host.add_session(&sid, &link.candidates(), child_port, Some(name.to_string())).unwrap();
        link.start_session(&sid, &host.candidates()).unwrap();
        assert!(wait_for(|| link.state() == LinkState::Connected, 5000), "{name}: {}", link.reason());
        Viewer { link, child, sid }
    };
    const ENTRIES: &str = "stream=codec_name,profile,pix_fmt,color_transfer,color_primaries";

    let (a, b) = (join("A"), join("B"));
    let (result_a, result_b) = thread::scope(|scope| {
        let pa = scope.spawn(|| probe(&ffprobe, a.link.local_port(), &keys, LATENCY_MS, ENTRIES));
        let pb = scope.spawn(|| probe(&ffprobe, b.link.local_port(), &keys, LATENCY_MS, ENTRIES));
        (pa.join().unwrap(), pb.join().unwrap())
    });
    for result in [&result_a, &result_b] {
        assert!(result.contains("codec_name=hevc"), "{result}");
        assert!(result.contains("profile=Main 10"), "{result}");
        assert!(result.contains("color_transfer=smpte2084"), "{result}");
        assert!(result.contains("color_primaries=bt2020"), "{result}");
    }
    assert!(a.link.path().unwrap().is_ipv6());

    // C joins mid-stream; A leaves; B keeps working (its child restarts on the next connection)
    let c = join("C");
    let result_c = probe(&ffprobe, c.link.local_port(), &keys, LATENCY_MS, ENTRIES);
    assert!(result_c.contains("profile=Main 10"), "{result_c}");
    host.remove_session(&a.sid, "test");
    assert!(wait_for(|| a.link.state() == LinkState::Closed, 3000));
    assert_eq!(host.session_count(), 2);
    assert!(fan.bytes_in() > 0);

    // in the order the app uses: the links go, the children are killed (then their stdin closes), the fan-out
    // lets go, and only then the encoder
    for viewer in [&a, &b, &c] {
        viewer.link.dispose();
        viewer.child.dispose();
        assert!(viewer.child.has_exited());
    }
    fan.dispose();
    host.dispose();
    drop(encoder);
}
