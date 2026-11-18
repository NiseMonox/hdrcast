//! Bitrate requests, host side (docs/design.md §8.13). A viewer whose path cannot carry this computer's setting
//! asks for less, in its join and later over the side channel; each stream runs at the lowest rate its viewers
//! asked for while this computer's user lets them. The bookkeeping is [`Requests`]; this hub answers the viewers,
//! writes the log, and tells the app when what a stream runs at moved, so that its encoder takes the new rate
//! (`share.rs`: without a break when the capture engine encodes, a restart when ffmpeg does).

use std::sync::{Arc, Mutex};

use hdrcast_media::bitrate::{Policy, Request, Requests, Stream, Taken};
use serde_json::{json, Value};

type LogFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Sends an answer to a viewer's session (only to one whose join said "br").
pub type SendFn = Arc<dyn Fn(&str, &[u8]) -> bool + Send + Sync>;
/// The key a session's requests and answers are signed with.
pub type KeyFn = Arc<dyn Fn(&str) -> [u8; 32] + Send + Sync>;
/// What a stream runs at moved: the app gives its encoder the new rate. Never called with the hub's lock held.
pub type MovedFn = Arc<dyn Fn(Stream) + Send + Sync>;

pub struct BitrateHub {
    requests: Mutex<Requests>,
    send: SendFn,
    key: KeyFn,
    moved: MovedFn,
    log: LogFn,
}

/// "8 Mbps", "1.5 Mbps".
pub fn mbps(kbps: u32) -> String {
    if kbps % 1000 == 0 {
        format!("{} Mbps", kbps / 1000)
    } else {
        format!("{:.1} Mbps", f64::from(kbps) / 1000.0)
    }
}

fn stream_name(stream: Stream) -> &'static str {
    match stream {
        Stream::Watch => "the stream",
        Stream::Control => "control mode's stream",
    }
}

/// What `stream` runs at now and why, for the log.
fn describe(r: &Requests, stream: Stream) -> String {
    let (rate, ceiling) = (r.rate(stream), r.ceiling(stream));
    let name = stream_name(stream);
    if r.policy() == Policy::Off {
        return format!("requests are switched off, {name} runs at the setting ({})", mbps(ceiling));
    }
    let by = r.limited_by(stream);
    if by.is_empty() {
        format!("{name} runs at the setting ({})", mbps(ceiling))
    } else {
        format!("{name} runs at {} (the setting is {}), as {} asked", mbps(rate), mbps(ceiling), by.join(", "))
    }
}

impl BitrateHub {
    /// `taken`: this computer's user lets viewers lower the bitrate; the two streams' settings, kbit/s.
    pub fn new(taken: bool, watch_kbps: u32, control_kbps: u32, send: SendFn, key: KeyFn, moved: MovedFn, log: LogFn) -> Arc<Self> {
        Arc::new(BitrateHub { requests: Mutex::new(Requests::new(taken, watch_kbps, control_kbps)), send, key, moved, log })
    }

    /// The settings changed: whether requests count, and each stream's setting. The viewers whose answer changed
    /// hear of it. The caller applies the settings to the encoders itself: `moved` is not called.
    pub fn set_policy(&self, taken: bool, watch_kbps: u32, control_kbps: u32) {
        let (streams, line) = {
            let mut r = self.requests.lock().unwrap();
            let was = r.policy();
            let streams = r.set_policy(taken, watch_kbps, control_kbps);
            let line = (r.policy() != was).then(|| {
                let lowered: Vec<String> =
                    Stream::BOTH.into_iter().filter(|s| r.limit(*s).is_some_and(|l| l < r.ceiling(*s))).map(|s| describe(&r, s)).collect();
                match (taken, lowered.is_empty()) {
                    (false, _) => "[bitrate] viewers may no longer lower the bitrate: the streams run at the settings".to_string(),
                    (true, true) => "[bitrate] viewers may ask for a lower bitrate again".to_string(),
                    (true, false) => format!("[bitrate] viewers may ask for a lower bitrate again: {}", lowered.join("; ")),
                }
            });
            (streams, line)
        };
        if let Some(line) = line {
            (self.log)(&line);
        }
        for stream in streams {
            self.tell(stream, None);
        }
    }

