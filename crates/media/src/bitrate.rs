//! A lower bitrate on a viewer's request (docs/design.md §8.13). The host's setting is the most its stream carries.
//! A viewer whose path cannot take that much asks for less over the side channel (§8.9), and the host's encoder
//! runs at the lowest rate any viewer of that stream asked for, never below [`FLOOR_KBPS`]: one encode serves them
//! all, so the host decides for all of them together ([`Requests`]). Asking for more than the setting changes
//! nothing: raising it is up to the host's user, and a viewer can only take back its own request. The host's user
//! may switch requests off; they are still answered then, and the stream runs at the setting.
//!
//! The viewer keeps its latest request open and sends it again until the host answers it, then now and then to
//! hear what the stream runs at ([`BitrateAsker`]). The host answers every request, and tells every viewer of a
//! stream when what that stream runs at changes.
//!
//! The datagrams (the side channel's payload):
//!
//! ```text
//! request, viewer → host (25 bytes)
//!  0  1  kind    'B' (0x42)
//!  1  4  seq     the viewer's request number in this session (big-endian); 0 is what its join asked for ("br")
//!  5  4  kbps    the most the viewer wants the stream to carry, kbit/s; 0 = the host's setting
//!  9 16  mac     HMAC-SHA256(GroupKeys::bitrate_key(sid), bytes 0..9), its first 16 bytes
//!
//! answer, host → viewer (34 bytes)
//!  0  1  kind    'b' (0x62)
//!  1  4  seq     the latest request the host took from this viewer (0: only its join's)
//!  5  4  asked   what that request asked for, kbit/s (0: nothing)
//!  9  4  kbps    what the stream runs at (or is about to, while its encoder starts again), kbit/s
//! 13  4  ceil    the host's setting, kbit/s
//! 17  1  policy  a [`Policy`]
//! 18 16  mac     HMAC-SHA256(GroupKeys::bitrate_key(sid), bytes 0..18), its first 16 bytes
//! ```
//!
//! Remote control's datagrams start with 1 or 2, keyframe requests' with 'K' or 'k', control mode's with 'V', 'A',
//! 'R' or 'r', so none of them mix. Both ends send these only to a peer whose live record / join carried "br".

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use hdrcast_core::group::keys;

pub const REQUEST: u8 = b'B';
pub const ANSWER: u8 = b'b';
pub const REQUEST_LENGTH: usize = 25;
pub const ANSWER_LENGTH: usize = 34;
const MAC_BYTES: usize = 16;

/// The least a request brings a stream down to, kbit/s.
pub const FLOOR_KBPS: u32 = 1000;

/// The datagram belongs to bitrate requests (a request or an answer), whatever else is wrong with it.
pub fn is_bitrate_datagram(payload: &[u8]) -> bool {
    matches!(payload.first(), Some(&REQUEST) | Some(&ANSWER))
}

/// Whether the host takes requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Taken: the stream runs at the lowest one, down to [`FLOOR_KBPS`].
    Taken = 1,
    /// The host's user switched requests off: the stream runs at the setting. Requests are still kept, and count
    /// again once the switch is back on.
    Off = 2,
}

impl Policy {
    fn from_u8(v: u8) -> Option<Policy> {
        match v {
            1 => Some(Policy::Taken),
            2 => Some(Policy::Off),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub seq: u32,
    /// The most the viewer wants the stream to carry, kbit/s; 0 = the host's setting.
    pub kbps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    /// The latest request taken from this viewer (0: its join's).
    pub seq: u32,
    /// What that request asked for, kbit/s (0: nothing).
    pub asked_kbps: u32,
    /// What the stream runs at, kbit/s.
    pub kbps: u32,
    /// The host's setting, kbit/s.
    pub ceiling_kbps: u32,
    pub policy: Policy,
}

fn seal(key: &[u8; 32], mut d: Vec<u8>) -> Vec<u8> {
    let mac = keys::hmac_sha256(key, &d);
    d.extend_from_slice(&mac[..MAC_BYTES]);
    d
}

/// The signed part of a datagram of `kind` and `length` whose MAC holds.
fn open<'a>(key: &[u8; 32], kind: u8, length: usize, d: &'a [u8]) -> Option<&'a [u8]> {
    if d.len() != length || d[0] != kind {
        return None;
    }
    let signed = length - MAC_BYTES;
    let mac = keys::hmac_sha256(key, &d[..signed]);
    keys::fixed_time_eq(&mac[..MAC_BYTES], &d[signed..]).then(|| &d[..signed])
}

