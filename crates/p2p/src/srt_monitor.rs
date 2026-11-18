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
//!
//! Where the packets come in from the network ([`SrtMonitor::receiving`], the viewer's end), the monitor also
//! measures how late each one arrived against its own timestamp ([`Lateness`], docs/design.md §5.1): the delay
//! SRT would have needed to hand it over in time. The connection test picks the viewer's delay from that.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::udp_channel::mono_clock;
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
/// A gap is "lasting" (see [`SrtMonitor::take_lasting_loss`]) after twice the time SRT takes to ask for a packet
/// again and get it (RTT + 4 × its variance, as SRT's periodic loss report), and never before this.
const LASTING_AT_LEAST: Duration = Duration::from_millis(60);
/// How often the gaps are looked at, at most.
const LASTING_CHECK_EVERY: Duration = Duration::from_millis(10);
/// Gaps remembered (a long outage is one gap).
const MAX_HOLES: usize = 4096;
/// Lateness is counted in 1 ms bins up to this many (the last bin takes everything later).
const LATE_BINS: usize = 10_000;
/// A connection's first packet stands in for its quickest one until that is known: packets up to this many ms
/// quicker than the first still land in their own bin.
const RAW_BEFORE: usize = 2_000;
const RAW_BINS: usize = RAW_BEFORE + LATE_BINS;

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

/// How late the data packets of a stream arrived, from [`SrtMonitor::lateness`] (docs/design.md §5.1).
///
/// A packet's lateness is its arrival time minus its SRT timestamp (the moment the sender took it in; a resend
/// carries the original's), less the same difference for the quickest packet of its connection. SRT hands a packet
/// over at its timestamp plus the latency, counted from about the quickest arrival: a packet later than the
/// latency is dropped at the receiver. So this is the latency each packet needed, with everything that held it up
/// on the way: the sender's pacing of a big keyframe, queues and jitter on the path, and for a packet lost on the
/// way the rounds of asking for it again. Each sequence number counts once, by its first copy to arrive; packets
/// that never arrived are in [`SrtCounts::unrecovered`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Lateness {
    /// Packets measured.
    pub packets: u64,
    /// The median, 99th, 99.9th and 99.99th percentile and the latest one, in ms (1 ms steps).
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub p999_ms: f64,
    pub p9999_ms: f64,
    pub max_ms: f64,
    /// Packets that came on their first send: the 99.9th percentile and the latest (the sender's pacing and the
    /// path's jitter, without resends).
    pub first_try_p999_ms: f64,
    pub first_try_max_ms: f64,
    /// Packets that came only on a resend (or out of order): how many, the median and the latest.
    pub recovered: u64,
    pub recovered_p50_ms: f64,
    pub recovered_max_ms: f64,
    /// The longest time no data came from the host (an outage), ms.
    pub longest_gap_ms: f64,
    /// Runs of first sends found missing (a packet came after a gap), and the longest run, in packets.
    pub loss_runs: u64,
    pub longest_run: u32,
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
    /// Gaps in the data, oldest first: what the receiver is still waiting for.
    holes: VecDeque<Hole>,
    last_check: Option<Instant>,
    /// A gap that turned lasting and was not taken yet: how long it had been open.
    lasting: Option<Duration>,
    /// A connection started and its first data has not been seen yet.
    awaiting_data: bool,
    /// How late packets arrived ([`SrtMonitor::receiving`] only).
    late: Option<Box<LateBook>>,
}

/// The bookkeeping behind [`Lateness`].
struct LateBook {
    /// The connection that runs now: 1 ms bins of (arrival − timestamp) against its first packet's value (bin
    /// [`RAW_BEFORE`]) until the quickest one is known; packets that came on their first send, and the others.
    raw_first_try: Vec<u32>,
    raw_recovered: Vec<u32>,
    /// (arrival − timestamp) of the connection's first packet, µs.
    first: Option<i64>,
    /// The newest timestamp, unwrapped (SRT's are 32-bit µs: they wrap after 71.6 minutes).
    newest_ts: Option<i64>,
    /// Connections before this one, each against its own quickest packet.
    past_first_try: Vec<u32>,
    past_recovered: Vec<u32>,
    last_arrival_us: Option<i64>,
    longest_gap_us: i64,
    loss_runs: u64,
    longest_run: u32,
}

impl LateBook {
    fn new() -> LateBook {
        LateBook {
            raw_first_try: vec![0; RAW_BINS],
            raw_recovered: vec![0; RAW_BINS],
            first: None,
            newest_ts: None,
            past_first_try: vec![0; LATE_BINS],
            past_recovered: vec![0; LATE_BINS],
            last_arrival_us: None,
            longest_gap_us: 0,
            loss_runs: 0,
            longest_run: 0,
        }
    }

