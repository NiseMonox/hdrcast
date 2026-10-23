//! Reads the plain-text headers of the SRT packets a link carries (their payload is encrypted and stays as it is)
//! to tell how the path is doing: what the receiver reports lost, what the sender sends again and what it gives
//! up on, what never arrived in time, and what the receiver's acknowledgements say about the round trip and its
//! rates (docs/design.md §8.11). Nothing here changes a byte of what is forwarded.
//!
//! The SRT header is four big-endian 32-bit words:
//!
//! ```text
//! data:    0|seq(31)    PP(2) O(1) KK(2) R(1) msgno(26)   timestamp   destination socket   payload
//! control: 1|type(15) subtype(16)   type-specific info    timestamp   destination socket   control info
//! ```
//!
//! Read here: data packets (one behind the newest seen is a resend; old senders do not set R), HANDSHAKE (0: a
//! new connection, a new initial sequence number),
//! ACK (2: next expected sequence number, RTT and its variance in µs, free buffer, receive rate in packets/s,
//! link capacity in packets/s, receive rate in bytes/s; a light ACK has the first field only), NAK (3: lost
//! sequence numbers, a range as `first | 0x80000000, last`) and DROPREQ (7: first and last sequence number the
//! sender dropped).

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::util::lock;

const HEADER_BYTES: usize = 16;
const SEQ_MASK: u32 = 0x7FFF_FFFF;
const CONTROL: u32 = 0x8000_0000;
const TYPE_HANDSHAKE: u32 = 0;
const TYPE_ACK: u32 = 2;
const TYPE_NAK: u32 = 3;
const TYPE_DROPREQ: u32 = 7;
/// Sequence numbers remembered for the bookkeeping (about 34 s at 40 Mbps).
const WINDOW: u32 = 1 << 17;
/// How far back [`SrtStats`]'s `recent` figures look.
pub const RECENT: Duration = Duration::from_secs(10);
/// SRT's payload per packet, to turn packets per second into Mbps.
const PAYLOAD_BITS: f64 = 1316.0 * 8.0;

/// Counts since the monitor started. Which side's figures mean what: the host's monitor sees everything its SRT
/// sender sends; the viewer's sees everything that reaches its SRT receiver, so only it can tell what never did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SrtCounts {
    /// Data packets sent for the first time, and their payload bytes.
    pub sent: u64,
    pub sent_bytes: u64,
    /// Data packets sent again (retransmissions).
    pub resent: u64,
    /// Packets the receiver reported missing, each counted once.
    pub lost: u64,
    /// Packets the sender gave up on (too late to send again), each counted once.
    pub dropped: u64,
    /// Packets the receiver went past without having them (lost for good): only the viewer's monitor sees this.
    pub unrecovered: u64,
}

impl SrtCounts {
    /// What was counted after `earlier` (an earlier snapshot of the same counts).
    pub fn since(&self, earlier: &SrtCounts) -> SrtCounts {
        SrtCounts {
            sent: self.sent.saturating_sub(earlier.sent),
            sent_bytes: self.sent_bytes.saturating_sub(earlier.sent_bytes),
            resent: self.resent.saturating_sub(earlier.resent),
            lost: self.lost.saturating_sub(earlier.lost),
            dropped: self.dropped.saturating_sub(earlier.dropped),
            unrecovered: self.unrecovered.saturating_sub(earlier.unrecovered),
        }
    }

    /// Lost packets per hundred sent (0 when nothing was sent).
    pub fn loss_percent(&self) -> f64 {
        percent(self.lost, self.sent)
    }

