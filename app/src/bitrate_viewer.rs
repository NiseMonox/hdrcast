//! Bitrate requests, viewer side (docs/design.md §8.13). The most this viewer wants a host's stream to carry is kept
//! for each host (`receiver.json`) and goes with every join as "br"; changed while watching, it goes over the side
//! channel at once, sent again until the host answers. Once answered it is sent every few seconds, and the host
//! answers with what its stream runs at then: its setting, or less when a viewer asked for less. Only a host whose
//! live record said "br" is asked.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hdrcast_media::bitrate::{self, Answer, BitrateAsker, Policy, Request};
use serde_json::{json, Value};

use crate::bitrate_host::mbps;

type LogFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Sends a datagram to the host on the side channel; false when it cannot go (no path, or the host does not take
/// bitrate requests).
pub type SendFn = Arc<dyn Fn(&[u8]) -> bool + Send + Sync>;

struct Inner {
    /// The current session and its key; `None` while not connected (or the host takes no requests).
    session: Option<(String, [u8; 32])>,
    asker: BitrateAsker,
    /// What this viewer wants, kbit/s (0: the host's setting). A new session's join asked for it.
    wanted: u32,
}

pub struct BitrateLink {
    inner: Mutex<Inner>,
    wake: Condvar,
    send: SendFn,
    log: LogFn,
    stopped: AtomicBool,
}

impl BitrateLink {
    /// `wanted`: what this viewer asks the host for, kbit/s (0: nothing), as its joins carry it.
    pub fn start(wanted: u32, send: SendFn, log: LogFn) -> Arc<Self> {
        let link = Arc::new(Self {
            inner: Mutex::new(Inner { session: None, asker: BitrateAsker::new(wanted), wanted }),
            wake: Condvar::new(),
            send,
            log,
            stopped: AtomicBool::new(false),
        });
        let me = link.clone();
        thread::Builder::new().name("bitrate-viewer".into()).spawn(move || me.run()).ok();
        link
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.wake.notify_all();
    }

    /// Connected in session `sid` to a host that takes requests; their key. A new session starts from what its
    /// join asked for, and asks at once how things stand.
    pub fn session(&self, sid: &str, key: [u8; 32]) {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.session.as_ref().is_some_and(|(current, _)| current == sid) {
                return;
            }
            inner.session = Some((sid.to_string(), key));
            inner.asker = BitrateAsker::new(inner.wanted);
        }
        self.wake.notify_all();
    }

    /// Not connected, or the host takes no requests: nothing is asked.
    pub fn session_ended(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.session = None;
        inner.asker = BitrateAsker::new(inner.wanted);
    }

    /// This viewer now wants at most `kbps` (0: the host's setting): asked at once while connected, and from the
    /// next join on.
    pub fn want(&self, kbps: u32) {
        let datagram = {
            let mut inner = self.inner.lock().unwrap();
            inner.wanted = kbps;
            let Some((_, key)) = inner.session else { return };
            inner.asker.want(Instant::now(), kbps).map(|request| request.seal(&key))
        };
        if let Some(datagram) = datagram {
            (self.send)(&datagram);
            (self.log)(&if kbps == 0 {
                "[bitrate] asked the host for its own bitrate again".to_string()
            } else {
                format!("[bitrate] asked the host for at most {}", mbps(kbps))
            });
        }
        self.wake.notify_all();
    }

    /// A side-channel datagram from the host of this kind (on a network thread): an answer.
    pub fn on_datagram(&self, payload: &[u8]) {
        if payload.first() != Some(&bitrate::ANSWER) {
            return;
        }
        let news = {
            let mut inner = self.inner.lock().unwrap();
            let Some((_, key)) = inner.session else { return };
            let Some(answer) = Answer::open(&key, payload) else { return };
            inner.asker.answered(answer).then_some((answer, inner.wanted))
        };
        self.wake.notify_all();
        let Some((answer, wanted)) = news else { return };
        (self.log)(&describe(&answer, wanted));
    }

    /// For the page: what this viewer asks for, and what the host said last.
    pub fn status(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        let answer = inner.session.as_ref().and(inner.asker.answer());
        json!({
            "wantedKbps": inner.wanted,
            // the host has not answered the latest request yet
            "waiting": inner.session.is_some() && inner.asker.waiting(),
            "kbps": answer.map(|a| a.kbps),
            "ceilingKbps": answer.map(|a| a.ceiling_kbps),
            "askedKbps": answer.map(|a| a.asked_kbps),
            "taken": answer.map(|a| a.policy == Policy::Taken),
        })
    }

    fn run(&self) {
        while !self.stopped.load(Ordering::SeqCst) {
            let mut inner = self.inner.lock().unwrap();
            let now = Instant::now();
            let due: Option<(Request, [u8; 32])> = match inner.session {
                Some((_, key)) => inner.asker.poll(now).map(|request| (request, key)),
                None => None,
            };
            if let Some((request, key)) = due {
                drop(inner);
                (self.send)(&request.seal(&key));
                continue;
            }
            let wait = match inner.session {
                Some(_) => inner.asker.next_send(now).saturating_duration_since(now).max(Duration::from_millis(10)),
                None => Duration::from_secs(1),
            };
            let _ = self.wake.wait_timeout(inner, wait);
        }
    }
}

