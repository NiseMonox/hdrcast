//! A test aid for keyframes on request on one PC (`HDRCAST_SIMULATE_LOSS`, docs/design.md §6, §8.12): every so
//! often the next few SRT data packets from the host are lost for good, their first send and every resend, the
//! way a path abroad sometimes loses data SRT cannot get again in time. Only a viewer's own link uses it.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::udp_channel::PacketFilter;
use crate::util::{lock, LogFn};

const SEQ_MASK: u32 = 0x7FFF_FFFF;
const DEFAULT_BURST: u32 = 20;

pub struct SimulatedLoss {
    every: Duration,
    burst: u32,
    log: Option<LogFn>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    next_burst: Option<Instant>,
    /// Packets of the current burst still to lose, and its first one.
    left: u32,
    first: u32,
    top: Option<u32>,
    gone: HashSet<u32>,
}

impl SimulatedLoss {
    /// `"5"` (every 5 s, 20 packets) or `"5:40"` (every 5 s, 40 packets); `None` for anything else.
    pub fn parse(spec: &str, log: Option<LogFn>) -> Option<SimulatedLoss> {
        let (every, burst) = match spec.trim().split_once(':') {
            Some((every, burst)) => (every.trim(), burst.trim().parse::<u32>().ok()?),
            None => (spec.trim(), DEFAULT_BURST),
        };
        let every = every.parse::<f64>().ok().filter(|s| (0.5..=3600.0).contains(s))?;
        (1..=100_000).contains(&burst).then(|| SimulatedLoss {
            every: Duration::from_secs_f64(every),
            burst,
            log,
            state: Mutex::new(State::default()),
        })
    }

    pub fn describe(&self) -> String {
        format!("{} packets lost for good every {:.1} s", self.burst, self.every.as_secs_f64())
    }
}

impl PacketFilter for SimulatedLoss {
    fn on_outbound(&self, _: SocketAddr) {}

    fn allow_inbound(&self, _: SocketAddr) -> bool {
        true
    }

    fn allow_datagram(&self, _: SocketAddr, datagram: &[u8]) -> bool {
        // SRT data with a full TS payload; the link's own packets (control, side channel, echo) are small
        if datagram.len() < 1000 || datagram[0] & 0x80 != 0 {
            return true;
        }
        let seq = u32::from_be_bytes([datagram[0], datagram[1], datagram[2], datagram[3]]) & SEQ_MASK;
        let mut s = lock(&self.state);
        if s.gone.contains(&seq) {
            return false; // a resend of one that was lost: lost as well
        }
        let fresh = s.top.is_none_or(|top| {
            let ahead = seq.wrapping_sub(top) & SEQ_MASK;
            ahead != 0 && ahead < 1 << 30
        });
        if !fresh {
            return true;
        }
        s.top = Some(seq);
        let now = Instant::now();
        if s.left == 0 {
            let due = *s.next_burst.get_or_insert(now + self.every);
            if now < due {
                return true;
            }
            s.next_burst = Some(now + self.every);
            // the resends of the last burst are long over: SRT gives up within its latency
            s.gone.clear();
            s.left = self.burst;
            s.first = seq;
        }
        s.gone.insert(seq);
        s.left -= 1;
        if s.left == 0 {
            if let Some(log) = &self.log {
                log(&format!("[test] lost {} packets of the stream for good on the way in (seq {}..{})", self.burst, s.first, seq));
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(seq: u32) -> Vec<u8> {
        let mut d = seq.to_be_bytes().to_vec();
        d.resize(1332, 0);
        d
    }

    #[test]
    fn specs_parse_or_are_refused() {
        assert_eq!(SimulatedLoss::parse("5", None).map(|l| (l.every, l.burst)), Some((Duration::from_secs(5), 20)));
        assert_eq!(SimulatedLoss::parse(" 2.5 : 40 ", None).map(|l| (l.every, l.burst)), Some((Duration::from_millis(2500), 40)));
        for bad in ["", "x", "0.1", "5:0", "5:x", "-1"] {
            assert!(SimulatedLoss::parse(bad, None).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_burst_and_all_its_resends_are_lost_and_nothing_else() {
        let loss = SimulatedLoss::parse("0.5:3", None).unwrap();
        let from: SocketAddr = "127.0.0.1:1".parse().unwrap();
        // control packets and short ones always pass
        assert!(loss.allow_datagram(from, &[0x80; 1332]));
        assert!(loss.allow_datagram(from, &data(1)[..58]));
        let mut seq = 100;
        while loss.allow_datagram(from, &data(seq)) {
            seq += 1;
            std::thread::sleep(Duration::from_millis(1));
        }
        let first = seq;
        assert!(!loss.allow_datagram(from, &data(first + 1)));
        assert!(!loss.allow_datagram(from, &data(first + 2)));
        assert!(loss.allow_datagram(from, &data(first + 3)), "three lost, then the stream goes on");
        // resends of the lost ones never arrive; others do
        for again in [first, first + 1, first + 2] {
            assert!(!loss.allow_datagram(from, &data(again)));
        }
        assert!(loss.allow_datagram(from, &data(first - 1)));
    }
}
