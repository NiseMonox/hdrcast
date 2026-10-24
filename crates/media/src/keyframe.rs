//! Keyframes on request (docs/design.md §8.12). A viewer that lost part of the stream asks the host for a fresh
//! keyframe over the side channel (§8.9), and the host's encoder, which runs inside HdrCast's capture engine
//! (§2.8), makes one unless one is on its way already. One encode serves every viewer, so the host decides for
//! all of them together ([`KeyframeGovernor`]); the viewer keeps one request open at a time and sends it again
//! until it is answered ([`KeyframeAsker`]).
//!
//! The datagrams (the side channel's payload, 26 bytes):
//!
//! ```text
//!  0  1  kind   'K' (0x4B) a request, viewer → host; 'k' (0x6B) its answer, host → viewer
//!  1  4  seq    the viewer's request number in this session, from 1 (big-endian); the answer repeats it
//!  5  4  ms     request: how long ago the lost data left the host, as the viewer estimates it;
//!               answer: how long ago the keyframe that serves it went out, or how soon it will
//!  9  1  why    request: a [`Reason`]; answer: an [`Outcome`]
//! 10 16  mac    HMAC-SHA256(GroupKeys::keyframe_key(sid), bytes 0..10), its first 16 bytes
//! ```
//!
//! Remote control's datagrams on the same channel start with its direction byte, 1 or 2, so the two never mix.
//! Both ends send these only to a peer whose live record / join carried `"kf":true`.

use std::time::{Duration, Instant};

use hdrcast_core::group::keys;

pub const REQUEST: u8 = b'K';
pub const ANSWER: u8 = b'k';
pub const LENGTH: usize = 26;
const SIGNED: usize = 10;
const MAC_BYTES: usize = 16;

/// The datagram belongs to keyframe requests (a request or an answer), whatever else is wrong with it.
pub fn is_keyframe_datagram(payload: &[u8]) -> bool {
    matches!(payload.first(), Some(&REQUEST) | Some(&ANSWER))
}

/// Why a viewer asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Its guard (§3.2) found a damaged picture as it was about to be shown: the picture holds.
    Froze = 1,
    /// Data of the stream is still missing on the path after SRT had time to send it again: the picture is
    /// going to hold when it gets there.
    Missing = 2,
}

impl Reason {
    fn from_u8(v: u8) -> Option<Reason> {
        match v {
            1 => Some(Reason::Froze),
            2 => Some(Reason::Missing),
            _ => None,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Reason::Froze => "a damaged picture reached the player",
            Reason::Missing => "part of the stream is still missing on the way",
        }
    }
}

/// What the host did about a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It made a keyframe now.
    Forced = 1,
    /// One went out after the loss, `ms` ago: it will end the freeze.
    AlreadySent = 2,
    /// A scheduled one goes out in `ms`.
    Coming = 3,
    /// One will be made in `ms` (the last one asked for was too recent).
    Deferred = 4,
    /// The host's encoder cannot make keyframes on request right now (ffmpeg encodes).
    Cannot = 5,
}