    /// Packets sent again per hundred sent.
    pub fn resent_percent(&self) -> f64 {
        percent(self.resent, self.sent)
    }
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

/// A snapshot of [`SrtMonitor`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SrtStats {
    pub total: SrtCounts,
    /// The same counts over about the last [`RECENT`] (less right after the start), and how long that was.
    pub recent: SrtCounts,
    pub recent_seconds: f64,
    /// From the receiver's last full acknowledgement; 0 before the first one.
    pub rtt_ms: f64,
    pub rtt_var_ms: f64,
    /// The receive rate in that acknowledgement: measured over the last few packets, so the rate of a burst
    /// rather than the stream's (see [`SrtStats::recent_mbps`] for that).
    pub receive_mbps: f64,
    /// What the receiver estimates the path can carry (SRT's packet-pair probe: a rough figure).
    pub capacity_mbps: f64,
    /// SRT connections that started while the monitor watched (a restarted remux child or player starts one).
    pub connections: u32,
}

impl SrtStats {
    /// The stream's own rate over the recent window (first sends only).
    pub fn recent_mbps(&self) -> f64 {
        if self.recent_seconds <= 0.0 {
            0.0
        } else {
            self.recent.sent_bytes as f64 * 8.0 / 1e6 / self.recent_seconds
        }
    }
}

/// Watches one SRT connection through a forwarder; see the module documentation. Thread-safe: the two directions
/// are fed from different threads.
pub struct SrtMonitor {
    inner: Mutex<Inner>,
}

struct Inner {
    counts: SrtCounts,
    rtt_us: u32,
    rtt_var_us: u32,
    receive_bytes_per_s: u32,
    capacity_packets_per_s: u32,
    connections: u32,
    /// Highest data sequence number seen; `None` before the first (or after a new handshake).
    top: Option<u32>,
    /// The receiver's last acknowledgement: everything before it is received or given up.
    acked: Option<u32>,
    /// Per sequence number in the window: seen as data, reported lost, given up by the sender.
    seen: Bits,
    reported: Bits,
    given_up: Bits,
    /// (when, counts) about once a second, for the recent figures.
    history: VecDeque<(Instant, SrtCounts)>,
}

impl Default for SrtMonitor {
    fn default() -> Self {
        SrtMonitor::new()
    }
}

impl SrtMonitor {
    pub fn new() -> SrtMonitor {
        SrtMonitor {
            inner: Mutex::new(Inner {
                counts: SrtCounts::default(),
                rtt_us: 0,
                rtt_var_us: 0,
                receive_bytes_per_s: 0,
                capacity_packets_per_s: 0,
                connections: 0,
                top: None,
                acked: None,
                seen: Bits::new(),
                reported: Bits::new(),
                given_up: Bits::new(),
                history: VecDeque::new(),
            }),
        }
    }

    /// A datagram the SRT sender sent (data, or its control packets).
    pub fn from_sender(&self, datagram: &[u8]) {
        let Some((word0, _)) = header(datagram) else { return };
        let mut inner = lock(&self.inner);
        if word0 & CONTROL == 0 {
            inner.on_data(word0 & SEQ_MASK, datagram.len().saturating_sub(HEADER_BYTES));
        } else {
            match control_type(word0) {
                TYPE_HANDSHAKE => inner.on_handshake(),
                TYPE_DROPREQ => {
                    if let (Some(first), Some(last)) = (word(datagram, 4), word(datagram, 5)) {
                        inner.on_dropped(first & SEQ_MASK, last & SEQ_MASK);
                    }
                }
                _ => {}
            }
        }
        inner.sample(Instant::now());
    }

    /// A datagram the SRT receiver sent (its acknowledgements and loss reports).
    pub fn from_receiver(&self, datagram: &[u8]) {
        let Some((word0, _)) = header(datagram) else { return };
        if word0 & CONTROL == 0 {
            return;
        }
        let mut inner = lock(&self.inner);
        match control_type(word0) {
            TYPE_HANDSHAKE => inner.on_handshake(),
            TYPE_ACK => inner.on_ack(datagram),
            TYPE_NAK => {
                let mut words = (4..).map_while(|i| word(datagram, i));
                while let Some(w) = words.next() {
                    if w & CONTROL != 0 {
                        let Some(last) = words.next() else { break };
                        inner.on_lost(w & SEQ_MASK, last & SEQ_MASK);
                    } else {
                        inner.on_lost(w, w);
                    }
                }
            }
            _ => {}
        }
    }