fn word(d: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(d[at..at + 4].try_into().expect("four bytes"))
}

impl Request {
    pub fn seal(&self, key: &[u8; 32]) -> Vec<u8> {
        let mut d = Vec::with_capacity(REQUEST_LENGTH);
        d.push(REQUEST);
        d.extend_from_slice(&self.seq.to_be_bytes());
        d.extend_from_slice(&self.kbps.to_be_bytes());
        seal(key, d)
    }

    /// `None` unless it is a request signed with `key` (this session's).
    pub fn open(key: &[u8; 32], datagram: &[u8]) -> Option<Request> {
        let d = open(key, REQUEST, REQUEST_LENGTH, datagram)?;
        Some(Request { seq: word(d, 1), kbps: word(d, 5) })
    }
}

impl Answer {
    pub fn seal(&self, key: &[u8; 32]) -> Vec<u8> {
        let mut d = Vec::with_capacity(ANSWER_LENGTH);
        d.push(ANSWER);
        for w in [self.seq, self.asked_kbps, self.kbps, self.ceiling_kbps] {
            d.extend_from_slice(&w.to_be_bytes());
        }
        d.push(self.policy as u8);
        seal(key, d)
    }

    /// `None` unless it is an answer signed with `key` (this session's).
    pub fn open(key: &[u8; 32], datagram: &[u8]) -> Option<Answer> {
        let d = open(key, ANSWER, ANSWER_LENGTH, datagram)?;
        Some(Answer {
            seq: word(d, 1),
            asked_kbps: word(d, 5),
            kbps: word(d, 9),
            ceiling_kbps: word(d, 13),
            policy: Policy::from_u8(d[17])?,
        })
    }
}

/// Which of the host's streams a viewer gets. Each has its own setting, and runs at its own rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    /// The buffered stream, through SRT.
    Watch = 0,
    /// Control mode's low-latency stream (§11).
    Control = 1,
}

impl Stream {
    pub const BOTH: [Stream; 2] = [Stream::Watch, Stream::Control];
}

#[derive(Debug, Clone)]
struct Asking {
    name: String,
    stream: Stream,
    seq: u32,
    kbps: u32,
    /// Being let in: not among the host's viewers yet, so [`Requests::keep_only`] keeps it.
    joining: bool,
}

/// What a request did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Taken {
    /// Not from a viewer in the list, or older than one taken already (late, or replayed): no answer.
    Ignored,
    /// The one taken last, again (a repeat, or the viewer asking how things stand): answer it.
    Again,
    /// A new request, taken: answer it. `moved`: what its stream runs at changed, so every viewer of the stream is
    /// to hear of it, and the encoder is to be told.
    New { moved: bool },
}

/// The host's side (docs/design.md §8.13): what each viewer asked for, and what each stream runs at because of it.
/// Bookkeeping only: the app sends the answers and gives the encoders their rates.
#[derive(Debug, Clone)]
pub struct Requests {
    policy: Policy,
    /// The settings, kbit/s, by [`Stream`].
    ceilings: [u32; 2],
    viewers: BTreeMap<String, Asking>,
}

impl Requests {
    /// `taken`: the host's user lets viewers lower the bitrate; the settings of the two streams, kbit/s.
    pub fn new(taken: bool, watch_kbps: u32, control_kbps: u32) -> Self {
        Requests { policy: policy_of(taken), ceilings: [watch_kbps, control_kbps], viewers: BTreeMap::new() }
    }