    /// A viewer is being let in, and its join asked for at most `kbps` (0: nothing). Before its stream starts, so
    /// that it starts at the rate asked for.
    pub fn add(&self, sid: &str, name: &str, stream: Stream, kbps: u32) {
        let (moved, line) = {
            let mut r = self.requests.lock().unwrap();
            let moved = r.add(sid, name, stream, kbps);
            (moved, (kbps > 0).then(|| format!("[bitrate] {name} joins asking for at most {}: {}", mbps(kbps), describe(&r, stream))))
        };
        if let Some(line) = line {
            (self.log)(&line);
        }
        if moved {
            self.after_move(stream, Some(sid));
        }
    }

    /// The viewer was let in: from now on it is kept while the host serves it.
    pub fn admitted(&self, sid: &str) {
        self.requests.lock().unwrap().admitted(sid);
    }

    /// The viewer could not be let in after all: its request no longer counts.
    pub fn remove(&self, sid: &str) {
        let (moved, line) = {
            let mut r = self.requests.lock().unwrap();
            let moved = r.remove(sid);
            (moved, moved.map(|stream| format!("[bitrate] a viewer who asked for less was not let in: {}", describe(&r, stream))))
        };
        if let Some(line) = line {
            (self.log)(&line);
        }
        if let Some(stream) = moved {
            self.after_move(stream, None);
        }
    }

    /// Forgets the viewers the host no longer serves (`sids`: those it does). The request of one who left no longer
    /// counts.
    pub fn keep_only(&self, sids: &[String]) {
        let (moved, lines) = {
            let mut r = self.requests.lock().unwrap();
            let moved = r.keep_only(sids);
            let lines: Vec<String> = moved.iter().map(|s| format!("[bitrate] a viewer who asked for less left: {}", describe(&r, *s))).collect();
            (moved, lines)
        };
        for line in lines {
            (self.log)(&line);
        }
        for stream in moved {
            self.after_move(stream, None);
        }
    }

    /// A datagram of the side channel that [`hdrcast_media::bitrate::is_bitrate_datagram`] took for one of these.
    pub fn on_datagram(&self, sid: &str, payload: &[u8]) {
        let Some(request) = Request::open(&(self.key)(sid), payload) else { return }; // forged, damaged, or an answer
        let (taken, stream, line) = {
            let mut r = self.requests.lock().unwrap();
            let taken = r.take(sid, request);
            let stream = r.stream_of(sid);
            let line = match (taken, stream) {
                (Taken::New { .. }, Some(stream)) => {
                    let name = r.name_of(sid).unwrap_or("a viewer").to_string();
                    Some(if request.kbps == 0 {
                        format!("[bitrate] {name} no longer asks for a lower bitrate: {}", describe(&r, stream))
                    } else {
                        format!("[bitrate] {name} asks for at most {}: {}", mbps(request.kbps), describe(&r, stream))
                    })
                }
                _ => None,
            };
            (taken, stream, line)
        };
        if let Some(line) = line {
            (self.log)(&line);
        }
        match (taken, stream) {
            (Taken::New { moved: true }, Some(stream)) => self.after_move(stream, Some(sid)),
            (Taken::New { moved: false } | Taken::Again, _) => self.answer(sid),
            _ => {}
        }
    }

    /// The lowest rate the viewers of `stream` asked for while requests count (`None`: nobody, or they do not).
    /// Its encoder runs at the lower of this and the setting.
    pub fn limit(&self, stream: Stream) -> Option<u32> {
        self.requests.lock().unwrap().limit(stream)
    }

