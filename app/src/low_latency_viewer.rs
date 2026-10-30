//! Control mode, viewer side (docs/design.md §11). The host's sealed side datagrams are opened here: the shards of
//! each picture are put together (the Reed-Solomon parity standing in for lost ones), and every complete picture is
//! written into mpv's stdin at once, followed by an access unit delimiter so that mpv's parser hands it on without
//! waiting for the next one. The sound goes into a small buffer that the sound card pulls from.
//!
//! When a picture is lost the decoder cannot go on with the ones after it: the picture on screen stays as it is
//! (nothing is written), and the host is asked for a keyframe, again every so often until one comes. A new mpv
//! (started, or restarted after a crash) waits for a keyframe the same way. Every half second a report goes to the
//! host; its echo gives the round trip.

use std::io::Write;
use std::process::ChildStdin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use hdrcast_capture::playback::SoundOutput;
use hdrcast_media::low_latency::audio::{self, AudioJitter};
use hdrcast_media::low_latency::messages::{Answer, Counts, Outcome, Request};
use hdrcast_media::low_latency::seal::{Direction, Opener, Sealer};
use hdrcast_media::low_latency::video::{Assembler, Event};
use hdrcast_media::low_latency::{self, ANSWER, AUDIO, REQUEST, VIDEO};
use hdrcast_p2p::mono_clock;
use serde_json::{json, Value};

type LogFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Sends one datagram of control mode to the host (only in a session that joined in control mode).
pub type SendFn = Arc<dyn Fn(&[u8]) -> bool + Send + Sync>;

/// An access unit delimiter: ends the picture before it for mpv's parser (HEVC NAL type 35, any picture type).
const AUD: [u8; 7] = [0, 0, 0, 1, 0x46, 0x01, 0x50];
const TICK: Duration = Duration::from_millis(10);
const REPORT_EVERY: Duration = Duration::from_millis(500);
/// Pictures that may wait for mpv; more means mpv has fallen behind, and the stream starts again at a keyframe.
const PLAYER_QUEUE: usize = 3;

enum ToPlayer {
    Picture(Vec<u8>),
    /// A new mpv: write into this from now on.
    Stdin(ChildStdin),
}

struct State {
    sid: Option<String>,
    opener: Option<Opener>,
    sealer: Option<Sealer>,
    assembler: Assembler,
    events: Vec<Event>,
    /// No keyframe since the stream started, a picture was lost, or mpv is new: nothing is shown until one comes.
    waiting: bool,
    /// Since when the picture has been held (a loss), for the page and the log.
    frozen_since: Option<Instant>,
    frozen_total: Duration,
    seq: u32,
    /// The newest picture known lost (what the next request names), and when the last request went.
    lost: u32,
    asked_at: Option<Instant>,
    rtt: Option<Duration>,
    counts: Counts,
    host_us: u32,
    encode_us: u32,
    report_at: Instant,
    bytes: u64,
    /// Bytes and pictures shown at the previous look, for the rates on the page (mpv's own frame rate means nothing
    /// here: the stream has no timestamps).
    rate_mark: Option<(u64, u32, Instant)>,
    mbps: f64,
    fps: f64,
    /// A host that cannot make keyframes now (its engine stopped): said once in the log.
    told_cannot: bool,
}

impl State {
    fn new() -> State {
        State {
            sid: None,
            opener: None,
            sealer: None,
            assembler: Assembler::new(),
            events: Vec::new(),
            waiting: true,
            frozen_since: None,
            frozen_total: Duration::ZERO,
            seq: 0,
            lost: 0,
            asked_at: None,
            rtt: None,
            counts: Counts::default(),
            host_us: 0,
            encode_us: 0,
            report_at: Instant::now(),
            bytes: 0,
            rate_mark: None,
            mbps: 0.0,
            fps: 0.0,
            told_cannot: false,
        }
    }