    /// The host's settings changed. Returns the streams whose viewers are to hear of it: what the stream runs at,
    /// the setting or the policy is not what it was.
    pub fn set_policy(&mut self, taken: bool, watch_kbps: u32, control_kbps: u32) -> Vec<Stream> {
        let before = Stream::BOTH.map(|s| self.summary(s));
        self.policy = policy_of(taken);
        self.ceilings = [watch_kbps, control_kbps];
        Stream::BOTH.into_iter().filter(|s| self.summary(*s) != before[*s as usize]).collect()
    }

    fn summary(&self, stream: Stream) -> (u32, u32, Policy) {
        (self.rate(stream), self.ceilings[stream as usize], self.policy)
    }

    /// A viewer is being let in, and its join asked for `kbps` (0: nothing). True when what its stream runs at
    /// moved. It stays in the list until [`admitted`](Self::admitted) or [`remove`](Self::remove).
    pub fn add(&mut self, sid: &str, name: &str, stream: Stream, kbps: u32) -> bool {
        let before = self.rate(stream);
        self.viewers.insert(sid.to_string(), Asking { name: name.to_string(), stream, seq: 0, kbps, joining: true });
        self.rate(stream) != before
    }

    /// The viewer was let in: from now on it is kept while the host serves it ([`keep_only`](Self::keep_only)).
    pub fn admitted(&mut self, sid: &str) {
        if let Some(v) = self.viewers.get_mut(sid) {
            v.joining = false;
        }
    }

    /// The viewer is gone, or was not let in after all. Its stream, when what that runs at moved.
    pub fn remove(&mut self, sid: &str) -> Option<Stream> {
        let stream = self.viewers.get(sid)?.stream;
        let before = self.rate(stream);
        self.viewers.remove(sid);
        (self.rate(stream) != before).then_some(stream)
    }

    /// Forgets the viewers the host no longer serves (`sids`: the ones it does), except those being let in.
    /// Returns the streams whose rate moved.
    pub fn keep_only(&mut self, sids: &[String]) -> Vec<Stream> {
        let before = Stream::BOTH.map(|s| self.rate(s));
        self.viewers.retain(|sid, v| v.joining || sids.contains(sid));
        Stream::BOTH.into_iter().filter(|s| self.rate(*s) != before[*s as usize]).collect()
    }

    /// A request from viewer `sid` (its MAC already checked).
    pub fn take(&mut self, sid: &str, request: Request) -> Taken {
        let Some(v) = self.viewers.get(sid) else { return Taken::Ignored };
        match request.seq.cmp(&v.seq) {
            Ordering::Less => Taken::Ignored,
            Ordering::Equal => Taken::Again,
            Ordering::Greater => {
                let stream = v.stream;
                let before = self.rate(stream);
                let v = self.viewers.get_mut(sid).expect("looked up above");
                (v.seq, v.kbps) = (request.seq, request.kbps);
                Taken::New { moved: self.rate(stream) != before }
            }
        }
    }

    /// The lowest rate the viewers of `stream` asked for (never below [`FLOOR_KBPS`]) while requests are taken;
    /// `None` when nobody asked, or requests are off. The encoder runs at the lower of this and the setting.
    pub fn limit(&self, stream: Stream) -> Option<u32> {
        if self.policy == Policy::Off {
            return None;
        }
        self.viewers.values().filter(|v| v.stream == stream && v.kbps > 0).map(|v| v.kbps.max(FLOOR_KBPS)).min()
    }

    /// What `stream` runs at, kbit/s: the setting, or less when a viewer asked for less.
    pub fn rate(&self, stream: Stream) -> u32 {
        let ceiling = self.ceilings[stream as usize];
        self.limit(stream).map_or(ceiling, |limit| limit.min(ceiling))
    }

    /// Who brought `stream` below its setting: the names of the viewers whose request it runs at (none while it
    /// runs at the setting).
    pub fn limited_by(&self, stream: Stream) -> Vec<String> {
        let rate = self.rate(stream);
        if self.policy == Policy::Off || rate >= self.ceilings[stream as usize] {
            return Vec::new();
        }
        self.viewers
            .values()
            .filter(|v| v.stream == stream && v.kbps > 0 && v.kbps.max(FLOOR_KBPS) == rate)
            .map(|v| v.name.clone())
            .collect()
    }