impl Outcome {
    fn from_u8(v: u8) -> Option<Outcome> {
        match v {
            1 => Some(Outcome::Forced),
            2 => Some(Outcome::AlreadySent),
            3 => Some(Outcome::Coming),
            4 => Some(Outcome::Deferred),
            5 => Some(Outcome::Cannot),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub seq: u32,
    /// How long ago the lost data left the host, in ms (the viewer's estimate).
    pub ago_ms: u32,
    pub reason: Reason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    pub seq: u32,
    pub outcome: Outcome,
    /// See [`Outcome`]: since or until the keyframe, in ms.
    pub ms: u32,
}

fn seal(key: &[u8; 32], kind: u8, seq: u32, ms: u32, why: u8) -> Vec<u8> {
    let mut d = Vec::with_capacity(LENGTH);
    d.push(kind);
    d.extend_from_slice(&seq.to_be_bytes());
    d.extend_from_slice(&ms.to_be_bytes());
    d.push(why);
    let mac = keys::hmac_sha256(key, &d);
    d.extend_from_slice(&mac[..MAC_BYTES]);
    d
}

/// (seq, ms, why) of a datagram of `kind` whose MAC holds.
fn open(key: &[u8; 32], kind: u8, d: &[u8]) -> Option<(u32, u32, u8)> {
    if d.len() != LENGTH || d[0] != kind {
        return None;
    }
    let mac = keys::hmac_sha256(key, &d[..SIGNED]);
    if !keys::fixed_time_eq(&mac[..MAC_BYTES], &d[SIGNED..]) {
        return None;
    }
    let word = |at: usize| u32::from_be_bytes(d[at..at + 4].try_into().expect("four bytes"));
    Some((word(1), word(5), d[9]))
}

impl Request {
    pub fn seal(&self, key: &[u8; 32]) -> Vec<u8> {
        seal(key, REQUEST, self.seq, self.ago_ms, self.reason as u8)
    }

    /// `None` unless it is a request signed with `key` (this session's).
    pub fn open(key: &[u8; 32], datagram: &[u8]) -> Option<Request> {
        let (seq, ago_ms, why) = open(key, REQUEST, datagram)?;
        Some(Request { seq, ago_ms, reason: Reason::from_u8(why)? })
    }
}

impl Answer {
    pub fn seal(&self, key: &[u8; 32]) -> Vec<u8> {
        seal(key, ANSWER, self.seq, self.ms, self.outcome as u8)
    }

    pub fn open(key: &[u8; 32], datagram: &[u8]) -> Option<Answer> {
        let (seq, ms, why) = open(key, ANSWER, datagram)?;
        Some(Answer { seq, outcome: Outcome::from_u8(why)?, ms })
    }
}

fn millis(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

/// The host's decision for one request (or a loss it saw itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Make a keyframe now: the caller tells the encoder.
    Force,
    /// One went out after the loss, this long ago.
    AlreadySent(Duration),
    /// A scheduled one goes out in this long.
    Coming(Duration),
    /// One will be made in this long ([`KeyframeGovernor::poll`] says when).
    Deferred(Duration),
    /// The encoder in use cannot make keyframes on request.
    Cannot,
}

impl Verdict {
    /// The answer to the viewer's request `seq`.
    pub fn answer(self, seq: u32) -> Answer {
        let (outcome, ms) = match self {
            Verdict::Force => (Outcome::Forced, 0),
            Verdict::AlreadySent(ago) => (Outcome::AlreadySent, millis(ago)),
            Verdict::Coming(soon) => (Outcome::Coming, millis(soon)),
            Verdict::Deferred(soon) => (Outcome::Deferred, millis(soon)),
            Verdict::Cannot => (Outcome::Cannot, 0),
        };
        Answer { seq, outcome, ms }
    }
}

/// When the host's one encoder makes a keyframe for whoever lost part of the stream (docs/design.md §8.12).
///
/// * A keyframe that went out after the loss (asked for by someone else, or a scheduled one) serves it already.
/// * A scheduled keyframe due within [`SOON`](Self::SOON) is waited for.
/// * At most one keyframe is asked for every [`MIN_INTERVAL`](Self::MIN_INTERVAL): one encode serves all
///   viewers, and each keyframe costs every one of them a bigger picture. A request inside the interval is not
///   dropped but deferred to its end, and then dropped only if a keyframe went out meanwhile.
#[derive(Debug, Clone)]
pub struct KeyframeGovernor {
    gop: Duration,
    can_force: bool,
    last_keyframe: Option<Instant>,
    last_forced: Option<Instant>,
    /// A keyframe to make at `.0`, for losses up to `.1` (the latest): a keyframe after that serves them all.
    deferred: Option<(Instant, Instant)>,
}

impl KeyframeGovernor {
    pub const MIN_INTERVAL: Duration = Duration::from_millis(300);
    pub const SOON: Duration = Duration::from_millis(150);
    /// How late a scheduled keyframe may be and still count as coming (the encoder's report of one takes a
    /// moment to arrive).
    const LATE: Duration = Duration::from_millis(50);

    /// `gop`: the interval of the keyframes nobody asks for.
    pub fn new(gop: Duration) -> Self {
        Self { gop, can_force: false, last_keyframe: None, last_forced: None, deferred: None }
    }

    /// Whether the encoder in use can make keyframes on request (the one inside the capture engine can; ffmpeg's
    /// cannot). Off: every request is answered [`Verdict::Cannot`] and what was deferred is forgotten.
    pub fn set_can_force(&mut self, can: bool) {
        self.can_force = can;
        if !can {
            self.deferred = None;
        }
    }

    pub fn can_force(&self) -> bool {
        self.can_force
    }

    pub fn set_gop(&mut self, gop: Duration) {
        self.gop = gop;
    }

    /// The encoder made a keyframe at `at` (asked for or scheduled).
    pub fn keyframe_sent(&mut self, at: Instant) {
        self.last_keyframe = Some(self.last_keyframe.map_or(at, |k| k.max(at)));
    }

    /// The encoder started again: its schedule starts over with its first keyframe.
    pub fn restarted(&mut self) {
        self.last_keyframe = None;
        self.deferred = None;
    }

    /// A loss: the data lost left this host `ago` before `now`.
    pub fn request(&mut self, now: Instant, ago: Duration) -> Verdict {
        if !self.can_force {
            return Verdict::Cannot;
        }
        let lost_at = now.checked_sub(ago).unwrap_or(now);
        if let Some(keyframe) = self.last_keyframe.filter(|k| *k >= lost_at) {
            return Verdict::AlreadySent(now.saturating_duration_since(keyframe));
        }
        if let Some(next) = self.last_keyframe.map(|k| k + self.gop) {
            if next + Self::LATE >= now && next <= now + Self::SOON {
                return Verdict::Coming(next.saturating_duration_since(now));
            }
        }
        if let Some((at, latest)) = &mut self.deferred {
            *latest = (*latest).max(lost_at);
            return Verdict::Deferred(at.saturating_duration_since(now));
        }
        if let Some(earliest) = self.last_forced.map(|f| f + Self::MIN_INTERVAL).filter(|e| *e > now) {
            self.deferred = Some((earliest, lost_at));
            return Verdict::Deferred(earliest - now);
        }
        self.last_forced = Some(now);
        Verdict::Force
    }

    /// True when a deferred keyframe is to be made now (the caller tells the encoder). A keyframe that went out
    /// meanwhile, after the losses it was for, makes it unnecessary.
    pub fn poll(&mut self, now: Instant) -> bool {
        let Some((at, latest)) = self.deferred else { return false };
        if now < at {
            return false;
        }
        self.deferred = None;
        if !self.can_force || self.last_keyframe.is_some_and(|k| k >= latest) {
            return false;
        }
        self.last_forced = Some(now);
        true
    }

    /// When [`poll`](Self::poll) has something to do.
    pub fn next_poll(&self) -> Option<Instant> {
        self.deferred.map(|(at, _)| at)
    }
}

/// The viewer's side: one request open at a time, sent again every [`RESEND`](Self::RESEND) until its answer
/// comes (the path it is for loses packets), given up after [`SENDS`](Self::SENDS) sends. A loss while one is
/// open is asked about once that one is settled: an answer for an earlier loss need not cover a later one.
#[derive(Debug, Default)]
pub struct KeyframeAsker {
    last_seq: u32,
    open: Option<OpenRequest>,
    /// The latest loss not asked about yet: when the data left the host, and why.
    queued: Option<(Instant, Reason)>,
}

#[derive(Debug, Clone, Copy)]
struct OpenRequest {
    request: Request,
    lost_at: Instant,
    first_sent: Instant,
    last_sent: Instant,
    sends: u32,
}

impl KeyframeAsker {
    pub const RESEND: Duration = Duration::from_millis(50);
    pub const SENDS: u32 = 5;

    pub fn new() -> Self {
        Self::default()
    }

    /// A loss: the data left the host `ago` before `now`. Returns the request to send now, or `None` while one
    /// is open (this one is asked about after it).
    pub fn ask(&mut self, now: Instant, ago: Duration, reason: Reason) -> Option<Request> {
        let lost_at = now.checked_sub(ago).unwrap_or(now);
        if self.open.is_some() {
            self.queued = Some((lost_at, reason));
            return None;
        }
        Some(self.send_new(now, lost_at, reason))
    }

    fn send_new(&mut self, now: Instant, lost_at: Instant, reason: Reason) -> Request {
        self.last_seq = self.last_seq.wrapping_add(1).max(1);
        let request = Request { seq: self.last_seq, ago_ms: millis(now.saturating_duration_since(lost_at)), reason };
        self.open = Some(OpenRequest { request, lost_at, first_sent: now, last_sent: now, sends: 1 });
        request
    }

    /// The request to send (again) now, if any: the open one when its answer is late, or the queued loss once
    /// the open one gave up.
    pub fn poll(&mut self, now: Instant) -> Option<Request> {
        let open = self.open.as_mut()?;
        if now.saturating_duration_since(open.last_sent) < Self::RESEND {
            return None;
        }
        if open.sends >= Self::SENDS {
            self.open = None;
            let (lost_at, reason) = self.queued.take()?;
            return Some(self.send_new(now, lost_at, reason));
        }
        open.sends += 1;
        open.last_sent = now;
        // the same request: the host answers a repeat the same way, and the time it carries is still its own
        let mut request = open.request;
        request.ago_ms = millis(now.saturating_duration_since(open.lost_at));
        Some(request)
    }

    /// An answer came: the request it settles and how long that took, or `None` for one that is not open (late,
    /// repeated, or forged). A queued loss is then the next request, from [`poll`](Self::poll)'s next call.
    pub fn answered(&mut self, now: Instant, answer: &Answer) -> Option<(Request, Duration)> {
        let open = self.open.filter(|o| o.request.seq == answer.seq && o.sends > 0)?;
        self.open = None;
        if let Some((lost_at, reason)) = self.queued.take() {
            // opened unsent, so that the next poll sends it at once
            self.last_seq = self.last_seq.wrapping_add(1).max(1);
            let request = Request { seq: self.last_seq, ago_ms: 0, reason };
            let last_sent = now.checked_sub(Self::RESEND).unwrap_or(now);
            self.open = Some(OpenRequest { request, lost_at, first_sent: now, last_sent, sends: 0 });
        }
        Some((open.request, now.saturating_duration_since(open.first_sent)))
    }

    /// A request is open (waiting for its answer).
    pub fn waiting(&self) -> bool {
        self.open.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [9; 32];

    #[test]
    fn requests_and_answers_round_trip_and_nothing_else_opens() {
        let request = Request { seq: 7, ago_ms: 3512, reason: Reason::Missing };
        let d = request.seal(&KEY);
        assert_eq!(d.len(), LENGTH);
        assert_eq!(d[0], b'K');
        assert!(is_keyframe_datagram(&d));
        assert_eq!(Request::open(&KEY, &d), Some(request));
        // another session's key, a flipped bit, the wrong kind, a short one: refused
        assert_eq!(Request::open(&[8; 32], &d), None);
        let mut bad = d.clone();
        bad[6] ^= 1;
        assert_eq!(Request::open(&KEY, &bad), None);
        assert_eq!(Answer::open(&KEY, &d), None);
        assert_eq!(Request::open(&KEY, &d[..25]), None);

        let answer = Answer { seq: 7, outcome: Outcome::AlreadySent, ms: 120 };
        let d = answer.seal(&KEY);
        assert_eq!(d[0], b'k');
        assert_eq!(Answer::open(&KEY, &d), Some(answer));
        assert_eq!(Request::open(&KEY, &d), None);
        // remote control's datagrams start with 1 or 2: never taken for these
        assert!(!is_keyframe_datagram(&[1, 0, 0]) && !is_keyframe_datagram(&[2]) && !is_keyframe_datagram(&[]));
        // an unknown reason or outcome is refused even with a good MAC
        assert_eq!(Request::open(&KEY, &seal(&KEY, REQUEST, 1, 1, 9)), None);
        assert_eq!(Answer::open(&KEY, &seal(&KEY, ANSWER, 1, 1, 0)), None);
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn one_keyframe_per_interval_for_everyone_and_the_rest_deferred() {
        let t0 = Instant::now();
        let mut g = KeyframeGovernor::new(ms(1000));
        assert_eq!(g.request(t0, ms(0)), Verdict::Cannot, "ffmpeg encodes: nothing can be done");
        g.set_can_force(true);
        g.keyframe_sent(t0); // the stream's first keyframe; the next scheduled one is at t0 + 1 s

        // viewer A lost data 40 ms ago: a keyframe now
        assert_eq!(g.request(t0 + ms(200), ms(40)), Verdict::Force);
        // viewer B a moment later, for a loss before that keyframe: the rate limit holds it back
        assert_eq!(g.request(t0 + ms(230), ms(60)), Verdict::Deferred(ms(270)));
        // C as well: the same deferred keyframe
        assert_eq!(g.request(t0 + ms(260), ms(10)), Verdict::Deferred(ms(240)));
        assert_eq!(g.next_poll(), Some(t0 + ms(500)));
        assert!(!g.poll(t0 + ms(499)));
        // the keyframe forced for A went out at 215 ms: B's loss (170 ms) and C's (250 ms) came before / after it
        g.keyframe_sent(t0 + ms(215));
        assert!(g.poll(t0 + ms(500)), "C's loss is after A's keyframe: one more");
        assert_eq!(g.next_poll(), None);
        g.keyframe_sent(t0 + ms(510));

        // a loss before the last keyframe is served by it
        assert_eq!(g.request(t0 + ms(600), ms(150)), Verdict::AlreadySent(ms(90)));
        // within the interval after the last forced one, deferred, and then made unnecessary by a keyframe
        assert_eq!(g.request(t0 + ms(700), ms(20)), Verdict::Deferred(ms(100)));
        g.keyframe_sent(t0 + ms(750));
        assert!(!g.poll(t0 + ms(800)));
    }

    #[test]
    fn a_scheduled_keyframe_about_to_come_is_waited_for() {
        let t0 = Instant::now();
        let mut g = KeyframeGovernor::new(ms(1000));
        g.set_can_force(true);
        g.keyframe_sent(t0);
        assert_eq!(g.request(t0 + ms(900), ms(30)), Verdict::Coming(ms(100)));
        assert_eq!(g.request(t0 + ms(1000), ms(30)), Verdict::Coming(ms(0)));
        // its report is a little late: still coming
        assert_eq!(g.request(t0 + ms(1040), ms(30)), Verdict::Coming(ms(0)));
        // much later than it should have come (the encoder restarted?): do not count on it
        assert_eq!(g.request(t0 + ms(1100), ms(30)), Verdict::Force);
        // 150 ms is the limit
        let mut g = KeyframeGovernor::new(ms(1000));
        g.set_can_force(true);
        g.keyframe_sent(t0);
        assert_eq!(g.request(t0 + ms(849), ms(0)), Verdict::Force);
    }

    #[test]
    fn without_a_keyframe_on_record_requests_are_forced_and_switching_off_forgets_what_waits() {
        let t0 = Instant::now();
        let mut g = KeyframeGovernor::new(ms(1000));
        g.set_can_force(true);
        assert_eq!(g.request(t0, ms(0)), Verdict::Force);
        assert_eq!(g.request(t0 + ms(10), ms(0)), Verdict::Deferred(ms(290)));
        g.set_can_force(false);
        assert_eq!(g.next_poll(), None);
        assert!(!g.poll(t0 + ms(400)));
        assert_eq!(g.request(t0 + ms(500), ms(0)), Verdict::Cannot);
        // a restart of the encoder: its old schedule means nothing
        g.set_can_force(true);
        g.keyframe_sent(t0 + ms(500));
        g.restarted();
        assert_eq!(g.request(t0 + ms(1450), ms(0)), Verdict::Force);
        assert_eq!(Verdict::Deferred(ms(42)).answer(3), Answer { seq: 3, outcome: Outcome::Deferred, ms: 42 });
    }

    #[test]
    fn the_viewer_repeats_its_request_until_answered_and_gives_up_after_five() {
        let t0 = Instant::now();
        let mut a = KeyframeAsker::new();
        let first = a.ask(t0, ms(300), Reason::Missing).unwrap();
        assert_eq!((first.seq, first.ago_ms), (1, 300));
        assert_eq!(a.poll(t0 + ms(49)), None);
        let again = a.poll(t0 + ms(50)).unwrap();
        assert_eq!((again.seq, again.ago_ms), (1, 350), "the same request, its loss older now");
        for i in 2..5 {
            assert_eq!(a.poll(t0 + ms(50 * i)).map(|r| r.seq), Some(1));
        }
        assert_eq!(a.poll(t0 + ms(250)), None, "five sends: given up");
        assert!(!a.waiting());
        // a late answer for it settles nothing
        assert_eq!(a.answered(t0 + ms(260), &Answer { seq: 1, outcome: Outcome::Forced, ms: 0 }), None);

        let second = a.ask(t0 + ms(300), ms(10), Reason::Froze).unwrap();
        assert_eq!(second.seq, 2);
        // a loss while it is open is asked about after it
        assert_eq!(a.ask(t0 + ms(310), ms(20), Reason::Missing), None);
        assert_eq!(a.answered(t0 + ms(320), &Answer { seq: 99, outcome: Outcome::Forced, ms: 0 }), None, "not the open one");
        let (settled, took) = a.answered(t0 + ms(330), &Answer { seq: 2, outcome: Outcome::Forced, ms: 0 }).unwrap();
        assert_eq!((settled.seq, took), (2, ms(30)));
        let third = a.poll(t0 + ms(330)).unwrap();
        assert_eq!((third.seq, third.reason, third.ago_ms), (3, Reason::Missing, 40));
    }
}
