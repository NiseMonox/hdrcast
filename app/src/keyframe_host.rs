//! Keyframes on request, host side (docs/design.md §8.12). One encoder serves every viewer, so viewers' requests
//! (from the side channel) and this host's own reasons (a viewer's stream starting, its queue in the fan-out
//! overflowing) go through one [`KeyframeGovernor`] to the encoder inside the capture engine. A request is
//! answered with what was done about it; a keyframe deferred by the rate limit is made by this hub's thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hdrcast_capture::EncoderEvent;
use hdrcast_media::keyframe::{self, KeyframeGovernor, Request, Verdict};
use hdrcast_p2p::StreamEvent;

use crate::sender_runner::EncoderNews;

type LogFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Sends a keyframe answer to a viewer's session (only to one that announced "kf").
pub type SendFn = Arc<dyn Fn(&str, &[u8]) -> bool + Send + Sync>;
/// Asks the encoder for a keyframe now; false when it cannot (no engine that encodes).
pub type ForceFn = Arc<dyn Fn() -> bool + Send + Sync>;
/// The key a session's requests and answers are signed with.
pub type KeyFn = Arc<dyn Fn(&str) -> [u8; 32] + Send + Sync>;

/// Log lines about requests: this many in a burst, and one more each second; what goes beyond is summed up
/// [`SUMMARY_AFTER`] later.
const LOG_BURST: f64 = 12.0;
const SUMMARY_AFTER: Duration = Duration::from_secs(10);

struct Viewer {
    name: String,
    /// The last request number seen, and what it was answered.
    last: Option<(u32, Vec<u8>)>,
}

struct Inner {
    governor: KeyframeGovernor,
    viewers: HashMap<String, Viewer>,
    /// When the encoder was last asked for a keyframe, for the log line once it went out.
    forced_at: Option<(Instant, String)>,
    log_tokens: f64,
    tokens_at: Instant,
    /// Lines left out since the last summary, and when that summary is due.
    left_out: u32,
    summary_due: Option<Instant>,
}

pub struct KeyframeHub {
    inner: Mutex<Inner>,
    wake: Condvar,
    send: SendFn,
    force: ForceFn,
    key: KeyFn,
    log: LogFn,
    stopped: AtomicBool,
}

impl KeyframeHub {
    /// `gop`: the interval of the keyframes nobody asks for.
    pub fn start(gop: Duration, send: SendFn, force: ForceFn, key: KeyFn, log: LogFn) -> Arc<Self> {
        let hub = Arc::new(Self {
            inner: Mutex::new(Inner {
                governor: KeyframeGovernor::new(gop),
                viewers: HashMap::new(),
                forced_at: None,
                log_tokens: LOG_BURST,
                tokens_at: Instant::now(),
                left_out: 0,
                summary_due: None,
            }),
            wake: Condvar::new(),
            send,
            force,
            key,
            log,
            stopped: AtomicBool::new(false),
        });
        let me = hub.clone();
        thread::Builder::new().name("keyframes-host".into()).spawn(move || me.run()).ok();
        hub
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.wake.notify_all();
    }

    pub fn set_gop(&self, gop: Duration) {
        self.inner.lock().unwrap().governor.set_gop(gop);
    }

    /// A viewer was let in (its name is for the log).
    pub fn register(&self, sid: &str, name: &str) {
        self.inner.lock().unwrap().viewers.insert(sid.to_string(), Viewer { name: name.to_string(), last: None });
    }

    /// Forgets the viewers that left.
    pub fn keep_only(&self, sids: &[String]) {
        self.inner.lock().unwrap().viewers.retain(|sid, _| sids.contains(sid));
    }

    /// What the runner says about the encoder.
    pub fn encoder_news(&self, news: &EncoderNews) {
        let mut inner = self.inner.lock().unwrap();
        match news {
            EncoderNews::Started { in_engine } => {
                inner.governor.restarted();
                inner.governor.set_can_force(*in_engine);
                inner.forced_at = None;
            }
            EncoderNews::Event(EncoderEvent::Keyframe { asked, waited_ms, bytes, .. }) => {
                let now = Instant::now();
                inner.governor.keyframe_sent(now);
                if *asked {
                    if let Some((at, why)) = inner.forced_at.take() {
                        let line = format!(
                            "[keyframe] a keyframe went out {} ms after {why} ({} in the encoder, {} KB)",
                            now.saturating_duration_since(at).as_millis(),
                            waited_ms.map_or_else(String::new, |ms| format!("{ms:.0} ms")),
                            bytes / 1024
                        );
                        drop(inner);
                        self.note(&line);
                    }
                }
            }
            EncoderNews::Event(EncoderEvent::Bitrate { kbps, ok }) => {
                drop(inner);
                (self.log)(&format!("[keyframe] the encoder {} {kbps} kbit/s", if *ok { "now runs at" } else { "could not change to" }));
            }
        }
    }