    /// The answer for viewer `sid` as things stand.
    pub fn answer(&self, sid: &str) -> Option<Answer> {
        let v = self.viewers.get(sid)?;
        Some(Answer {
            seq: v.seq,
            asked_kbps: v.kbps,
            kbps: self.rate(v.stream),
            ceiling_kbps: self.ceilings[v.stream as usize],
            policy: self.policy,
        })
    }

    /// The viewers of `stream` (session ids).
    pub fn viewers_of(&self, stream: Stream) -> Vec<String> {
        self.viewers.iter().filter(|(_, v)| v.stream == stream).map(|(sid, _)| sid.clone()).collect()
    }

    /// What viewer `sid` asked for, kbit/s (0: nothing); `None` for a viewer not in the list.
    pub fn asked(&self, sid: &str) -> Option<u32> {
        self.viewers.get(sid).map(|v| v.kbps)
    }

    /// The name viewer `sid` joined with.
    pub fn name_of(&self, sid: &str) -> Option<&str> {
        self.viewers.get(sid).map(|v| v.name.as_str())
    }

    /// Which stream viewer `sid` gets.
    pub fn stream_of(&self, sid: &str) -> Option<Stream> {
        self.viewers.get(sid).map(|v| v.stream)
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// The setting of `stream`, kbit/s.
    pub fn ceiling(&self, stream: Stream) -> u32 {
        self.ceilings[stream as usize]
    }
}

fn policy_of(taken: bool) -> Policy {
    if taken {
        Policy::Taken
    } else {
        Policy::Off
    }
}

/// The viewer's side: its latest request (the most it wants the stream to carry) stays open, sent again every
/// [`RESEND`](Self::RESEND) until the host answers it, [`SENDS`](Self::SENDS) times in a row before it waits
/// [`REFRESH`](Self::REFRESH) between sends. Once answered it is sent every REFRESH, and the host answers with what
/// the stream runs at then (other viewers' requests and the host's settings move it too).
#[derive(Debug, Clone)]
pub struct BitrateAsker {
    seq: u32,
    kbps: u32,
    /// The host answered request `seq`, holding what it asks for.
    answered: bool,
    /// Sends of request `seq` since it was last answered.
    sends: u32,
    last_sent: Option<Instant>,
    answer: Option<Answer>,
}

impl BitrateAsker {
    pub const RESEND: Duration = Duration::from_millis(250);
    pub const SENDS: u32 = 8;
    pub const REFRESH: Duration = Duration::from_secs(5);

    /// A new session, whose join asked for `kbps` (request 0; 0 = nothing). Its first [`poll`](Self::poll) sends
    /// request 0 again, to hear how things stand.
    pub fn new(kbps: u32) -> Self {
        BitrateAsker { seq: 0, kbps, answered: false, sends: 0, last_sent: None, answer: None }
    }

    /// The viewer now wants at most `kbps` (0: the host's setting): the request to send now, or `None` when that
    /// is what it asks for already.
    pub fn want(&mut self, now: Instant, kbps: u32) -> Option<Request> {
        if kbps == self.kbps {
            return None;
        }
        self.kbps = kbps;
        self.next_request();
        Some(self.send(now))
    }

    fn next_request(&mut self) {
        self.seq = self.seq.saturating_add(1);
        self.answered = false;
        self.sends = 0;
        self.last_sent = None;
    }

    fn send(&mut self, now: Instant) -> Request {
        self.sends = self.sends.saturating_add(1);
        self.last_sent = Some(now);
        Request { seq: self.seq, kbps: self.kbps }
    }

    fn interval(&self) -> Duration {
        if self.answered || self.sends >= Self::SENDS {
            Self::REFRESH
        } else {
            Self::RESEND
        }
    }

    /// The request to send (again) at `now`, if one is due.
    pub fn poll(&mut self, now: Instant) -> Option<Request> {
        if self.last_sent.is_some_and(|at| now.saturating_duration_since(at) < self.interval()) {
            return None;
        }
        Some(self.send(now))
    }

    /// When [`poll`](Self::poll) has something to send (`now` when it has at once).
    pub fn next_send(&self, now: Instant) -> Instant {
        self.last_sent.map_or(now, |at| at + self.interval())
    }