    /// The timestamp `ts` on the connection's unwrapped scale; a resend's (older) does not move the newest.
    fn unwrap(&mut self, ts: u32) -> i64 {
        let Some(newest) = self.newest_ts else {
            self.newest_ts = Some(i64::from(ts));
            return i64::from(ts);
        };
        let value = newest + i64::from(ts.wrapping_sub(newest as u32) as i32);
        if value > newest {
            self.newest_ts = Some(value);
        }
        value
    }

    /// A data packet arrived at `at_us`. `first_copy`: the first one of its sequence number; `first_try`: and it is
    /// the first send (the newest sequence number so far), not a resend.
    fn arrived(&mut self, ts: u32, at_us: i64, first_copy: bool, first_try: bool) {
        if let Some(last) = self.last_arrival_us {
            self.longest_gap_us = self.longest_gap_us.max(at_us - last);
        }
        self.last_arrival_us = Some(at_us.max(self.last_arrival_us.unwrap_or(at_us)));
        let ts = self.unwrap(ts);
        if !first_copy {
            return;
        }
        let d = at_us - ts;
        let first = *self.first.get_or_insert(d);
        let bin = ((d - first).div_euclid(1000) + RAW_BEFORE as i64).clamp(0, RAW_BINS as i64 - 1) as usize;
        if first_try {
            self.raw_first_try[bin] += 1;
        } else {
            self.raw_recovered[bin] += 1;
        }
    }

    fn missing_run(&mut self, packets: u32) {
        self.loss_runs += 1;
        self.longest_run = self.longest_run.max(packets);
    }

