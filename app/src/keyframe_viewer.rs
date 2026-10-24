//! Keyframes on request, viewer side (docs/design.md §8.12). When part of the stream is lost the host is asked
//! for a fresh keyframe over the side channel: as soon as data has been missing on the way in for longer than
//! SRT needs to get it again (the picture that needs it will hold when it gets to the player, a latency later),
//! and when the guard (§3.2) finds a damaged picture as it is about to be shown. One request is open at a time,
//! sent again until it is answered; only a host whose live record said "kf" is asked.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hdrcast_media::keyframe::{self, Answer, KeyframeAsker, Outcome, Reason, Request};
use hdrcast_media::receiver::ts_guard::GuardEvent;

type LogFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Sends a datagram to the host on the side channel; false when it cannot go (no path, or the host does not
/// take keyframe requests).
pub type SendFn = Arc<dyn Fn(&[u8]) -> bool + Send + Sync>;

const LOG_BURST: f64 = 12.0;
/// A freeze is put down to a keyframe asked for when one was asked for this long before it ended (a latency and
/// a little more: the request for a lasting gap goes out long before the picture gets to the player).
const ASKED_WITHIN_EXTRA: Duration = Duration::from_secs(2);

struct Inner {
    /// The current session and its key; `None` while not connected (or the host takes no requests).
    session: Option<(String, [u8; 32])>,
    asker: KeyframeAsker,
    latency: Duration,
    rtt: Duration,
    last_asked: Option<Instant>,
    log_tokens: f64,
    tokens_at: Instant,
}

pub struct KeyframeLink {
    inner: Mutex<Inner>,
    wake: Condvar,
    send: SendFn,
    log: LogFn,
    stopped: AtomicBool,
}

fn millis(d: Duration) -> u128 {
    d.as_millis()
}

impl KeyframeLink {
    /// `latency`: this viewer's SRT latency, how far behind the host the player runs.
    pub fn start(latency: Duration, send: SendFn, log: LogFn) -> Arc<Self> {
        let link = Arc::new(Self {
            inner: Mutex::new(Inner {
                session: None,
                asker: KeyframeAsker::new(),
                latency,
                rtt: Duration::ZERO,
                last_asked: None,
                log_tokens: LOG_BURST,
                tokens_at: Instant::now(),
            }),
            wake: Condvar::new(),
            send,
            log,
            stopped: AtomicBool::new(false),
        });
        let me = link.clone();
        thread::Builder::new().name("keyframes-viewer".into()).spawn(move || me.run()).ok();
        link
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.wake.notify_all();
    }

    /// Connected in session `sid` to a host that takes requests; their key. A new session starts afresh.
    pub fn session(&self, sid: &str, key: [u8; 32]) {
        let mut inner = self.inner.lock().unwrap();
        if inner.session.as_ref().is_some_and(|(current, _)| current == sid) {
            return;
        }
        inner.session = Some((sid.to_string(), key));
        inner.asker = KeyframeAsker::new();
    }

    /// Not connected, or the host takes no requests: nothing is asked.
    pub fn session_ended(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.session = None;
        inner.asker = KeyframeAsker::new();
    }

    /// The round trip SRT measures on the path.
    pub fn set_rtt(&self, rtt: Duration) {
        self.inner.lock().unwrap().rtt = rtt;
    }

    /// Data of the stream has been missing on the way in for this long (on a network thread).
    pub fn lasting_loss(&self, missing: Duration) {
        let one_way = self.inner.lock().unwrap().rtt / 2;
        self.ask(missing + one_way, Reason::Missing);
    }

    /// What the guard did (on the player's pump thread).
    pub fn guard_event(&self, event: GuardEvent) {
        match event {
            GuardEvent::Froze => {
                // the damaged picture left the host about one latency ago
                let latency = self.inner.lock().unwrap().latency;
                self.ask(latency, Reason::Froze);
            }
            GuardEvent::Recovered { skipped, ticks } => {
                let asked = {
                    let inner = self.inner.lock().unwrap();
                    inner.session.is_some() && inner.last_asked.is_some_and(|at| at.elapsed() <= inner.latency + ASKED_WITHIN_EXTRA)
                };
                if asked {
                    self.note(&format!(
                        "[keyframe] the picture moves again: it held {} ms of the stream ({skipped} pictures) until the keyframe",
                        ticks / 90
                    ));
                }
            }
        }
    }

    fn ask(&self, ago: Duration, reason: Reason) {
        let datagram = {
            let mut inner = self.inner.lock().unwrap();
            let Some((_, key)) = inner.session else { return };
            let now = Instant::now();
            inner.last_asked = Some(now);
            inner.asker.ask(now, ago, reason).map(|request| request.seal(&key))
        };
        if let Some(datagram) = datagram {
            (self.send)(&datagram);
            self.note(&format!("[keyframe] asked the host for a keyframe: {} ({} ms ago)", reason.describe(), millis(ago)));
        }
        self.wake.notify_all();
    }

    /// A side-channel datagram from the host of this kind (on a network thread): the answer to a request.
    pub fn on_datagram(&self, payload: &[u8]) {
        if payload.first() != Some(&keyframe::ANSWER) {
            return;
        }
        let settled = {
            let mut inner = self.inner.lock().unwrap();
            let Some((_, key)) = inner.session else { return };
            let Some(answer) = Answer::open(&key, payload) else { return };
            inner.asker.answered(Instant::now(), &answer).map(|(request, took)| (request, took, answer))
        };
        self.wake.notify_all();
        let Some((request, took, answer)) = settled else { return };
        let what = match answer.outcome {
            Outcome::Forced => "is making a keyframe".to_string(),
            Outcome::AlreadySent => format!("had sent one {} ms before, after the loss", answer.ms),
            Outcome::Coming => format!("sends its next one in {} ms anyway", answer.ms),
            Outcome::Deferred => format!("makes one in {} ms", answer.ms),
            Outcome::Cannot => "cannot make keyframes right now (its ffmpeg encodes)".to_string(),
        };
        self.note(&format!("[keyframe] the host {what} (request {} answered after {} ms)", request.seq, millis(took)));
    }

    fn note(&self, line: &str) {
        let show = {
            let mut inner = self.inner.lock().unwrap();
            let now = Instant::now();
            let refill = now.saturating_duration_since(inner.tokens_at).as_secs_f64();
            inner.log_tokens = (inner.log_tokens + refill).min(LOG_BURST);
            inner.tokens_at = now;
            let show = inner.log_tokens >= 1.0;
            if show {
                inner.log_tokens -= 1.0;
            }
            show
        };
        if show {
            (self.log)(line);
        }
    }

    fn run(&self) {
        while !self.stopped.load(Ordering::SeqCst) {
            let mut inner = self.inner.lock().unwrap();
            let resend: Option<(Request, [u8; 32])> = match inner.session {
                Some((_, key)) => inner.asker.poll(Instant::now()).map(|request| (request, key)),
                None => None,
            };
            let waiting = inner.asker.waiting();
            if let Some((request, key)) = resend {
                drop(inner);
                (self.send)(&request.seal(&key));
                continue;
            }
            let wait = if waiting { KeyframeAsker::RESEND / 5 } else { Duration::from_secs(1) };
            let _ = self.wake.wait_timeout(inner, wait);
        }
    }
}