    /// An answer from the host (its MAC already checked). True when it says something new.
    pub fn answered(&mut self, answer: Answer) -> bool {
        if answer.seq > self.seq || self.answer.is_some_and(|last| answer.seq < last.seq) {
            return false; // not for a request of this session's, or overtaken by a later answer
        }
        if answer.seq == self.seq {
            if answer.asked_kbps == self.kbps {
                self.answered = true;
                self.sends = 0;
            } else {
                // The host holds something else under this number: what the join asked for before the viewer
                // changed its mind. The next poll asks again, at once.
                self.next_request();
            }
        }
        let news = self.answer != Some(answer);
        self.answer = Some(answer);
        news
    }

    /// What the viewer asks for, kbit/s (0: the host's setting).
    pub fn wanted(&self) -> u32 {
        self.kbps
    }

    /// The host has not answered the latest request yet.
    pub fn waiting(&self) -> bool {
        !self.answered
    }

    /// The host's latest answer.
    pub fn answer(&self) -> Option<Answer> {
        self.answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [5; 32];

    #[test]
    fn requests_and_answers_round_trip_and_nothing_else_opens() {
        let request = Request { seq: 3, kbps: 8000 };
        let d = request.seal(&KEY);
        assert_eq!(d.len(), REQUEST_LENGTH);
        assert_eq!(d[0], b'B');
        assert!(is_bitrate_datagram(&d));
        assert_eq!(Request::open(&KEY, &d), Some(request));
        // another session's key, a flipped bit, the wrong kind, a short one: refused
        assert_eq!(Request::open(&[6; 32], &d), None);
        let mut bad = d.clone();
        bad[7] ^= 1;
        assert_eq!(Request::open(&KEY, &bad), None);
        assert_eq!(Answer::open(&KEY, &d), None);
        assert_eq!(Request::open(&KEY, &d[..24]), None);

        let answer = Answer { seq: 3, asked_kbps: 8000, kbps: 5000, ceiling_kbps: 20000, policy: Policy::Taken };
        let d = answer.seal(&KEY);
        assert_eq!(d.len(), ANSWER_LENGTH);
        assert_eq!(d[0], b'b');
        assert_eq!(Answer::open(&KEY, &d), Some(answer));
        assert_eq!(Request::open(&KEY, &d), None);
        let off = Answer { policy: Policy::Off, ..answer };
        assert_eq!(Answer::open(&KEY, &off.seal(&KEY)), Some(off));
        // an unknown policy is refused even with a good MAC
        let mut odd = d[..18].to_vec();
        odd[17] = 3;
        assert_eq!(Answer::open(&KEY, &seal(&KEY, odd)), None);

        // the other uses of the side channel start with other bytes
        for first in [1u8, 2, b'K', b'k', b'V', b'A', b'R', b'r'] {
            assert!(!is_bitrate_datagram(&[first, 0]));
        }
        assert!(!is_bitrate_datagram(&[]));
        assert!(!crate::keyframe::is_keyframe_datagram(&d) && !crate::low_latency::is_low_latency_datagram(&d));
    }

    fn sids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_lowest_request_of_a_streams_viewers_sets_its_rate_within_the_setting() {
        let mut r = Requests::new(true, 20_000, 30_000);
        assert_eq!((r.rate(Stream::Watch), r.rate(Stream::Control)), (20_000, 30_000));
        // a join that asks for nothing changes nothing; one that asks for less brings its stream down
        assert!(!r.add("a", "Ann", Stream::Watch, 0));
        assert!(r.add("b", "Bob", Stream::Watch, 10_000));
        assert_eq!(r.rate(Stream::Watch), 10_000);
        assert_eq!(r.limited_by(Stream::Watch), vec!["Bob".to_string()]);
        // the other stream is not touched
        assert_eq!(r.rate(Stream::Control), 30_000);
        // more than the setting changes nothing, less than the floor stops at it
        assert!(!r.add("c", "Cy", Stream::Watch, 50_000));
        assert!(r.add("d", "Di", Stream::Control, 200));
        assert_eq!(r.rate(Stream::Control), FLOOR_KBPS);
        assert_eq!(r.limited_by(Stream::Control), vec!["Di".to_string()]);
        assert_eq!(r.answer("d").unwrap().asked_kbps, 200, "the answer says what was asked");

        // requests in order: a newer one is taken, the same one again is answered, an older one is not
        assert_eq!(r.take("b", Request { seq: 1, kbps: 15_000 }), Taken::New { moved: true });
        assert_eq!(r.rate(Stream::Watch), 15_000);
        assert_eq!(r.take("b", Request { seq: 1, kbps: 2_000 }), Taken::Again, "a number is taken once");
        assert_eq!(r.take("b", Request { seq: 0, kbps: 2_000 }), Taken::Ignored);
        assert_eq!(r.rate(Stream::Watch), 15_000);
        assert_eq!(r.take("x", Request { seq: 5, kbps: 2_000 }), Taken::Ignored, "not a viewer");
        // taking a request back: the setting again
        assert_eq!(r.take("b", Request { seq: 2, kbps: 0 }), Taken::New { moved: true });
        assert_eq!(r.rate(Stream::Watch), 20_000);
        assert!(r.limited_by(Stream::Watch).is_empty());
        assert_eq!(
            r.answer("b"),
            Some(Answer { seq: 2, asked_kbps: 0, kbps: 20_000, ceiling_kbps: 20_000, policy: Policy::Taken })
        );
        // two at the same rate: both named
        r.take("a", Request { seq: 1, kbps: 8_000 });
        r.take("b", Request { seq: 3, kbps: 8_000 });
        assert_eq!(r.limited_by(Stream::Watch), vec!["Ann".to_string(), "Bob".to_string()]);
        let mut watchers = r.viewers_of(Stream::Watch);
        watchers.sort();
        assert_eq!(watchers, sids(&["a", "b", "c"]));
    }

    #[test]
    fn the_hosts_switch_and_setting_win_and_viewers_hear_of_them() {
        let mut r = Requests::new(true, 20_000, 30_000);
        r.add("a", "Ann", Stream::Watch, 10_000);
        // switched off: the setting, but the request is kept
        assert_eq!(r.set_policy(false, 20_000, 30_000), Stream::BOTH.to_vec(), "both streams' answers say Off now");
        assert_eq!((r.rate(Stream::Watch), r.limit(Stream::Watch)), (20_000, None));
        assert_eq!(r.answer("a").unwrap().policy, Policy::Off);
        assert_eq!(r.answer("a").unwrap().asked_kbps, 10_000);
        assert!(r.limited_by(Stream::Watch).is_empty());
        // a new request while off is taken (and counts once the switch is back on) but moves nothing
        assert_eq!(r.take("a", Request { seq: 1, kbps: 5_000 }), Taken::New { moved: false });
        assert_eq!(r.set_policy(true, 20_000, 30_000), Stream::BOTH.to_vec());
        assert_eq!(r.rate(Stream::Watch), 5_000);
        // a lower setting than the request: the setting
        assert_eq!(r.set_policy(true, 4_000, 30_000), vec![Stream::Watch]);
        assert_eq!(r.rate(Stream::Watch), 4_000);
        assert!(r.limited_by(Stream::Watch).is_empty(), "the host's own setting is lower");
        // nothing changed: nobody to tell
        assert!(r.set_policy(true, 4_000, 30_000).is_empty());
    }

    #[test]
    fn viewers_being_let_in_are_kept_until_decided_and_leaving_lifts_their_request() {
        let mut r = Requests::new(true, 20_000, 30_000);
        r.add("a", "Ann", Stream::Watch, 6_000);
        // not among the host's viewers yet: kept
        assert!(r.keep_only(&[]).is_empty());
        assert_eq!(r.rate(Stream::Watch), 6_000);
        r.admitted("a");
        r.add("b", "Bob", Stream::Watch, 9_000);
        // the host serves b only now (a left): a's request goes with it
        assert_eq!(r.keep_only(&sids(&["b"])), vec![Stream::Watch]);
        assert_eq!(r.rate(Stream::Watch), 9_000);
        // b could not be let in after all
        assert_eq!(r.remove("b"), Some(Stream::Watch));
        assert_eq!(r.rate(Stream::Watch), 20_000);
        assert_eq!(r.remove("b"), None);
        assert_eq!(r.asked("b"), None);
    }

    #[test]
    fn the_viewer_sends_until_answered_then_now_and_then() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let answer = |seq, asked, kbps| Answer { seq, asked_kbps: asked, kbps, ceiling_kbps: 20_000, policy: Policy::Taken };

        // the join asked for 8 Mbps: request 0 goes again at once, to hear how things stand
        let mut a = BitrateAsker::new(8_000);
        assert_eq!(a.poll(t0), Some(Request { seq: 0, kbps: 8_000 }));
        assert_eq!(a.poll(at(100)), None);
        assert_eq!(a.next_send(at(100)), at(250));
        assert_eq!(a.poll(at(250)), Some(Request { seq: 0, kbps: 8_000 }));
        assert!(a.waiting());
        assert!(a.answered(answer(0, 8_000, 8_000)));
        assert!(!a.waiting());
        assert!(!a.answered(answer(0, 8_000, 8_000)), "the same answer again is no news");
        // answered: asked again every 5 s
        assert_eq!(a.poll(at(1000)), None);
        assert_eq!(a.poll(at(5250)), Some(Request { seq: 0, kbps: 8_000 }));

        // a new choice goes at once, under a new number, and is sent again until answered
        assert_eq!(a.want(at(6000), 8_000), None, "asked already");
        assert_eq!(a.want(at(6000), 3_000), Some(Request { seq: 1, kbps: 3_000 }));
        assert_eq!(a.poll(at(6250)), Some(Request { seq: 1, kbps: 3_000 }));
        // an answer to the older request still says what the stream runs at, but settles nothing
        assert!(a.answered(answer(0, 8_000, 7_000)));
        assert!(a.waiting());
        assert!(a.answered(answer(1, 3_000, 3_000)));
        assert!(!a.waiting());
        assert_eq!(a.answer().unwrap().kbps, 3_000);
        // late, or not this session's: ignored
        assert!(!a.answered(answer(0, 8_000, 8_000)));
        assert!(!a.answered(answer(7, 3_000, 1_000)));
        assert_eq!(a.answer().unwrap().kbps, 3_000);

        // a host that never answers: 8 sends 250 ms apart, then one every 5 s
        let mut b = BitrateAsker::new(0);
        let mut sent = vec![];
        for ms in (0..10_000).step_by(50) {
            if b.poll(at(ms)).is_some() {
                sent.push(ms);
            }
        }
        assert_eq!(sent, vec![0, 250, 500, 750, 1000, 1250, 1500, 1750, 6750]);
    }

