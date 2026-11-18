//! The connection test's verdict (docs/design.md §5.1): from what a test session measured (how late SRT's packets
//! came against their own timestamps, how many went missing, the round trip), the lowest delay (SRT latency) that
//! still gets nearly every packet over in time, with the player's buffer and the resend headroom to go with it.
//!
//! The delay has to cover three things, and the test session, joined with a generous delay so that nothing was
//! cut short, sees each of them:
//!
//! * how late packets come on their first send. The sender paces its packets at the stream's rate plus the
//!   headroom, so a keyframe takes a while to leave, and the path's jitter adds to that
//!   ([`Lateness::first_try_p999_ms`]);
//! * one resend: the receiver reports a gap once the next packet is there (a frame later at worst), and the resend
//!   takes a round trip;
//! * every resend that is lost as well. libsrt's receiver (live mode) reports a gap again every (RTT + 4·RTTVar)/2,
//!   at least every 20 ms, and its sender sends a packet again only once the last resend is RTT − 4·RTTVar old.
//!
//! The loss rate says how many resends to plan for: enough that a packet lost on every send is rarer than once in
//! ten minutes. The latest packet the test saw is a floor, and so is Haivision's RTT multiplier for the loss rate
//! (the SRT documentation's table: 3 × RTT up to 1 % loss ... 30 × RTT up to 40 %, the RTT taken as 20 ms at least).

use crate::srt_monitor::{Lateness, SrtCounts};

/// libsrt's shortest interval between periodic loss reports in live mode (LiveCC's `m_iMinNakInterval_us`).
const MIN_NAK_MS: f64 = 20.0;
/// How long a gap can go unnoticed: the last packet of a picture is lost, and the next picture's first packet
/// shows it a frame later (at 30 frames per second).
const GAP_UNSEEN_MS: f64 = 34.0;
/// A packet lost on every send may happen this often, at most: once in ten minutes.
const LOST_FOR_GOOD_PER_S: f64 = 1.0 / 600.0;
/// The stream's packets per second taken at least (about 10 Mbps): a test of a still picture sends far fewer than
/// a game will.
const MIN_PACKETS_PER_S: f64 = 1000.0;
/// Resends planned for, at most.
const MAX_ROUNDS: u32 = 6;
const MARGIN_FACTOR: f64 = 1.15;
const MARGIN_MS: f64 = 10.0;
/// Fewer packets than this make a rough figure.
const FEW_PACKETS: u64 = 1000;
/// The player's buffer (seconds) by how the path did. SRT already delivers each packet at its own time plus the
/// delay, so the player only has to cover the stream's own unevenness and the path's worse moments.
pub const BUFFER_GOOD: f64 = 0.3;
pub const BUFFER_FAIR: f64 = 0.8;
pub const BUFFER_POOR: f64 = 1.5;
/// SRT's own delay range as the settings allow it.
pub const MIN_LATENCY_MS: u32 = 20;
pub const MAX_LATENCY_MS: u32 = 30_000;

/// Haivision's RTT multipliers by loss rate (up to the percentage).
const RTT_MULTIPLIERS: [(f64, f64); 10] =
    [(1.0, 3.0), (3.0, 4.0), (7.0, 6.0), (10.0, 8.0), (12.0, 8.0), (21.0, 10.0), (25.0, 13.0), (27.0, 14.0), (30.0, 14.0), (40.0, 30.0)];

/// What a test session measured once its connection had settled.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Measured {
    pub lateness: Lateness,
    /// What happened to the packets over the same time.
    pub counts: SrtCounts,
    pub seconds: f64,
    pub rtt_ms: f64,
    pub rtt_var_ms: f64,
    /// The delay the test joined with: a packet later than that was not sent again.
    pub test_latency_ms: u32,
    /// SRT connections that started meanwhile (the path broke and came back).
    pub reconnects: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Little or no loss and a short delay: a small buffer will do.
    Good,
    /// Some loss, or a long way: a longer delay and a medium buffer.
    Fair,
    /// Heavy loss, or the path broke: watching will stutter now and then, whatever the delay.
    Poor,
    /// Nothing came, or most of it was lost.
    Unusable,
}