    pub fn snapshot(&self) -> SrtStats {
        let inner = lock(&self.inner);
        let now = Instant::now();
        let (recent, recent_seconds) = match inner.history.iter().find(|(at, _)| now.duration_since(*at) <= RECENT) {
            Some((at, counts)) => (inner.counts.since(counts), now.duration_since(*at).as_secs_f64()),
            None => (SrtCounts::default(), 0.0),
        };
        SrtStats {
            total: inner.counts,
            recent,
            recent_seconds,
            rtt_ms: f64::from(inner.rtt_us) / 1000.0,
            rtt_var_ms: f64::from(inner.rtt_var_us) / 1000.0,
            receive_mbps: f64::from(inner.receive_bytes_per_s) * 8.0 / 1e6,
            capacity_mbps: f64::from(inner.capacity_packets_per_s) * PAYLOAD_BITS / 1e6,
            connections: inner.connections,
        }
    }
}

impl Inner {
    fn on_data(&mut self, seq: u32, payload: usize) {
        let fresh = match self.top {
            None => true,
            Some(top) => after(seq, top),
        };
        if fresh {
            self.advance_to(seq);
            self.counts.sent += 1;
            self.counts.sent_bytes += payload as u64;
        } else {
            // behind the front: sent again, whether or not the sender set the flag (old versions do not)
            self.counts.resent += 1;
        }
        self.seen.set(seq);
    }

    /// Moves the window's front to `seq`, forgetting what falls out at the back.
    fn advance_to(&mut self, seq: u32) {
        let steps = match self.top {
            Some(top) => seq.wrapping_sub(top) & SEQ_MASK,
            None => WINDOW,
        };
        if steps >= WINDOW {
            self.seen.clear_all();
            self.reported.clear_all();
            self.given_up.clear_all();
        } else {
            let top = self.top.unwrap_or(seq);
            for i in 1..=steps {
                let s = top.wrapping_add(i) & SEQ_MASK;
                self.seen.clear(s);
                self.reported.clear(s);
                self.given_up.clear(s);
            }
        }
        self.top = Some(seq);
    }

    fn on_handshake(&mut self) {
        if self.top.is_some() || self.acked.is_some() || self.connections == 0 {
            self.connections += 1;
        }
        self.top = None;
        self.acked = None;
    }

    fn on_lost(&mut self, first: u32, last: u32) {
        for seq in range(first, last) {
            if self.reported.set(seq) {
                self.counts.lost += 1;
            }
        }
    }

    fn on_dropped(&mut self, first: u32, last: u32) {
        for seq in range(first, last) {
            if self.given_up.set(seq) {
                self.counts.dropped += 1;
            }
        }
    }

    fn on_ack(&mut self, datagram: &[u8]) {
        let Some(next) = word(datagram, 4).map(|w| w & SEQ_MASK) else { return };
        if let Some(acked) = self.acked {
            if after(next, acked) && self.top.is_some() {
                // everything up to `next` is in, or the receiver went on without it
                let steps = next.wrapping_sub(acked) & SEQ_MASK;
                if steps < WINDOW {
                    for i in 0..steps {
                        let seq = acked.wrapping_add(i) & SEQ_MASK;
                        if !self.seen.get(seq) {
                            self.counts.unrecovered += 1;
                        }
                    }
                }
            }
        }
        if self.acked.is_none_or(|acked| after(next, acked)) {
            self.acked = Some(next);
        }
        if let (Some(rtt), Some(var)) = (word(datagram, 5), word(datagram, 6)) {
            self.rtt_us = rtt;
            self.rtt_var_us = var;
        }
        if let (Some(capacity), Some(bytes)) = (word(datagram, 9), word(datagram, 10)) {
            self.capacity_packets_per_s = capacity;
            self.receive_bytes_per_s = bytes;
        }
    }