    #[test]
    fn a_host_that_holds_another_rate_under_the_number_is_asked_again() {
        let t0 = Instant::now();
        // the join went out with 8 Mbps; the viewer chose 5 before its first exchange (it thinks request 0 is 5)
        let mut a = BitrateAsker::new(5_000);
        assert_eq!(a.poll(t0), Some(Request { seq: 0, kbps: 5_000 }));
        let held = Answer { seq: 0, asked_kbps: 8_000, kbps: 8_000, ceiling_kbps: 20_000, policy: Policy::Taken };
        assert!(a.answered(held));
        assert!(a.waiting());
        assert_eq!(a.poll(t0 + Duration::from_millis(10)), Some(Request { seq: 1, kbps: 5_000 }), "at once");
        // the host takes it
        let mut r = Requests::new(true, 20_000, 30_000);
        r.add("s", "Ann", Stream::Watch, 8_000);
        assert_eq!(r.take("s", Request { seq: 0, kbps: 5_000 }), Taken::Again);
        assert_eq!(r.take("s", Request { seq: 1, kbps: 5_000 }), Taken::New { moved: true });
        assert!(a.answered(r.answer("s").unwrap()));
        assert!(!a.waiting());
        assert_eq!(a.answer().unwrap().kbps, 5_000);
    }
}