    /// How long to give a keyframe request before asking again: a round trip and a half, within reason.
    fn retry_after(&self) -> Duration {
        self.rtt.map_or(Duration::from_millis(60), |rtt| rtt.mul_f64(1.5)).clamp(Duration::from_millis(20), Duration::from_millis(250))
    }
}

pub struct LowLatencyLink {
    state: Mutex<State>,
    send: SendFn,
    log: LogFn,
    player: SyncSender<ToPlayer>,
    jitter: Arc<Mutex<AudioJitter>>,
    sound: Mutex<Option<SoundOutput>>,
    muted: Arc<AtomicBool>,
    stopped: AtomicBool,
    /// The current mpv has taken a picture: it is set up and reading (a new one reads nothing for a few hundred ms).
    player_reading: AtomicBool,
}

impl LowLatencyLink {
    pub fn start(send: SendFn, log: LogFn, muted: bool) -> Arc<LowLatencyLink> {
        let (player, pictures) = mpsc::sync_channel(PLAYER_QUEUE);
        let link = Arc::new(LowLatencyLink {
            state: Mutex::new(State::new()),
            send,
            log,
            player,
            jitter: Arc::new(Mutex::new(AudioJitter::default())),
            sound: Mutex::new(None),
            muted: Arc::new(AtomicBool::new(muted)),
            stopped: AtomicBool::new(false),
            player_reading: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&link);
        thread::Builder::new().name("ll-player".into()).spawn(move || write_pictures(weak, pictures)).ok();
        let weak = Arc::downgrade(&link);
        thread::Builder::new().name("ll-tick".into()).spawn(move || tick(weak)).ok();
        link
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.sound.lock().unwrap().take();
        let mut s = self.state.lock().unwrap();
        s.sid = None;
        s.opener = None;
        s.sealer = None;
    }

    /// A session in control mode connected, with its remote-control key: its datagrams are opened from now on.
    pub fn session(&self, sid: &str, control_key: &[u8; 32]) {
        {
            let mut s = self.state.lock().unwrap();
            if s.sid.as_deref() == Some(sid) {
                return;
            }
            let key = low_latency::session_key(control_key);
            let fresh = State { sid: Some(sid.to_string()), opener: Some(Opener::new(&key, Direction::ToViewer)), sealer: Some(Sealer::new(&key, Direction::ToHost)), ..State::new() };
            *s = fresh;
        }
        self.start_sound();
    }

    /// The session ended (it may join again): datagrams are dropped until the next one.
    pub fn session_ended(&self) {
        let mut s = self.state.lock().unwrap();
        s.sid = None;
        s.opener = None;
        s.sealer = None;
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::SeqCst);
    }

    /// Where mpv's stdin goes: each mpv the player runner starts hands it over here.
    pub fn feed(self: &Arc<Self>) -> crate::mpv_runner::StdinFeed {
        let weak = Arc::downgrade(self);
        Arc::new(move |stdin| {
            if let Some(link) = weak.upgrade() {
                let _ = link.player.send(ToPlayer::Stdin(stdin));
            }
        })
    }

    fn start_sound(&self) {
        let mut sound = self.sound.lock().unwrap();
        if sound.is_some() {
            return;
        }
        let (jitter, muted) = (self.jitter.clone(), self.muted.clone());
        let pull = Box::new(move |out: &mut [i16]| {
            jitter.lock().unwrap().pull(out);
            if muted.load(Ordering::Relaxed) {
                out.fill(0);
            }
        });
        match SoundOutput::start(pull) {
            Ok(output) => *sound = Some(output),
            Err(e) => (self.log)(&format!("[ll] no sound: {e}")),
        }
    }