    /// Something happened to a viewer's stream that leaves it without a picture until the next keyframe.
    pub fn stream_event(&self, sid: &str, event: StreamEvent) {
        let why = match event {
            StreamEvent::Started => "its stream started",
            StreamEvent::QueueCleared => "half a second of its stream was dropped here (it fell behind)",
        };
        let mut inner = self.inner.lock().unwrap();
        if !inner.governor.can_force() {
            return; // ffmpeg encodes: nothing to do, and nobody asked
        }
        let name = inner.viewers.get(sid).map_or_else(|| "a viewer".to_string(), |v| v.name.clone());
        let now = Instant::now();
        let verdict = inner.governor.request(now, Duration::ZERO);
        let line = self.decided(&mut inner, now, verdict, &format!("{name}: {why}"));
        drop(inner);
        self.wake.notify_all();
        if let Some(line) = line {
            self.note(&line);
        }
    }

    /// A side-channel datagram from a viewer (the link checked where it came from): a keyframe request.
    pub fn on_datagram(&self, sid: &str, payload: &[u8]) {
        if self.stopped.load(Ordering::SeqCst) || payload.first() != Some(&keyframe::REQUEST) {
            return;
        }
        let Some(request) = Request::open(&(self.key)(sid), payload) else { return };
        let mut inner = self.inner.lock().unwrap();
        let Some(viewer) = inner.viewers.get(sid) else { return };
        let name = viewer.name.clone();
        match &viewer.last {
            // the answer was lost on the way: the same answer again
            Some((seq, answer)) if *seq == request.seq => {
                let answer = answer.clone();
                drop(inner);
                (self.send)(sid, &answer);
                return;
            }
            Some((seq, _)) if request.seq < *seq => return, // late or replayed
            _ => {}
        }
        let now = Instant::now();
        let verdict = inner.governor.request(now, Duration::from_millis(u64::from(request.ago_ms)));
        let why = format!("{name} asked ({}, {} ms ago)", request.reason.describe(), request.ago_ms);
        let line = self.decided(&mut inner, now, verdict, &why);
        let answer = verdict.answer(request.seq).seal(&(self.key)(sid));
        if let Some(viewer) = inner.viewers.get_mut(sid) {
            viewer.last = Some((request.seq, answer.clone()));
        }
        drop(inner);
        self.wake.notify_all();
        (self.send)(sid, &answer);
        if let Some(line) = line {
            self.note(&line);
        }
    }

    /// Carries out a verdict (a keyframe now) and says what happened, for the log.
    fn decided(&self, inner: &mut Inner, now: Instant, verdict: Verdict, why: &str) -> Option<String> {
        match verdict {
            Verdict::Force => {
                if (self.force)() {
                    inner.forced_at = Some((now, why.to_string()));
                    Some(format!("[keyframe] {why}: a keyframe now"))
                } else {
                    // the engine is not there (yet): nothing was made
                    inner.governor.set_can_force(false);
                    Some(format!("[keyframe] {why}: the encoder cannot make one right now"))
                }
            }
            Verdict::AlreadySent(ago) => Some(format!("[keyframe] {why}: one went out {} ms ago, after the loss", ago.as_millis())),
            Verdict::Coming(soon) => Some(format!("[keyframe] {why}: the next one comes in {} ms anyway", soon.as_millis())),
            Verdict::Deferred(soon) => Some(format!("[keyframe] {why}: in {} ms (one at most every {} ms)", soon.as_millis(), KeyframeGovernor::MIN_INTERVAL.as_millis())),
            Verdict::Cannot => Some(format!("[keyframe] {why}: ffmpeg encodes here, it cannot make keyframes on request")),
        }
    }

    /// A log line, unless lines come faster than one a second for long (then they are counted and summed up).
    fn note(&self, line: &str) {
        let show = {
            let mut inner = self.inner.lock().unwrap();
            let now = Instant::now();
            let refill = now.saturating_duration_since(inner.tokens_at).as_secs_f64();
            inner.log_tokens = (inner.log_tokens + refill).min(LOG_BURST);
            inner.tokens_at = now;
            if inner.log_tokens >= 1.0 {
                inner.log_tokens -= 1.0;
                true
            } else {
                inner.left_out += 1;
                inner.summary_due.get_or_insert(now + SUMMARY_AFTER);
                false
            }
        };
        if show {
            (self.log)(line);
        } else {
            self.wake.notify_all();
        }
    }

    fn run(&self) {
        while !self.stopped.load(Ordering::SeqCst) {
            let mut inner = self.inner.lock().unwrap();
            let now = Instant::now();
            if inner.governor.poll(now) {
                let made = (self.force)();
                if made {
                    inner.forced_at = Some((now, "the request the rate limit held back".to_string()));
                }
                drop(inner);
                if made {
                    self.note("[keyframe] the keyframe held back by the rate limit: now");
                }
                continue;
            }
            if inner.summary_due.is_some_and(|due| now >= due) {
                let left_out = std::mem::take(&mut inner.left_out);
                inner.summary_due = None;
                drop(inner);
                (self.log)(&format!("[keyframe] {left_out} more lines like the ones above were left out in the last {} s", SUMMARY_AFTER.as_secs()));
                continue;
            }
            let wait = [inner.governor.next_poll(), inner.summary_due]
                .into_iter()
                .flatten()
                .map(|at| at.saturating_duration_since(now))
                .min()
                .unwrap_or(Duration::from_secs(1))
                .clamp(Duration::from_millis(1), Duration::from_secs(1));
            let _ = self.wake.wait_timeout(inner, wait);
        }
    }
}