/// An answer, for the log.
fn describe(answer: &Answer, wanted: u32) -> String {
    let (rate, ceiling) = (mbps(answer.kbps), mbps(answer.ceiling_kbps));
    if answer.policy == Policy::Off {
        return format!("[bitrate] the host does not let viewers lower the bitrate: its stream runs at its setting ({ceiling})");
    }
    if answer.kbps >= answer.ceiling_kbps {
        format!("[bitrate] the host's stream runs at its setting ({ceiling})")
    } else if wanted > 0 && answer.kbps >= wanted.min(answer.ceiling_kbps) {
        format!("[bitrate] the host's stream runs at {rate}, as asked (its setting is {ceiling})")
    } else {
        format!("[bitrate] the host's stream runs at {rate}: another viewer asked for less (its setting is {ceiling})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hdrcast_media::bitrate::{Requests, Stream, Taken};

    #[test]
    fn a_choice_reaches_the_host_and_its_answer_the_page() {
        let key = [3u8; 32];
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let s = sent.clone();
        let link = BitrateLink::start(0, Arc::new(move |d| {
            s.lock().unwrap().push(d.to_vec());
            true
        }), Arc::new(|_| {}));
        // not connected: remembered for the next join, nothing sent
        link.want(8000);
        assert!(sent.lock().unwrap().is_empty());
        assert_eq!(link.status()["wantedKbps"], 8000);
        assert_eq!(link.status()["kbps"], Value::Null);

        // connected (the join asked for 8 Mbps): the host hears request 0 again at once
        let mut host = Requests::new(true, 20_000, 30_000);
        host.add("s", "Ann", Stream::Watch, 8000);
        link.session("s", key);
        let deadline = Instant::now() + Duration::from_secs(5);
        while sent.lock().unwrap().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let first = Request::open(&key, &sent.lock().unwrap()[0]).unwrap();
        assert_eq!(first, Request { seq: 0, kbps: 8000 });
        assert_eq!(host.take("s", first), Taken::Again);
        link.on_datagram(&host.answer("s").unwrap().seal(&key));
        let status = link.status();
        assert_eq!((status["kbps"].clone(), status["ceilingKbps"].clone(), status["waiting"].clone()), (json!(8000), json!(20_000), json!(false)));

        // a new choice goes at once
        sent.lock().unwrap().clear();
        link.want(3000);
        let request = Request::open(&key, &sent.lock().unwrap()[0]).unwrap();
        assert_eq!(request, Request { seq: 1, kbps: 3000 });
        assert!(link.status()["waiting"].as_bool().unwrap());
        assert_eq!(host.take("s", request), Taken::New { moved: true });
        // forged with another key: nothing
        link.on_datagram(&host.answer("s").unwrap().seal(&[4u8; 32]));
        assert!(link.status()["waiting"].as_bool().unwrap());
        link.on_datagram(&host.answer("s").unwrap().seal(&key));
        assert_eq!(link.status()["kbps"], 3000);
        assert!(!link.status()["waiting"].as_bool().unwrap());
        link.stop();
    }

    #[test]
    fn the_log_says_who_brought_the_stream_down() {
        let answer = |kbps, policy| Answer { seq: 1, asked_kbps: 8000, kbps, ceiling_kbps: 20_000, policy };
        assert!(describe(&answer(8000, Policy::Taken), 8000).contains("runs at 8 Mbps, as asked"));
        assert!(describe(&answer(5000, Policy::Taken), 8000).contains("another viewer asked for less"));
        assert!(describe(&answer(20_000, Policy::Taken), 0).contains("runs at its setting (20 Mbps)"));
        assert!(describe(&answer(20_000, Policy::Off), 8000).contains("does not let viewers lower the bitrate"));
    }
}
