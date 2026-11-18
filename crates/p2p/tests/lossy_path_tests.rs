//! The whole media path under loss, on one machine (docs/design.md §3.2, §8.11). An HEVC Main10 stream goes
//! through a remux child, HostLink and ViewerLink to ffmpeg receiving it raw, as the app's player pipe does,
//! while the viewer's side of the path loses some first sends of SRT data and every resend of those: each of
//! those losses is for good. Then:
//! * the SRT monitors on both ends saw the losses, the resends and what never arrived in time;
//! * decoded as it arrived, the stream shows pictures the encoder never made (the smearing a viewer sees);
//! * through the guard, every picture that is decoded is one the encoder made, bit for bit.
//!
//! A second run loses the first send and the first resend of some packets: libsrt's resends keep the original's
//! timestamp, and the viewer's monitor measures how late those packets came (docs/design.md §5.1).
//!
//! Needs ffmpeg with libx265 and libsrt (HDRCAST_TOOLS or PATH); without it the test prints a note and passes.

mod common;

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use hdrcast_core::group::keys;
use hdrcast_core::srt_url::{self, SrtRole, SrtTopology, SrtUrlOptions};
use hdrcast_media::receiver::{mpv_command, TsGuard};
use hdrcast_p2p::{HostLink, LinkAddresses, LinkState, PacketFilter, RemuxChild, TsFanOut, ViewerLink};

use common::{random_keys, wait_for, KillOnDrop};

const LATENCY_MS: u32 = 250;

/// Loses one in `every` SRT data packets that arrive for the first time, and every later copy of a lost one.
struct LossyPath {
    every: u64,
    state: Mutex<Lossy>,
    lost: AtomicU64,
}

#[derive(Default)]
struct Lossy {
    rng: u64,
    top: Option<u32>,
    gone: HashSet<u32>,
}

impl LossyPath {
    fn new(every: u64) -> LossyPath {
        LossyPath { every, state: Mutex::new(Lossy { rng: 0x9E37_79B9_7F4A_7C15, ..Lossy::default() }), lost: AtomicU64::new(0) }
    }
}

impl PacketFilter for LossyPath {
    fn on_outbound(&self, _: SocketAddr) {}

    fn allow_inbound(&self, _: SocketAddr) -> bool {
        true
    }

    fn allow_datagram(&self, _: SocketAddr, datagram: &[u8]) -> bool {
        // SRT data with a full TS payload; the link's own packets (58-byte control, the side channel) are small
        if datagram.len() < 1000 || datagram[0] & 0x80 != 0 {
            return true;
        }
        let seq = u32::from_be_bytes(datagram[..4].try_into().unwrap());
        let mut s = self.state.lock().unwrap();
        if s.gone.contains(&seq) {
            return false; // a resend of a lost one: lost as well
        }
        let first_send = s.top.is_none_or(|top| {
            let ahead = seq.wrapping_sub(top) & 0x7FFF_FFFF;
            ahead != 0 && ahead < 1 << 30
        });
        if first_send {
            s.top = Some(seq);
            // xorshift64
            s.rng ^= s.rng << 13;
            s.rng ^= s.rng >> 7;
            s.rng ^= s.rng << 17;
            if s.rng % self.every == 0 {
                s.gone.insert(seq);
                self.lost.fetch_add(1, Ordering::SeqCst);
                return false;
            }
        }
        true
    }
}

/// The encoder's stdout, with a copy of everything read going to a file (the reference).
struct Tee<R> {
    inner: R,
    copy: File,
}

impl<R: Read> Read for Tee<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.copy.write_all(&buf[..n])?;
        Ok(n)
    }
}