    /// What viewer `sid` asked for, kbit/s (0: nothing); `None` for one that does not ask (an older version).
    pub fn asked(&self, sid: &str) -> Option<u32> {
        self.requests.lock().unwrap().asked(sid)
    }

    /// For the page: whether requests count, and what each stream runs at and who brought it down.
    pub fn status(&self) -> Value {
        let r = self.requests.lock().unwrap();
        let stream = |s: Stream| json!({ "kbps": r.rate(s), "ceilingKbps": r.ceiling(s), "by": r.limited_by(s) });
        json!({
            "taken": r.policy() == Policy::Taken,
            "watch": stream(Stream::Watch),
            "control": stream(Stream::Control),
        })
    }

    /// What `stream` runs at moved: its viewers hear of it (`asking`, whose request moved it, among them), then the
    /// app hands the new rate to the encoder.
    fn after_move(&self, stream: Stream, asking: Option<&str>) {
        self.tell(stream, asking);
        (self.moved)(stream);
    }

    /// Every viewer of `stream` hears what it runs at now (`also`: one that is to be answered even if it is not
    /// among them any more).
    fn tell(&self, stream: Stream, also: Option<&str>) {
        let mut sids = self.requests.lock().unwrap().viewers_of(stream);
        if let Some(sid) = also.filter(|sid| !sids.iter().any(|s| s == sid)) {
            sids.push(sid.to_string());
        }
        for sid in sids {
            self.answer(&sid);
        }
    }