    /// A datagram of control mode from the host (on a network thread).
    pub fn on_datagram(&self, datagram: &[u8]) {
        let mut s = self.state.lock().unwrap();
        let Some(opener) = s.opener.as_mut() else { return };
        let Some((kind, message)) = opener.open(datagram) else { return };
        let message = message.to_vec();
        s.bytes += datagram.len() as u64;
        match kind {
            VIDEO => {
                let mut events = std::mem::take(&mut s.events);
                s.assembler.push(&message, Instant::now(), &mut events);
                for event in events.drain(..) {
                    self.handle(&mut s, event);
                }
                s.events = events;
            }
            AUDIO => {
                if let Some((seq, samples)) = audio::parse(&message) {
                    self.jitter.lock().unwrap().push(seq, &samples);
                }
            }
            ANSWER => match Answer::parse(&message) {
                Some(Answer::Echo { sent_us, held_us }) => {
                    let elapsed = (mono_clock::now_us() as u64).saturating_sub(sent_us).saturating_sub(u64::from(held_us));
                    s.rtt = Some(Duration::from_micros(elapsed));
                }
                Some(Answer::Keyframe { outcome: Outcome::Cannot, .. }) if !s.told_cannot => {
                    s.told_cannot = true;
                    (self.log)("[ll] the host cannot make a keyframe right now (its encoder is restarting?)");
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn handle(&self, s: &mut State, event: Event) {
        match event {
            Event::Picture(picture) => {
                s.host_us = picture.host_us;
                s.encode_us = picture.encode_us;
                s.counts.restored_shards += picture.recovered as u32;
                if s.waiting && !picture.keyframe {
                    // the decoder has nothing to go on: hold the picture, and ask (again) when it is time
                    s.lost = picture.frame;
                    self.ask(s, false);
                    return;
                }
                let mut unit = picture.data;
                unit.extend_from_slice(&AUD);
                match self.player.try_send(ToPlayer::Picture(unit)) {
                    Ok(()) => {
                        s.counts.shown += 1;
                        if s.waiting {
                            s.waiting = false;
                            if let Some(since) = s.frozen_since.take() {
                                s.frozen_total += since.elapsed();
                            }
                        }
                    }
                    Err(TrySendError::Full(_)) => {
                        // a new mpv reads nothing until it is set up: that is not worth a line
                        if !s.waiting && self.player_reading.load(Ordering::SeqCst) {
                            (self.log)("[ll] the player fell behind: starting again at a keyframe");
                        }
                        self.freeze(s, picture.frame);
                    }
                    Err(TrySendError::Disconnected(_)) => {}
                }
            }
            Event::Lost { first, last } => {
                s.counts.lost += last.wrapping_sub(first) + 1;
                self.freeze(s, last);
            }
        }
    }

    /// Holds the picture from now on and asks for a keyframe: at once when the picture just froze, otherwise when the
    /// last request had its time (more losses while waiting do not make the host hurry more).
    fn freeze(&self, s: &mut State, lost: u32) {
        let began = !s.waiting;
        if began {
            s.waiting = true;
            s.counts.keyframe_waits += 1;
            s.frozen_since.get_or_insert_with(Instant::now);
            s.asked_at = None;
        }
        s.lost = lost;
        self.ask(s, began);
    }

    /// Sends a keyframe request when one is due (`now`: whatever the last one was).
    fn ask(&self, s: &mut State, now: bool) {
        if !now && s.asked_at.is_some_and(|at| at.elapsed() < s.retry_after()) {
            return;
        }
        s.seq += 1;
        let request = Request::Keyframe { seq: s.seq, lost: s.lost };
        if self.seal_and_send(s, &request) {
            s.asked_at = Some(Instant::now());
        }
    }

    fn seal_and_send(&self, s: &mut State, request: &Request) -> bool {
        let Some(sealer) = s.sealer.as_mut() else { return false };
        let mut datagram = Vec::new();
        sealer.seal(REQUEST, &request.to_bytes(), &mut datagram);
        (self.send)(&datagram)
    }

    /// The new mpv (or none) is fed from the next keyframe on.
    fn player_changed(&self) {
        let mut s = self.state.lock().unwrap();
        let lost = s.lost;
        self.freeze(&mut s, lost);
    }

    fn on_tick(&self) {
        let mut s = self.state.lock().unwrap();
        if s.opener.is_none() {
            return;
        }
        let mut events = std::mem::take(&mut s.events);
        s.assembler.tick(Instant::now(), &mut events);
        for event in events.drain(..) {
            self.handle(&mut s, event);
        }
        s.events = events;
        if s.waiting && s.asked_at.is_some() {
            self.ask(&mut s, false);
        }
        if s.report_at.elapsed() >= REPORT_EVERY {
            s.report_at = Instant::now();
            let report = Request::Report {
                sent_us: mono_clock::now_us() as u64,
                rtt_us: s.rtt.map_or(0, |rtt| rtt.as_micros().min(u128::from(u32::MAX)) as u32),
                counts: s.counts,
            };
            self.seal_and_send(&mut s, &report);
        }
    }

    /// For the page and the panel over the player.
    pub fn status(&self) -> Value {
        let mut s = self.state.lock().unwrap();
        let now = Instant::now();
        match s.rate_mark {
            Some((bytes, shown, at)) if now.saturating_duration_since(at) >= Duration::from_millis(500) => {
                let seconds = now.saturating_duration_since(at).as_secs_f64();
                s.mbps = (s.bytes - bytes) as f64 * 8.0 / seconds / 1e6;
                s.fps = f64::from(s.counts.shown.wrapping_sub(shown)) / seconds;
                s.rate_mark = Some((s.bytes, s.counts.shown, now));
            }
            None => s.rate_mark = Some((s.bytes, s.counts.shown, now)),
            _ => {}
        }
        let frozen = s.frozen_total + s.frozen_since.map_or(Duration::ZERO, |since| since.elapsed());
        let sound = self.jitter.lock().unwrap().stats();
        json!({
            "rttMs": s.rtt.map(|rtt| rtt.as_secs_f64() * 1000.0),
            "shown": s.counts.shown,
            "lost": s.counts.lost,
            "restored": s.counts.restored_shards,
            "keyframeWaits": s.counts.keyframe_waits,
            "waiting": s.waiting,
            "frozenSec": frozen.as_secs_f64(),
            "hostMs": f64::from(s.host_us) / 1000.0,
            "encodeMs": f64::from(s.encode_us) / 1000.0,
            "mbps": s.mbps,
            "fps": s.fps,
            "sound": self.sound.lock().unwrap().is_some(),
            "soundLost": sound.lost,
            "soundUnderruns": sound.underruns,
        })
    }
}

/// Writes complete pictures into mpv's stdin, on a thread of its own: a stalled pipe never holds the network.
fn write_pictures(link: Weak<LowLatencyLink>, pictures: Receiver<ToPlayer>) {
    let mut stdin: Option<ChildStdin> = None;
    for item in pictures {
        match item {
            ToPlayer::Stdin(new) => {
                stdin = Some(new);
                if let Some(link) = link.upgrade() {
                    link.player_reading.store(false, Ordering::SeqCst);
                    link.player_changed();
                }
            }
            ToPlayer::Picture(unit) => {
                let Some(pipe) = stdin.as_mut() else { continue };
                if pipe.write_all(&unit).and_then(|()| pipe.flush()).is_err() {
                    // mpv is gone: the runner starts another one, which hands its stdin over
                    stdin = None;
                } else if let Some(link) = link.upgrade() {
                    link.player_reading.store(true, Ordering::SeqCst);
                }
            }
        }
        if link.upgrade().is_none_or(|link| link.stopped.load(Ordering::SeqCst)) {
            break;
        }
    }
}

fn tick(link: Weak<LowLatencyLink>) {
    loop {
        thread::sleep(TICK);
        let Some(link) = link.upgrade() else { return };
        if link.stopped.load(Ordering::SeqCst) {
            return;
        }
        link.on_tick();
    }
}