/// Which figure set the delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecidedBy {
    /// The resends a lost packet may need, from the loss rate and the round trip.
    Resends,
    /// The latest packet the test saw.
    Latest,
    /// Haivision's RTT multiplier for the loss rate.
    Table,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Note {
    /// Few packets came (this many): the host's picture hardly changed, or the test was short.
    FewPackets(u64),
    /// This many packets were lost for good, even with the test's delay.
    LostForGood(u64),
    /// The path broke and SRT connected again (this many times).
    Reconnected(u32),
    /// Packets came about as late as the test's delay allowed: the delay needed may be longer still.
    CutShort,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Advice {
    pub latency_ms: u32,
    pub buffer_seconds: f64,
    /// Resend headroom (oheadbw, %) the loss rate needs: 25 = the default.
    pub overhead_percent: u32,
    /// First sends lost, per hundred.
    pub loss_percent: f64,
    /// Resends the delay is planned for (1: a lost packet gets one more chance).
    pub rounds: u32,
    /// The parts of the delay: lateness on the first send (99.9th percentile), a resend, each further resend.
    pub first_try_ms: f64,
    pub resend_ms: f64,
    pub extra_round_ms: f64,
    /// The latest packet the test saw.
    pub latest_ms: f64,
    pub decided_by: DecidedBy,
    pub verdict: Verdict,
    pub notes: Vec<Note>,
}

/// The advice for what a test measured.
pub fn advise(m: &Measured) -> Advice {
    let sent = m.counts.sent;
    let loss_percent = m.counts.loss_percent();
    let mut notes = Vec::new();
    if sent < FEW_PACKETS {
        notes.push(Note::FewPackets(sent));
    }
    if m.counts.unrecovered > 0 {
        notes.push(Note::LostForGood(m.counts.unrecovered));
    }
    if m.reconnects > 0 {
        notes.push(Note::Reconnected(m.reconnects));
    }
    let late = &m.lateness;
    let cut_short = m.test_latency_ms > 0 && late.max_ms >= 0.9 * f64::from(m.test_latency_ms);
    if cut_short {
        notes.push(Note::CutShort);
    }

    // a loss rate of at least three in the packets seen: none lost in a short test does not mean none ever is
    let p = if sent == 0 { 1.0 } else { (m.counts.lost as f64 / sent as f64).max(3.0 / sent as f64).min(0.9) };
    let packets_per_s = (sent as f64 / m.seconds.max(0.001)).max(MIN_PACKETS_PER_S);
    let mut rounds = 1;
    while rounds < MAX_ROUNDS && p.powi(rounds as i32 + 1) * packets_per_s > LOST_FOR_GOOD_PER_S {
        rounds += 1;
    }

    let (rtt, var) = (m.rtt_ms.max(0.0), m.rtt_var_ms.max(0.0));
    let nak_every = MIN_NAK_MS.max((rtt + 4.0 * var) / 2.0);
    let first_try_ms = late.first_try_p999_ms.max(0.0);
    let resend_ms = GAP_UNSEEN_MS + rtt + 4.0 * var;
    let extra_round_ms = (rtt - 4.0 * var).max(0.0) + nak_every;
    let by_resends = first_try_ms + resend_ms + f64::from(rounds - 1) * extra_round_ms;
    let latest_ms = late.max_ms.max(0.0);
    let needed = by_resends.max(latest_ms) * MARGIN_FACTOR + MARGIN_MS;
    let by_table = rtt_multiplier(loss_percent).unwrap_or(30.0) * rtt.max(20.0);
    let decided_by = if by_table >= needed {
        DecidedBy::Table
    } else if latest_ms > by_resends {
        DecidedBy::Latest
    } else {
        DecidedBy::Resends
    };
    let mut latency_ms = round_up(needed.max(by_table));
    if cut_short {
        latency_ms = latency_ms.max(m.test_latency_ms);
    }

    let verdict = if sent == 0 || loss_percent >= 40.0 {
        Verdict::Unusable
    } else if m.counts.unrecovered > 0 || m.reconnects > 0 || loss_percent >= 10.0 || cut_short {
        Verdict::Poor
    } else if loss_percent >= 1.0 || latency_ms > 1000 {
        Verdict::Fair
    } else {
        Verdict::Good
    };
    let buffer_seconds = match verdict {
        Verdict::Good => BUFFER_GOOD,
        Verdict::Fair => BUFFER_FAIR,
        Verdict::Poor | Verdict::Unusable => BUFFER_POOR,
    };
    // Haivision: a constant loss rate needs at least 1.65 times as much headroom for the resends
    let overhead_percent = match (loss_percent * 1.65).ceil() as u32 {
        0..=25 => 25,
        26..=50 => 50,
        _ => 100,
    };

    Advice {
        latency_ms,
        buffer_seconds,
        overhead_percent,
        loss_percent,
        rounds,
        first_try_ms,
        resend_ms,
        extra_round_ms,
        latest_ms,
        decided_by,
        verdict,
        notes,
    }
}

/// Haivision's RTT multiplier for a loss rate (%); `None` above 40 %, where SRT cannot keep up.
pub fn rtt_multiplier(loss_percent: f64) -> Option<f64> {
    RTT_MULTIPLIERS.iter().find(|(upto, _)| loss_percent <= *upto).map(|(_, multiplier)| *multiplier)
}

/// A delay rounded up to a figure one would set: 10 ms steps up to 200 ms, 50 up to 1 s, 100 above; within the
/// settings' range.
pub fn round_up(ms: f64) -> u32 {
    let ms = ms.max(0.0).ceil() as u32;
    let step = match ms {
        0..=200 => 10,
        201..=1000 => 50,
        _ => 100,
    };
    ms.div_ceil(step).saturating_mul(step).clamp(MIN_LATENCY_MS, MAX_LATENCY_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measured(sent: u64, lost: u64, seconds: f64, rtt: f64, var: f64, first_try: f64, latest: f64) -> Measured {
        Measured {
            lateness: Lateness { packets: sent, first_try_p999_ms: first_try, max_ms: latest, ..Lateness::default() },
            counts: SrtCounts { sent, lost, resent: lost, ..SrtCounts::default() },
            seconds,
            rtt_ms: rtt,
            rtt_var_ms: var,
            test_latency_ms: 3000,
            reconnects: 0,
        }
    }

    #[test]
    fn delays_round_up_to_figures_one_would_set() {
        assert_eq!(round_up(0.0), 20);
        assert_eq!(round_up(61.0), 70);
        assert_eq!(round_up(200.0), 200);
        assert_eq!(round_up(201.0), 250);
        assert_eq!(round_up(1000.0), 1000);
        assert_eq!(round_up(1001.0), 1100);
        assert_eq!(round_up(99_999.0), 30_000);
    }

    #[test]
    fn haivisions_multipliers_by_loss_rate() {
        assert_eq!(rtt_multiplier(0.0), Some(3.0));
        assert_eq!(rtt_multiplier(1.0), Some(3.0));
        assert_eq!(rtt_multiplier(1.1), Some(4.0));
        assert_eq!(rtt_multiplier(8.0), Some(8.0));
        assert_eq!(rtt_multiplier(40.0), Some(30.0));
        assert_eq!(rtt_multiplier(41.0), None);
    }

    #[test]
    fn a_clean_short_path_gets_one_resend_and_a_small_buffer() {
        // 20 000 packets in 20 s, none lost, half a millisecond away
        let a = advise(&measured(20_000, 0, 20.0, 0.5, 0.1, 12.0, 15.0));
        assert_eq!(a.rounds, 1, "{a:?}");
        // 12 + 34 + 0.5 + 0.4 = 46.9, × 1.15 + 10 = 63.9: above Haivision's 3 × 20 ms
        assert_eq!(a.latency_ms, 70, "{a:?}");
        assert_eq!(a.decided_by, DecidedBy::Resends);
        assert_eq!((a.verdict, a.buffer_seconds, a.overhead_percent), (Verdict::Good, BUFFER_GOOD, 25));
        assert!(a.notes.is_empty(), "{a:?}");
    }

    #[test]
    fn one_percent_loss_needs_a_second_resend() {
        // 1 % of 1000 packets a second: a packet lost twice every 10 s, three times every 17 min
        let a = advise(&measured(20_000, 200, 20.0, 20.0, 2.0, 30.0, 90.0));
        assert_eq!(a.rounds, 2, "{a:?}");
        // a second resend: (20 - 8) + max(20, 14) = 32 ms; 30 + 62 + 32 = 124, × 1.15 + 10 = 152.6
        assert_eq!(a.extra_round_ms, 32.0);
        assert_eq!(a.latency_ms, 160, "{a:?}");
        assert_eq!((a.verdict, a.buffer_seconds), (Verdict::Fair, BUFFER_FAIR));
    }

    #[test]
    fn a_long_lossy_way_plans_for_several_resends() {
        // 5 % loss at 2000 packets a second, 180 ms away
        let a = advise(&measured(40_000, 2_000, 20.0, 180.0, 15.0, 60.0, 900.0));
        assert_eq!(a.rounds, 4, "{a:?}");
        // a resend again: (180 - 60) + (180 + 60) / 2 = 240 ms; 60 + 274 + 3 × 240 = 1054, × 1.15 + 10 = 1222
        assert_eq!(a.latency_ms, 1300, "{a:?}");
        assert_eq!(a.decided_by, DecidedBy::Resends);
        assert_eq!(a.verdict, Verdict::Fair);
    }

    #[test]
    fn the_latest_packet_seen_is_a_floor() {
        let a = advise(&measured(20_000, 0, 20.0, 0.5, 0.1, 12.0, 400.0));
        assert_eq!((a.latency_ms, a.decided_by), (500, DecidedBy::Latest), "{a:?}");
    }

    #[test]
    fn haivisions_table_is_a_floor() {
        // nothing lost 100 ms away: one resend would take 34 + 100 ms (164 with the margin), the table says 3 × 100
        let a = advise(&measured(20_000, 0, 20.0, 100.0, 0.0, 0.0, 0.0));
        assert_eq!((a.decided_by, a.latency_ms, a.verdict), (DecidedBy::Table, 300, Verdict::Good), "{a:?}");
    }

    #[test]
    fn heavy_loss_asks_for_more_headroom_and_caps_the_resends() {
        let a = advise(&measured(20_000, 4_000, 20.0, 20.0, 2.0, 30.0, 300.0));
        assert_eq!((a.overhead_percent, a.verdict), (50, Verdict::Poor), "{a:?}");
        let a = advise(&measured(20_000, 7_000, 20.0, 20.0, 2.0, 30.0, 300.0));
        assert_eq!((a.rounds, a.overhead_percent, a.verdict), (MAX_ROUNDS, 100, Verdict::Poor), "{a:?}");
        let a = advise(&measured(20_000, 9_000, 20.0, 20.0, 2.0, 30.0, 300.0));
        assert_eq!(a.verdict, Verdict::Unusable, "{a:?}");
    }

    #[test]
    fn what_went_wrong_is_noted() {
        let mut m = measured(500, 0, 20.0, 0.5, 0.1, 12.0, 2900.0);
        m.counts.unrecovered = 3;
        m.reconnects = 1;
        let a = advise(&m);
        assert_eq!(a.notes, vec![Note::FewPackets(500), Note::LostForGood(3), Note::Reconnected(1), Note::CutShort]);
        assert_eq!((a.verdict, a.buffer_seconds), (Verdict::Poor, BUFFER_POOR));
        assert!(a.latency_ms >= 3000, "cut short: at least the test's own delay: {a:?}");
    }

    #[test]
    fn nothing_came() {
        let a = advise(&measured(0, 0, 20.0, 0.0, 0.0, 0.0, 0.0));
        assert_eq!(a.verdict, Verdict::Unusable);
        assert_eq!(a.notes, vec![Note::FewPackets(0)]);
    }
}
