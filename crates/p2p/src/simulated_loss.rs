//! A test aid for keyframes on request on one PC (`HDRCAST_SIMULATE_LOSS`, docs/design.md §6, §8.12): every so
//! often the next few SRT data packets from the host are lost for good, their first send and every resend, the
//! way a path abroad sometimes loses data SRT cannot get again in time. Only a viewer's own link uses it.
//!
//! Control mode's datagrams (docs/design.md §11: the picture and the sound on the side channel) are lost the same
//! way, a burst of consecutive ones every so often, and with a third number also at random, that many percent of
//! them: what the parity and the keyframe requests have to make up for.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::side_channel;
use crate::udp_channel::PacketFilter;
use crate::util::{lock, LogFn};

const SEQ_MASK: u32 = 0x7FFF_FFFF;
const DEFAULT_BURST: u32 = 20;

pub struct SimulatedLoss {
    every: Duration,
    burst: u32,
    /// Control mode's datagrams lost at random, in percent.
    random_percent: f64,
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
    /// Control mode: when its next burst is due, how many of it are still to lose, and the random numbers.
    side_next_burst: Option<Instant>,
    side_left: u32,
    side_lost: u64,
    side_seen: u64,
    random: u64,
}

impl SimulatedLoss {
    /// `"5"` (every 5 s, 20 packets), `"5:40"` (every 5 s, 40 packets) or `"5:40:2"` (and 2 % of control mode's
    /// datagrams at random); `None` for anything else.
    pub fn parse(spec: &str, log: Option<LogFn>) -> Option<SimulatedLoss> {
        let parts: Vec<&str> = spec.trim().split(':').map(str::trim).collect();
        let every = parts.first()?.parse::<f64>().ok().filter(|s| (0.5..=3600.0).contains(s))?;
        let burst = match parts.get(1) {
            Some(burst) => burst.parse::<u32>().ok()?,
            None => DEFAULT_BURST,
        };
        let random_percent = match parts.get(2) {
            Some(percent) => percent.parse::<f64>().ok().filter(|p| (0.0..=50.0).contains(p))?,
            None => 0.0,
        };
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64) | 1;
        (parts.len() <= 3 && (1..=100_000).contains(&burst)).then(|| SimulatedLoss {
            every: Duration::from_secs_f64(every),
            burst,
            random_percent,
            log,
            state: Mutex::new(State { random: seed, ..State::default() }),
        })
    }

    pub fn describe(&self) -> String {
        let random = if self.random_percent > 0.0 { format!(", and {}% of control mode's datagrams at random", self.random_percent) } else { String::new() };
        format!("{} packets lost for good every {:.1} s{random}", self.burst, self.every.as_secs_f64())
    }

    /// Control mode's datagrams: the picture ('V') and the sound ('A') on the side channel.
    fn allow_side(&self, datagram: &[u8]) -> bool {
        let kind = datagram.get(side_channel::HEADER_LENGTH).copied();
        if !matches!(kind, Some(b'V') | Some(b'A')) {
            return true; // requests, answers, remote control, keyframes: the small ones always get through
        }
        let mut s = lock(&self.state);
        s.side_seen += 1;
        let now = Instant::now();
        if s.side_left == 0 {
            let due = *s.side_next_burst.get_or_insert(now + self.every);
            if now >= due {
                s.side_next_burst = Some(now + self.every);
                s.side_left = self.burst;
            }
        }
        let lose = if s.side_left > 0 {
            s.side_left -= 1;
            if s.side_left == 0 {
                if let Some(log) = &self.log {
                    log(&format!("[test] lost {} datagrams of control mode's stream on the way in", self.burst));
                }
            }
            true
        } else {
            // xorshift64: plenty for picking packets
            s.random ^= s.random << 13;
            s.random ^= s.random >> 7;
            s.random ^= s.random << 17;
            (s.random % 1_000_000) as f64 / 10_000.0 < self.random_percent
        };
        if lose {
            s.side_lost += 1;
        }
        !lose
    }
}

impl PacketFilter for SimulatedLoss {
    fn on_outbound(&self, _: SocketAddr) {}

    fn allow_inbound(&self, _: SocketAddr) -> bool {
        true
    }

    fn allow_datagram(&self, _: SocketAddr, datagram: &[u8]) -> bool {
        if side_channel::has_magic(datagram) {
            return self.allow_side(datagram);
        }
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
        assert_eq!(SimulatedLoss::parse("3:10:2.5", None).map(|l| (l.burst, l.random_percent)), Some((10, 2.5)));
        for bad in ["", "x", "0.1", "5:0", "5:x", "-1", "5:10:x", "5:10:60", "5:10:1:1"] {
            assert!(SimulatedLoss::parse(bad, None).is_none(), "{bad}");
        }
    }

    #[test]
    fn control_mode_loses_bursts_and_a_share_at_random_but_never_its_small_messages() {
        let loss = SimulatedLoss::parse("0.5:5:10", None).unwrap();
        let from: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let side = |kind: u8| side_channel::build(&[3u8; 16], &[kind; 1100]).unwrap();
        for kind in [b'R', b'r', 1, 2, b'K'] {
            for _ in 0..1000 {
                assert!(loss.allow_datagram(from, &side(kind)), "{}", kind as char);
            }
        }
        let lost = (0..20_000).filter(|_| !loss.allow_datagram(from, &side(b'V'))).count();
        assert!((1500..2500).contains(&lost), "about 10 % at random: {lost}");
        // the burst once its time came
        std::thread::sleep(Duration::from_millis(550));
        let run: Vec<bool> = (0..5).map(|_| loss.allow_datagram(from, &side(b'A'))).collect();
        assert_eq!(run, vec![false; 5]);
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