    fn sample(&mut self, now: Instant) {
        if self.history.back().is_none_or(|(at, _)| now.duration_since(*at) >= Duration::from_secs(1)) {
            self.history.push_back((now, self.counts));
            while self.history.len() > 2 && self.history.get(1).is_some_and(|(at, _)| now.duration_since(*at) >= RECENT) {
                self.history.pop_front();
            }
        }
    }
}

/// `a` comes after `b` in SRT's wrapping 31-bit sequence space (and is less than half of it ahead).
fn after(a: u32, b: u32) -> bool {
    let d = a.wrapping_sub(b) & SEQ_MASK;
    d != 0 && d < (1 << 30)
}

/// `first..=last` in sequence space, at most a window long.
fn range(first: u32, last: u32) -> impl Iterator<Item = u32> {
    let count = (last.wrapping_sub(first) & SEQ_MASK).min(WINDOW - 1) + 1;
    (0..count).map(move |i| first.wrapping_add(i) & SEQ_MASK)
}

fn header(datagram: &[u8]) -> Option<(u32, u32)> {
    Some((word(datagram, 0)?, word(datagram, 1)?))
}

fn word(datagram: &[u8], index: usize) -> Option<u32> {
    let bytes = datagram.get(index * 4..index * 4 + 4)?;
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}

fn control_type(word0: u32) -> u32 {
    (word0 >> 16) & 0x7FFF
}

/// One bit per sequence number in the window.
struct Bits(Vec<u64>);

impl Bits {
    fn new() -> Bits {
        Bits(vec![0; (WINDOW / 64) as usize])
    }

    fn slot(seq: u32) -> (usize, u64) {
        let i = seq % WINDOW;
        ((i / 64) as usize, 1u64 << (i % 64))
    }

    fn get(&self, seq: u32) -> bool {
        let (word, bit) = Bits::slot(seq);
        self.0[word] & bit != 0
    }

    /// Sets the bit; true when it was not set before.
    fn set(&mut self, seq: u32) -> bool {
        let (word, bit) = Bits::slot(seq);
        let was = self.0[word] & bit != 0;
        self.0[word] |= bit;
        !was
    }

    fn clear(&mut self, seq: u32) {
        let (word, bit) = Bits::slot(seq);
        self.0[word] &= !bit;
    }