    fn answer(&self, sid: &str) {
        let answer = self.requests.lock().unwrap().answer(sid);
        if let Some(answer) = answer {
            (self.send)(sid, &answer.seal(&(self.key)(sid)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hdrcast_media::bitrate::{Answer, BitrateAsker};
    use std::time::Instant;

    type Sent = Arc<Mutex<Vec<(String, Answer)>>>;
    type Shared<T> = Arc<Mutex<Vec<T>>>;

    fn key_of(sid: &str) -> [u8; 32] {
        let mut key = [0u8; 32];
        key[..sid.len().min(32)].copy_from_slice(&sid.as_bytes()[..sid.len().min(32)]);
        key
    }

    fn hub() -> (Arc<BitrateHub>, Sent, Shared<Stream>, Shared<String>) {
        let sent: Sent = Arc::new(Mutex::new(Vec::new()));
        let moved = Arc::new(Mutex::new(Vec::new()));
        let lines = Arc::new(Mutex::new(Vec::new()));
        let (s, m, l) = (sent.clone(), moved.clone(), lines.clone());
        let hub = BitrateHub::new(
            true,
            20_000,
            30_000,
            Arc::new(move |sid, d| {
                s.lock().unwrap().push((sid.to_string(), Answer::open(&key_of(sid), d).expect("answers are sealed with the session's key")));
                true
            }),
            Arc::new(key_of),
            Arc::new(move |stream| m.lock().unwrap().push(stream)),
            Arc::new(move |line| l.lock().unwrap().push(line.to_string())),
        );
        (hub, sent, moved, lines)
    }

    #[test]
    fn a_request_moves_the_stream_tells_its_viewers_and_the_encoder() {
        let (hub, sent, moved, lines) = hub();
        hub.add("a", "Ann", Stream::Watch, 0);
        hub.add("b", "Bob", Stream::Watch, 0);
        hub.add("c", "Cy", Stream::Control, 0);
        for sid in ["a", "b", "c"] {
            hub.admitted(sid);
        }
        assert!(moved.lock().unwrap().is_empty() && sent.lock().unwrap().is_empty(), "nothing asked, nothing moves");

        // Bob asks for 8 Mbps: everyone on the buffered stream hears of it, control mode's viewer does not
        let mut bob = BitrateAsker::new(0);
        let request = bob.want(Instant::now(), 8000).unwrap();
        hub.on_datagram("b", &request.seal(&key_of("b")));
        assert_eq!(*moved.lock().unwrap(), vec![Stream::Watch]);
        assert_eq!(hub.limit(Stream::Watch), Some(8000));
        let told: Vec<String> = sent.lock().unwrap().iter().map(|(sid, _)| sid.clone()).collect();
        assert_eq!(told, ["a", "b"]);
        let to_bob = sent.lock().unwrap().iter().find(|(sid, _)| sid == "b").unwrap().1;
        assert!(bob.answered(to_bob));
        assert!(!bob.waiting());
        assert_eq!(to_bob.kbps, 8000);
        assert!(lines.lock().unwrap().last().unwrap().contains("Bob asks for at most 8 Mbps: the stream runs at 8 Mbps (the setting is 20 Mbps), as Bob asked"));

        // the same request again (a repeat, or Bob asking how things stand): answered, nothing moves
        sent.lock().unwrap().clear();
        hub.on_datagram("b", &request.seal(&key_of("b")));
        assert_eq!(sent.lock().unwrap().len(), 1);
        assert_eq!(moved.lock().unwrap().len(), 1);
        // forged with another session's key, or from somebody unknown: nothing at all
        hub.on_datagram("b", &request.seal(&key_of("a")));
        hub.on_datagram("x", &request.seal(&key_of("x")));
        assert_eq!(sent.lock().unwrap().len(), 1);

        // Bob leaves: back at the setting, and Ann hears of it
        sent.lock().unwrap().clear();
        hub.keep_only(&["a".to_string(), "c".to_string()]);
        assert_eq!(*moved.lock().unwrap(), vec![Stream::Watch, Stream::Watch]);
        assert_eq!(hub.limit(Stream::Watch), None);
        assert_eq!(sent.lock().unwrap().iter().map(|(sid, a)| (sid.as_str(), a.kbps)).collect::<Vec<_>>(), [("a", 20_000)]);
        assert!(lines.lock().unwrap().last().unwrap().contains("left: the stream runs at the setting (20 Mbps)"));
    }

    #[test]
    fn a_join_that_asks_counts_before_the_stream_starts_and_is_lifted_if_not_let_in() {
        let (hub, _sent, moved, lines) = hub();
        hub.add("a", "Ann", Stream::Control, 5000);
        assert_eq!(hub.limit(Stream::Control), Some(5000));
        assert_eq!(*moved.lock().unwrap(), vec![Stream::Control]);
        assert!(lines.lock().unwrap()[0].contains("Ann joins asking for at most 5 Mbps: control mode's stream runs at 5 Mbps (the setting is 30 Mbps)"));
        // still being let in: housekeeping keeps it
        hub.keep_only(&[]);
        assert_eq!(hub.asked("a"), Some(5000));
        hub.remove("a");
        assert_eq!(hub.limit(Stream::Control), None);
        assert_eq!(moved.lock().unwrap().len(), 2);
    }

    #[test]
    fn switched_off_requests_are_kept_and_answered_but_do_not_count() {
        let (hub, sent, _moved, lines) = hub();
        hub.add("a", "Ann", Stream::Watch, 4000);
        hub.admitted("a");
        sent.lock().unwrap().clear();
        hub.set_policy(false, 20_000, 30_000);
        assert_eq!(hub.limit(Stream::Watch), None);
        assert_eq!(sent.lock().unwrap().last().unwrap().1.policy, Policy::Off, "Ann hears that it no longer counts");
        assert!(lines.lock().unwrap().last().unwrap().contains("may no longer lower the bitrate"));
        hub.set_policy(true, 20_000, 30_000);
        assert_eq!(hub.limit(Stream::Watch), Some(4000));
        assert!(lines.lock().unwrap().last().unwrap().contains("again: the stream runs at 4 Mbps"));
        assert_eq!(hub.status()["watch"], json!({ "kbps": 4000, "ceilingKbps": 20_000, "by": ["Ann"] }));
        assert_eq!(mbps(1500), "1.5 Mbps");
    }
}