    /// The bin of the connection's quickest packet (one that came on its first try, unless none did).
    fn base(&self) -> Option<usize> {
        let quickest = |bins: &[u32]| bins.iter().position(|&n| n > 0);
        match (quickest(&self.raw_first_try), quickest(&self.raw_recovered)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// `past` plus the running connection's `raw`, counted from `base`.
    fn add_raw(past: &mut [u32], raw: &[u32], base: usize) {
        for (i, &n) in raw.iter().enumerate().skip(base).filter(|(_, &n)| n > 0) {
            past[(i - base).min(LATE_BINS - 1)] += n;
        }
    }

    /// A new connection: the one before goes into `past`, against its own quickest packet.
    fn new_connection(&mut self) {
        if let Some(base) = self.base() {
            Self::add_raw(&mut self.past_first_try, &self.raw_first_try, base);
            Self::add_raw(&mut self.past_recovered, &self.raw_recovered, base);
        }
        self.raw_first_try.fill(0);
        self.raw_recovered.fill(0);
        self.first = None;
        self.newest_ts = None;
        self.last_arrival_us = None;
    }

    fn summary(&self) -> Lateness {
        let (mut first_try, mut recovered) = (self.past_first_try.clone(), self.past_recovered.clone());
        if let Some(base) = self.base() {
            Self::add_raw(&mut first_try, &self.raw_first_try, base);
            Self::add_raw(&mut recovered, &self.raw_recovered, base);
        }
        let all: Vec<u32> = first_try.iter().zip(&recovered).map(|(a, b)| a + b).collect();
        let first = Bins::of(&first_try);
        let again = Bins::of(&recovered);
        let both = Bins::of(&all);
        Lateness {
            packets: both.count,
            p50_ms: both.quantile(0.5),
            p99_ms: both.quantile(0.99),
            p999_ms: both.quantile(0.999),
            p9999_ms: both.quantile(0.9999),
            max_ms: both.max(),
            first_try_p999_ms: first.quantile(0.999),
            first_try_max_ms: first.max(),
            recovered: again.count,
            recovered_p50_ms: again.quantile(0.5),
            recovered_max_ms: again.max(),
            longest_gap_ms: self.longest_gap_us as f64 / 1000.0,
            loss_runs: self.loss_runs,
            longest_run: self.longest_run,
        }
    }
}

/// A histogram of lateness in 1 ms bins, for its percentiles.
struct Bins<'a> {
    bins: &'a [u32],
    count: u64,
}

impl<'a> Bins<'a> {
    fn of(bins: &'a [u32]) -> Bins<'a> {
        Bins { bins, count: bins.iter().map(|&n| u64::from(n)).sum() }
    }

    /// The bin (ms) where the fraction `q` of the packets is reached; 0 for none.
    fn quantile(&self, q: f64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        let wanted = ((self.count as f64 * q).ceil() as u64).clamp(1, self.count);
        let mut seen = 0u64;
        for (i, &n) in self.bins.iter().enumerate() {
            seen += u64::from(n);
            if seen >= wanted {
                return i as f64;
            }
        }
        (LATE_BINS - 1) as f64
    }

    fn max(&self) -> f64 {
        self.bins.iter().rposition(|&n| n > 0).map_or(0.0, |i| i as f64)
    }
}

/// Sequence numbers `first..=last` were missing when the packet after them came, at `since`.
#[derive(Debug, Clone, Copy)]
struct Hole {
    first: u32,
    last: u32,
    since: Instant,
    reported: bool,
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
                holes: VecDeque::new(),
                last_check: None,
                lasting: None,
                awaiting_data: false,
                late: None,
            }),
        }
    }

    /// A monitor where the data comes in from the network (the viewer's end): it also measures how late each
    /// packet arrived ([`lateness`](Self::lateness)).
    pub fn receiving() -> SrtMonitor {
        let monitor = SrtMonitor::new();
        lock(&monitor.inner).late = Some(Box::new(LateBook::new()));
        monitor
    }

    /// A datagram the SRT sender sent (data, or its control packets). True for the first data packet of a
    /// connection: the receiver's stream starts here.
    pub fn from_sender(&self, datagram: &[u8]) -> bool {
        self.from_sender_at(datagram, mono_clock::now_us())
    }

    /// [`from_sender`](Self::from_sender) for a datagram that arrived at `at_us` ([`mono_clock`] time).
    pub fn from_sender_at(&self, datagram: &[u8], at_us: i64) -> bool {
        let Some((word0, _)) = header(datagram) else { return false };
        let mut inner = lock(&self.inner);
        let mut first_data = false;
        if word0 & CONTROL == 0 {
            first_data = std::mem::take(&mut inner.awaiting_data) || inner.top.is_none();
            let ts = word(datagram, 2).unwrap_or(0);
            inner.on_data(word0 & SEQ_MASK, datagram.len().saturating_sub(HEADER_BYTES), ts, at_us);
            inner.check_lasting(Instant::now());
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
        first_data
    }

    /// Data the receiver has been waiting for has now been missing for longer than SRT needs to ask for it
    /// twice and get it (docs/design.md §8.12): `Some(how long)`, once per gap. Only the receiving end sees
    /// this; whether SRT still gets the data there in time is open, but the picture that needs it is in danger.
    pub fn take_lasting_loss(&self) -> Option<Duration> {
        lock(&self.inner).lasting.take()
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

    /// How late the packets arrived, over every connection so far; `None` for a monitor that is not
    /// [`receiving`](Self::receiving).
    pub fn lateness(&self) -> Option<Lateness> {
        lock(&self.inner).late.as_ref().map(|book| book.summary())
    }

    /// Measures lateness afresh from here on (the counts go on): the connection test starts once the connection
    /// has settled, since SRT assumes a round trip of 100 ms until it has measured one and asks for lost packets
    /// slowly until then.
    pub fn restart_lateness(&self) {
        if let Some(book) = lock(&self.inner).late.as_mut() {
            **book = LateBook::new();
        }
    }
}

impl Inner {
    fn on_data(&mut self, seq: u32, payload: usize, ts: u32, at_us: i64) {
        let fresh = match self.top {
            None => true,
            Some(top) => after(seq, top),
        };
        let first_copy = fresh || !self.seen.get(seq);
        if fresh {
            if let Some(top) = self.top {
                let next = top.wrapping_add(1) & SEQ_MASK;
                let missing = seq.wrapping_sub(next) & SEQ_MASK;
                if next != seq && missing < WINDOW {
                    if self.holes.len() >= MAX_HOLES {
                        self.holes.pop_front();
                    }
                    let last = seq.wrapping_sub(1) & SEQ_MASK;
                    self.holes.push_back(Hole { first: next, last, since: Instant::now(), reported: false });
                    if let Some(book) = self.late.as_mut() {
                        book.missing_run(missing);
                    }
                }
            }
            self.advance_to(seq);
            self.counts.sent += 1;
            self.counts.sent_bytes += payload as u64;
        } else {
            // behind the front: sent again, whether or not the sender set the flag (old versions do not)
            self.counts.resent += 1;
        }
        if let Some(book) = self.late.as_mut() {
            book.arrived(ts, at_us, first_copy, fresh);
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
        self.holes.clear();
        self.lasting = None;
        self.awaiting_data = true;
        if let Some(book) = self.late.as_mut() {
            // a new connection, a new start for the timestamps
            book.new_connection();
        }
    }

    /// Drops the gaps that filled up (or that the receiver went past), and notes the first one open for too long.
    fn check_lasting(&mut self, now: Instant) {
        if self.last_check.is_some_and(|at| now.saturating_duration_since(at) < LASTING_CHECK_EVERY) {
            return;
        }
        self.last_check = Some(now);
        let wait = 2 * (Duration::from_micros(u64::from(self.rtt_us)) + 4 * Duration::from_micros(u64::from(self.rtt_var_us)));
        let wait = wait.max(LASTING_AT_LEAST);
        let mut i = 0;
        while i < self.holes.len() {
            let hole = &mut self.holes[i];
            // what arrived since closes the gap from its front
            while self.seen.get(hole.first) && hole.first != hole.last {
                hole.first = hole.first.wrapping_add(1) & SEQ_MASK;
            }
            let passed = self.acked.is_some_and(|acked| after(acked, hole.last));
            if passed || (hole.first == hole.last && self.seen.get(hole.first)) {
                self.holes.remove(i);
                continue;
            }
            let age = now.saturating_duration_since(hole.since);
            if age < wait {
                break; // the rest are younger
            }
            if !hole.reported {
                hole.reported = true;
                self.lasting.get_or_insert(age);
            }
            i += 1;
        }
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

    /// The sender sends these data packets for the first time.
    fn stream(m: &SrtMonitor, seqs: impl IntoIterator<Item = u32>) {
        for seq in seqs {
            m.from_sender(&data(seq, false));
        }
    }

    #[test]
    fn a_gap_still_open_after_two_resend_rounds_is_reported_once() {
        let m = SrtMonitor::new();
        assert!(!m.from_sender(&control(TYPE_HANDSHAKE, &[0; 12])));
        assert!(m.from_sender(&data(1, false)), "the first data of a connection");
        assert!(!m.from_sender(&data(2, false)));
        // the receiver measures a short round trip: the floor of 60 ms applies
        m.from_receiver(&control(TYPE_ACK, &[3, 1_000, 500, 8000, 0, 0, 0]));
        stream(&m, (3..10).chain(13..20)); // 10..=12 missing
        assert_eq!(m.take_lasting_loss(), None, "young");
        std::thread::sleep(Duration::from_millis(70));
        stream(&m, [20]);
        let missing = m.take_lasting_loss().expect("still open after 60 ms");
        assert!(missing >= Duration::from_millis(60), "{missing:?}");
        std::thread::sleep(Duration::from_millis(15));
        stream(&m, [21]);
        assert_eq!(m.take_lasting_loss(), None, "once per gap");

        // a gap the resend fills in time is never reported
        stream(&m, (22..30).chain(31..40));
        m.from_sender(&data(30, true));
        std::thread::sleep(Duration::from_millis(70));
        stream(&m, [40]);
        assert_eq!(m.take_lasting_loss(), None);
        // nor one the receiver has gone past (it gave up: the guard sees that one)
        stream(&m, (41..45).chain(46..50));
        m.from_receiver(&control(TYPE_ACK, &[50, 1_000, 500, 8000, 0, 0, 0]));
        std::thread::sleep(Duration::from_millis(70));
        stream(&m, [50]);
        assert_eq!(m.take_lasting_loss(), None);
        // a new connection forgets the old gaps and starts again
        stream(&m, (51..55).chain(56..60));
        m.from_receiver(&control(TYPE_HANDSHAKE, &[0; 12]));
        assert!(m.from_sender(&data(7000, false)), "the first data of the new connection");
        std::thread::sleep(Duration::from_millis(70));
        stream(&m, [7001]);
        assert_eq!(m.take_lasting_loss(), None);
    }

    #[test]
    fn the_wait_grows_with_the_round_trip() {
        let m = SrtMonitor::new();
        // RTT 92 ms, variance 4 ms: 2 x (92 + 16) = 216 ms
        m.from_receiver(&ack(1));
        stream(&m, (1..5).chain(6..10));
        std::thread::sleep(Duration::from_millis(100));
        stream(&m, [10]);
        assert_eq!(m.take_lasting_loss(), None, "within twice a resend round");
        std::thread::sleep(Duration::from_millis(130));
        stream(&m, [11]);
        assert!(m.take_lasting_loss().is_some_and(|d| d >= Duration::from_millis(216)));
    }

    /// A data packet with SRT timestamp `ts` (µs).
    fn data_at(seq: u32, ts: u32, again: bool) -> Vec<u8> {
        let mut d = data(seq, again);
        d[8..12].copy_from_slice(&ts.to_be_bytes());
        d
    }

    #[test]
    fn only_a_receiving_monitor_measures_lateness() {
        assert_eq!(SrtMonitor::new().lateness(), None);
        let m = SrtMonitor::receiving();
        assert_eq!(m.lateness(), Some(Lateness::default()));
    }

    #[test]
    fn a_resend_is_as_late_as_its_original_timestamp_says() {
        let m = SrtMonitor::receiving();
        // sent every millisecond, 5 ms on the way; 50 is lost and comes again 120 ms after it was first sent
        for seq in 1..=100u32 {
            if seq != 50 {
                m.from_sender_at(&data_at(seq, seq * 1000, false), i64::from(seq * 1000) + 5_000);
            }
        }
        m.from_sender_at(&data_at(50, 50_000, true), 50_000 + 5_000 + 120_000);
        // and a copy of one that had arrived already: not measured twice
        m.from_sender_at(&data_at(60, 60_000, true), 60_000 + 5_000 + 300_000);
        let l = m.lateness().unwrap();
        assert_eq!(l.packets, 100);
        assert_eq!((l.p50_ms, l.p99_ms, l.max_ms), (0.0, 0.0, 120.0));
        assert_eq!((l.loss_runs, l.longest_run), (1, 1));
        assert_eq!(l.p9999_ms, 120.0, "the latest of 100 packets is the 99.99th percentile");
        assert_eq!((l.first_try_max_ms, l.recovered, l.recovered_max_ms), (0.0, 1, 120.0), "the resend is told apart");
    }

    #[test]
    fn the_quickest_packet_is_the_reference_even_when_it_came_later() {
        let m = SrtMonitor::receiving();
        // the first packet waited 30 ms (a keyframe in the sender's queue), the rest went straight through
        m.from_sender_at(&data_at(1, 0, false), 30_000);
        for seq in 2..=10u32 {
            let ts = 40_000 + seq * 1000;
            m.from_sender_at(&data_at(seq, ts, false), i64::from(ts));
        }
        let l = m.lateness().unwrap();
        assert_eq!((l.packets, l.p50_ms, l.max_ms), (10, 0.0, 30.0));
        assert!((l.longest_gap_ms - 12.0).abs() < 1e-9, "30 ms -> 42 ms: {}", l.longest_gap_ms);
    }

    #[test]
    fn timestamps_wrap_and_a_new_connection_gets_its_own_reference() {
        let m = SrtMonitor::receiving();
        // 32-bit microseconds wrap after 71.6 minutes
        let start = u32::MAX - 5_000;
        for i in 0..10u32 {
            let ts = start.wrapping_add(i * 1000);
            m.from_sender_at(&data_at(i + 1, ts, false), 1_000_000 + i64::from(i) * 1000);
        }
        assert_eq!(m.lateness().unwrap().max_ms, 0.0, "the wrap is not a jump");
        // a new connection: timestamps from 0 again, a different offset to this clock, the same steady path
        m.from_receiver(&control(TYPE_HANDSHAKE, &[0; 12]));
        for i in 0..10u32 {
            m.from_sender_at(&data_at(5000 + i, i * 1000, false), 9_000_000 + i64::from(i) * 1000);
        }
        let l = m.lateness().unwrap();
        assert_eq!((l.packets, l.max_ms), (20, 0.0));
    }

    #[test]
    fn a_restart_forgets_the_lateness_so_far_but_not_the_counts() {
        let m = SrtMonitor::receiving();
        m.from_sender_at(&data_at(0, 0, false), 0);
        m.from_sender_at(&data_at(1, 1_000, false), 500_000);
        assert_eq!(m.lateness().unwrap().max_ms, 499.0);
        m.restart_lateness();
        for seq in 2..10u32 {
            m.from_sender_at(&data_at(seq, seq * 1_000, false), 600_000 + i64::from(seq) * 1_000);
        }
        let l = m.lateness().unwrap();
        assert_eq!((l.packets, l.max_ms), (8, 0.0), "{l:?}");
        assert_eq!(m.snapshot().total.sent, 10);
    }

    #[test]
    fn outages_and_runs_of_missing_packets() {
        let m = SrtMonitor::receiving();
        let mut at = 0i64;
        for seq in (1..=10u32).chain(14..=20).chain(22..=30) {
            // nothing for 250 ms before 14 (13..=11 never came), 2 ms between the others
            at += if seq == 14 { 250_000 } else { 2_000 };
            m.from_sender_at(&data_at(seq, seq * 2000, false), at);
        }
        let l = m.lateness().unwrap();
        assert_eq!((l.loss_runs, l.longest_run), (2, 3));
        assert!((l.longest_gap_ms - 250.0).abs() < 1e-9, "{}", l.longest_gap_ms);
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