    fn clear_all(&mut self) {
        self.0.fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(seq: u32, again: bool) -> Vec<u8> {
        let mut d = Vec::with_capacity(HEADER_BYTES + 1316);
        d.extend_from_slice(&(seq & SEQ_MASK).to_be_bytes());
        // PP = solo, R = sent again, message number 7
        d.extend_from_slice(&(0xC000_0000u32 | if again { 0x0400_0000 } else { 0 } | 7).to_be_bytes());
        d.extend_from_slice(&[0; 8]);
        d.extend_from_slice(&[0xAB; 1316]);
        d
    }

    fn control(kind: u32, info: &[u32]) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&(CONTROL | kind << 16).to_be_bytes());
        d.extend_from_slice(&[0; 12]);
        for w in info {
            d.extend_from_slice(&w.to_be_bytes());
        }
        d
    }

    fn ack(next: u32) -> Vec<u8> {
        // next, RTT 92 ms, var 4 ms, buffer, 3800 packets/s, capacity 9500 packets/s, 5 MB/s
        control(TYPE_ACK, &[next, 92_000, 4_000, 8000, 3800, 9500, 5_000_000])
    }

    #[test]
    fn losses_resends_and_the_receivers_figures_are_counted() {
        let m = SrtMonitor::new();
        m.from_sender(&control(TYPE_HANDSHAKE, &[0; 12]));
        for seq in 1000..1100 {
            if !(1010..1013).contains(&seq) {
                m.from_sender(&data(seq, false));
            }
        }
        // the receiver reports 1010..=1012 missing, twice (a periodic report repeats it)
        m.from_receiver(&control(TYPE_NAK, &[1010 | CONTROL, 1012]));
        m.from_receiver(&control(TYPE_NAK, &[1010 | CONTROL, 1012]));
        m.from_sender(&data(1010, true));
        m.from_sender(&data(1011, true));
        // 1012 was too late: the sender drops it (and may say so more than once)
        m.from_sender(&control(TYPE_DROPREQ, &[1012, 1012]));
        m.from_sender(&control(TYPE_DROPREQ, &[1012, 1012]));
        m.from_receiver(&ack(1000));
        m.from_receiver(&ack(1100));
        let s = m.snapshot();
        assert_eq!(
            s.total,
            SrtCounts { sent: 97, sent_bytes: 97 * 1316, resent: 2, lost: 3, dropped: 1, unrecovered: 1 }
        );
        assert_eq!(s.recent, s.total, "all of it is recent");
        assert!((s.total.loss_percent() - 3.0 / 97.0 * 100.0).abs() < 1e-9);
        assert_eq!((s.rtt_ms, s.rtt_var_ms), (92.0, 4.0));
        assert!((s.capacity_mbps - 9500.0 * 1316.0 * 8.0 / 1e6).abs() < 1e-6);
        assert!((s.receive_mbps - 40.0).abs() < 1e-9);
        assert_eq!(s.connections, 1);
    }

    #[test]
    fn a_single_lost_packet_and_a_light_ack() {
        let m = SrtMonitor::new();
        for seq in [5, 6, 8] {
            m.from_sender(&data(seq, false));
        }
        m.from_receiver(&control(TYPE_NAK, &[7]));
        m.from_receiver(&control(TYPE_ACK, &[5]));
        m.from_receiver(&control(TYPE_ACK, &[9])); // light: no RTT
        let s = m.snapshot();
        assert_eq!((s.total.lost, s.total.unrecovered, s.rtt_ms), (1, 1, 0.0));
    }

    #[test]
    fn sequence_numbers_wrap_and_a_new_connection_starts_afresh() {
        let m = SrtMonitor::new();
        for seq in SEQ_MASK - 2..=SEQ_MASK {
            m.from_sender(&data(seq, false));
        }
        for seq in 0..3 {
            m.from_sender(&data(seq, false));
        }
        m.from_receiver(&control(TYPE_NAK, &[(SEQ_MASK - 1) | CONTROL, 1]));
        assert_eq!(m.snapshot().total.lost, 4, "a range across the wrap");
        assert_eq!(m.snapshot().total.sent, 6);
        // the remux child restarted: a handshake, a new initial sequence number far away
        m.from_receiver(&control(TYPE_HANDSHAKE, &[0; 12]));
        m.from_sender(&data(123_456, false));
        m.from_sender(&data(123_457, false));
        let s = m.snapshot();
        assert_eq!((s.total.sent, s.total.resent, s.connections), (8, 0, 1));
    }

    #[test]
    fn short_or_foreign_datagrams_are_ignored() {
        let m = SrtMonitor::new();
        m.from_sender(&[1, 2, 3]);
        m.from_receiver(&[0xFF; 8]);
        m.from_receiver(&data(4, false)); // a receiver sends no data
        m.from_receiver(&control(TYPE_NAK, &[1 | CONTROL])); // a range without its end
        assert_eq!(m.snapshot(), SrtStats::default());
    }

    #[test]
    fn a_resend_without_the_flag_is_still_a_resend() {
        let m = SrtMonitor::new();
        for seq in [1, 2, 4, 5] {
            m.from_sender(&data(seq, false));
        }
        m.from_sender(&data(3, false)); // an old sender that does not set R
        m.from_sender(&data(2, true));
        let s = m.snapshot();
        assert_eq!((s.total.sent, s.total.resent), (4, 2));
    }
}