/// libx265 Main10 PQ like the app's stream: closed GOPs of one second, B-frames, MPEG-TS on stdout, in real time.
fn encoder(ffmpeg: &Path, seconds: u32) -> Command {
    let mut command = Command::new(ffmpeg);
    command
        .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30,noise=alls=12:allf=t", "-t"])
        .arg(seconds.to_string())
        .args(["-c:v", "libx265", "-pix_fmt", "yuv420p10le", "-b:v", "3000k"])
        .args([
            "-x265-params",
            "keyint=30:min-keyint=30:scenecut=0:open-gop=0:bframes=2:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:log-level=error",
        ])
        .args(["-f", "mpegts", "-mpegts_flags", "+resend_headers", "-muxdelay", "0", "-muxpreload", "0", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

/// The MD5 of every decoded picture of a file (ffmpeg's framemd5), in output order.
fn picture_hashes(ffmpeg: &Path, file: &Path, input_options: &[&str]) -> Vec<String> {
    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-v", "error"])
        .args(input_options)
        .arg("-i")
        .arg(file)
        .args(["-map", "0:v:0", "-f", "framemd5", "-"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .expect("ffmpeg decodes");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.rsplit(',').next().map(|hash| hash.trim().to_string()))
        .filter(|hash| hash.len() == 32)
        .collect()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hdrcast-lossy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn damaged_pictures_never_reach_the_decoder_and_both_ends_see_the_loss() {
    const TEST: &str = "damaged_pictures_never_reach_the_decoder_and_both_ends_see_the_loss";
    let Some((ffmpeg, _)) = common::media_tools() else {
        common::skip(TEST, "needs ffmpeg with libx265 and libsrt (set HDRCAST_TOOLS to its directory)");
        return;
    };
    let keys = random_keys();
    let (reference, received) = (scratch("reference.ts"), scratch("received.ts"));

    let mut encoder = KillOnDrop(encoder(&ffmpeg, 10).spawn().expect("the encoder starts"));
    let stdout = encoder.0.stdout.take().unwrap();
    let fan = TsFanOut::new(Box::new(Tee { inner: stdout, copy: File::create(&reference).unwrap() }));
    let host = HostLink::new(&keys, &common::loopback_v4()).unwrap();
    fan.start().unwrap();

    let sid = keys::new_id();
    let child_port = RemuxChild::free_loopback_udp_port().unwrap();
    let mut child = RemuxChild::new(&ffmpeg, &RemuxChild::build_listener_url(child_port, keys.srt_passphrase(), LATENCY_MS, 25, 3_000_000).unwrap(), None);
    child.start().expect("the remux child starts");
    fan.add_sink(&sid, Box::new(child.input().unwrap()), TsFanOut::queue_bytes_for(3_000_000)).unwrap();

    let lossy = std::sync::Arc::new(LossyPath::new(150));
    let viewer = ViewerLink::new(
        &keys,
        &LinkAddresses { ipv6: None, ipv4_bind: Some(Ipv4Addr::LOCALHOST), ipv4_advertise: None, filter_v6: None, filter_v4: Some(lossy.clone()) },
        0,
    )
    .unwrap();
    host.add_session(&sid, &viewer.candidates(), child_port, None).unwrap();
    viewer.start_session(&sid, &host.candidates()).unwrap();
    assert!(wait_for(|| viewer.state() == LinkState::Connected, 5000), "{}", viewer.reason());

    // the app's player pipe: ffmpeg hands over the raw SRT payload
    let url = srt_url::build(&SrtUrlOptions {
        topology: SrtTopology::Caller,
        role: SrtRole::Read,
        host: "127.0.0.1".to_string(),
        port: viewer.local_port(),
        passphrase: Some(keys.srt_passphrase().to_string()),
        latency_ms: LATENCY_MS,
        bitrate_bps: 10_000_000,
        ..SrtUrlOptions::default()
    })
    .unwrap();
    let mut receiver = KillOnDrop(
        Command::new(&ffmpeg)
            .args(mpv_command::ffmpeg_pipe_args(&url))
            .stdin(Stdio::null())
            .stdout(File::create(&received).unwrap())
            .stderr(Stdio::null())
            .spawn()
            .expect("the receiving ffmpeg starts"),
    );
    // the encoder runs for 10 s; then a moment for what is in flight
    let _ = encoder.0.wait();
    thread::sleep(Duration::from_millis(1500));
    let host_stats = host.srt_stats(&sid).expect("the session's figures");
    let viewer_stats = viewer.srt_stats();
    let _ = receiver.0.kill();
    let _ = receiver.0.wait();
    viewer.dispose();
    child.dispose();
    fan.dispose();
    host.dispose();

    let lost = lossy.lost.load(Ordering::SeqCst);
    println!("{TEST}: {lost} first sends lost for good; host {host_stats:?}; viewer {viewer_stats:?}");
    assert!(lost >= 5, "the path lost too little to show anything ({lost})");
    // what the host's monitor saw: everything it sent, the viewer's loss reports, its resends
    assert!(host_stats.total.sent > 1000, "{host_stats:?}");
    assert!(host_stats.total.lost >= lost * 9 / 10 && host_stats.total.lost <= lost + 2, "{host_stats:?}");
    assert!(host_stats.total.resent >= host_stats.total.lost, "every loss was sent again at least once: {host_stats:?}");
    assert!(host_stats.rtt_ms > 0.0 && host_stats.capacity_mbps > 0.0, "{host_stats:?}");
    assert!((host_stats.recent_mbps() - 3.0).abs() < 1.5, "about the encoder's 3 Mbps: {host_stats:?}");
    // the viewer's: what arrived, and what the receiver went past without it
    assert!(viewer_stats.total.unrecovered >= lost.saturating_sub(3) && viewer_stats.total.unrecovered <= lost, "{viewer_stats:?}");
    assert_eq!(viewer_stats.total.lost, host_stats.total.lost, "the same loss reports pass both monitors");

    // decoded as it arrived: pictures the encoder never made
    let made: HashSet<String> = picture_hashes(&ffmpeg, &reference, &[]).into_iter().collect();
    assert!(made.len() > 200, "the reference has {} pictures", made.len());
    let raw = picture_hashes(&ffmpeg, &received, &[]);
    let broken = raw.iter().filter(|hash| !made.contains(*hash)).count();
    assert!(broken > 0, "nothing was damaged: the test lost nothing that mattered ({} pictures)", raw.len());

    // through the guard: only pictures the encoder made
    let mut guard = TsGuard::new();
    let mut guarded = Vec::new();
    guard.push(&std::fs::read(&received).unwrap(), &mut guarded);
    guard.finish();
    let guarded_file = scratch("guarded.ts");
    std::fs::write(&guarded_file, &guarded).unwrap();
    let shown = picture_hashes(&ffmpeg, &guarded_file, &[]);
    let foreign: Vec<&String> = shown.iter().filter(|hash| !made.contains(*hash)).collect();
    let stats = guard.stats();
    println!("{TEST}: as it came {} pictures, {broken} broken; through the guard {} pictures; {stats}", raw.len(), shown.len());
    assert!(foreign.is_empty(), "{} of {} pictures through the guard were never made by the encoder", foreign.len(), shown.len());
    assert!(stats.freezes >= 1 && stats.recoveries >= 1, "{stats}");
    assert!(shown.len() * 3 >= raw.len(), "the guard kept back too much: {} of {}", shown.len(), raw.len());

    let _ = std::fs::remove_dir_all(reference.parent().unwrap());
}

/// Loses the first send of one in `every` SRT data packets and the first resend of each of those, and notes the
/// timestamp of every copy of them that came: such a packet gets through on its second resend, after SRT's
/// periodic loss report asked for it again.
struct TwiceLost {
    every: u64,
    state: Mutex<Twice>,
}

#[derive(Default)]
struct Twice {
    rng: u64,
    top: Option<u32>,
    /// The packets held back: the timestamps of their copies, in the order they came.
    copies: HashMap<u32, Vec<u32>>,
}

impl PacketFilter for TwiceLost {
    fn on_outbound(&self, _: SocketAddr) {}

    fn allow_inbound(&self, _: SocketAddr) -> bool {
        true
    }

    fn allow_datagram(&self, _: SocketAddr, datagram: &[u8]) -> bool {
        if datagram.len() < 1000 || datagram[0] & 0x80 != 0 {
            return true;
        }
        let seq = u32::from_be_bytes(datagram[..4].try_into().unwrap());
        let timestamp = u32::from_be_bytes(datagram[8..12].try_into().unwrap());
        let mut s = self.state.lock().unwrap();
        if let Some(copies) = s.copies.get_mut(&seq) {
            copies.push(timestamp);
            return copies.len() > 2; // the first send and the first resend do not get through
        }
        let first_send = s.top.is_none_or(|top| {
            let ahead = seq.wrapping_sub(top) & 0x7FFF_FFFF;
            ahead != 0 && ahead < 1 << 30
        });
        if first_send {
            s.top = Some(seq);
            s.rng ^= s.rng << 13;
            s.rng ^= s.rng >> 7;
            s.rng ^= s.rng << 17;
            if s.rng % self.every == 0 {
                s.copies.insert(seq, vec![timestamp]);
                return false;
            }
        }
        true
    }
}

#[test]
fn a_resend_keeps_its_timestamp_and_the_viewer_measures_how_late_it_came() {
    const TEST: &str = "a_resend_keeps_its_timestamp_and_the_viewer_measures_how_late_it_came";
    const LATENCY: u32 = 1000;
    let Some((ffmpeg, _)) = common::media_tools() else {
        common::skip(TEST, "needs ffmpeg with libx265 and libsrt (set HDRCAST_TOOLS to its directory)");
        return;
    };
    let keys = random_keys();
    let mut encoder = KillOnDrop(encoder(&ffmpeg, 6).spawn().expect("the encoder starts"));
    let fan = TsFanOut::new(Box::new(encoder.0.stdout.take().unwrap()));
    let host = HostLink::new(&keys, &common::loopback_v4()).unwrap();
    fan.start().unwrap();

    let sid = keys::new_id();
    let child_port = RemuxChild::free_loopback_udp_port().unwrap();
    // SRT told of far more than the 3 Mbps the encoder makes: its pacing never holds a packet back, so what is late
    // is late because of the resends (the other test shows the pacing: a bursty encoder against a tight rate)
    let mut child = RemuxChild::new(&ffmpeg, &RemuxChild::build_listener_url(child_port, keys.srt_passphrase(), LATENCY, 25, 30_000_000).unwrap(), None);
    child.start().expect("the remux child starts");
    fan.add_sink(&sid, Box::new(child.input().unwrap()), TsFanOut::queue_bytes_for(3_000_000)).unwrap();

    let filter = std::sync::Arc::new(TwiceLost { every: 100, state: Mutex::new(Twice { rng: 0x2545_F491_4F6C_DD1D, ..Twice::default() }) });
    let viewer = ViewerLink::new(
        &keys,
        &LinkAddresses { ipv6: None, ipv4_bind: Some(Ipv4Addr::LOCALHOST), ipv4_advertise: None, filter_v6: None, filter_v4: Some(filter.clone()) },
        0,
    )
    .unwrap();
    host.add_session(&sid, &viewer.candidates(), child_port, None).unwrap();
    viewer.start_session(&sid, &host.candidates()).unwrap();
    assert!(wait_for(|| viewer.state() == LinkState::Connected, 5000), "{}", viewer.reason());

    let url = srt_url::build(&SrtUrlOptions {
        topology: SrtTopology::Caller,
        role: SrtRole::Read,
        host: "127.0.0.1".to_string(),
        port: viewer.local_port(),
        passphrase: Some(keys.srt_passphrase().to_string()),
        latency_ms: LATENCY,
        bitrate_bps: 10_000_000,
        ..SrtUrlOptions::default()
    })
    .unwrap();
    let mut receiver = KillOnDrop(
        Command::new(&ffmpeg)
            .args(mpv_command::ffmpeg_pipe_args(&url))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the receiving ffmpeg starts"),
    );
    let _ = encoder.0.wait();
    thread::sleep(Duration::from_millis(1500));
    let stats = viewer.srt_stats();
    let late = viewer.srt_lateness();
    let _ = receiver.0.kill();
    let _ = receiver.0.wait();
    viewer.dispose();
    child.dispose();
    fan.dispose();
    host.dispose();

    let copies = filter.state.lock().unwrap().copies.clone();
    let through: Vec<&Vec<u32>> = copies.values().filter(|c| c.len() >= 3).collect();
    println!("{TEST}: {} packets held back, {} came on a later resend; {late:?}; {stats:?}", copies.len(), through.len());
    assert!(through.len() >= 5, "too few packets came back to show anything: {copies:?}");
    for c in &through {
        assert!(c.iter().all(|&ts| ts == c[0]), "a resend carries the first send's timestamp: {c:?}");
    }
    // what never came back is the stream's last moments: the remux child stops sending at the end of its input
    assert!(stats.total.unrecovered as usize <= copies.len() - through.len(), "{stats:?}");
    assert!(late.packets + 10 >= stats.total.sent && late.packets <= stats.total.sent + stats.total.resent, "{late:?} {stats:?}");
    // the sender's own timing still shows (on Windows libsrt sends in steps of about 16 ms), but most packets come
    // far sooner than the ones that needed two resends
    assert!(late.p50_ms <= 30.0, "most packets come at once: {late:?}");
    assert!(late.max_ms >= 10.0 && late.max_ms >= 2.0 * late.p50_ms, "a packet that took two resends is late by a loss report or more: {late:?}");
    assert!(late.loss_runs >= through.len() as u64 / 2, "{late:?}");
    // told apart: what came on its first send is late by the sender's timing, a packet that took two resends by at
    // least one periodic loss report (in live mode libsrt sends one every (RTT + 4 RTTVar) / 2 and 20 ms at the
    // least; the loss shows a packet or two after the gap, so a little less is measured). Until the first RTT is
    // measured libsrt takes it as 100 ms, so a loss in the first moments waits far longer.
    assert!(late.recovered >= through.len() as u64, "every packet that came on a resend counts as recovered: {late:?}");
    assert!(late.recovered_p50_ms >= 15.0, "{late:?}");
    assert!(late.first_try_max_ms < late.recovered_max_ms, "{late:?}");
}
