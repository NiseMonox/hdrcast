//! The network report in the log: what the SRT packets of a stream said about its path (docs/design.md §8.11),
//! the first time a quarter of a minute in, then every half minute while packets are being lost. These lines
//! are what a friend's "the picture breaks up" is diagnosed from.

use std::time::{Duration, Instant};

use hdrcast_p2p::{SrtCounts, SrtStats};
use serde_json::{json, Value};

const FIRST_AFTER: Duration = Duration::from_secs(15);
const EVERY: Duration = Duration::from_secs(30);

/// Which end of the stream a report is written on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// The host: it sees what it sends, what the viewer reports missing and what it gave up on.
    Host,
    /// The viewer: it sees what arrives and what never did in time.
    Viewer,
}

pub struct NetReport {
    end: End,
    started: Instant,
    /// When the last line was written, and the counts then.
    last: Option<(Instant, SrtCounts)>,
    /// The host's fan-out queue clears for this viewer already told about.
    queue_clears: u32,
}

impl NetReport {
    pub fn new(end: End) -> NetReport {
        NetReport { end, started: Instant::now(), last: None, queue_clears: 0 }
    }

    /// The line to log now, if one is due (without the "[net] name" prefix).
    pub fn line(&mut self, stats: &SrtStats) -> Option<String> {
        let now = Instant::now();
        let (since, before) = match self.last {
            None if now.duration_since(self.started) < FIRST_AFTER => return None,
            None => (self.started, SrtCounts::default()),
            Some((at, _)) if now.duration_since(at) < EVERY => return None,
            Some((at, counts)) => (at, counts),
        };
        let first = self.last.is_none();
        self.last = Some((now, stats.total));
        let d = stats.total.since(&before);
        if d.sent == 0 || (!first && d.lost == 0 && d.dropped == 0 && d.unrecovered == 0) {
            return None; // nothing flowing, or nothing worth a line
        }
        let seconds = now.duration_since(since).as_secs_f64();
        let mbps = d.sent_bytes as f64 * 8.0 / 1e6 / seconds;
        let rtt = if stats.rtt_ms > 0.0 { format!("; round trip {:.0} ms", stats.rtt_ms) } else { String::new() };
        Some(match self.end {
            // (SRT's receive rate in the acknowledgements is the rate of the last burst, not the stream's: left out)
            End::Host => format!(
                "last {seconds:.0} s: {mbps:.1} Mbps sent, {:.2}% lost on the way, {:.2}% sent again, {} too late to send again{rtt}; SRT's estimate of the path: {:.0} Mbps",
                d.loss_percent(),
                d.resent_percent(),
                d.dropped,
                stats.capacity_mbps,
            ),
            End::Viewer => format!(
                "last {seconds:.0} s: {mbps:.1} Mbps arrived, {:.2}% lost on the way, {} never recovered in time{rtt}",
                d.loss_percent(),
                d.unrecovered,
            ),
        })
    }
}

impl NetReport {
    /// A line when the host's fan-out had to empty this viewer's queue since the last call: its remux child did
    /// not take the stream as fast as it came, and half a second of it was dropped each time. Before any of the
    /// stream has gone out (`flowing` false) that is expected: the child is still waiting for the viewer's player.
    pub fn queue_line(&mut self, clears: u32, flowing: bool) -> Option<String> {
        if clears < self.queue_clears {
            self.queue_clears = 0; // a restarted child has a new queue
        }
        let new = clears - self.queue_clears;
        self.queue_clears = clears;
        (new > 0 && flowing).then(|| {
            format!("its SRT sender did not keep up: half a second of the stream dropped {new} x before sending (the picture freezes until a keyframe)")
        })
    }
}

/// The figures the page shows for a path (over the last few seconds).
pub fn status(stats: &SrtStats, end: End) -> Value {
    json!({
        "lossPercent": stats.recent.loss_percent(),
        // the host knows what it gave up on, the viewer what never arrived in time
        "lostForGood": match end { End::Host => stats.recent.dropped, End::Viewer => stats.recent.unrecovered },
        "mbps": stats.recent_mbps(),
        "rttMs": stats.rtt_ms,
    })
}
