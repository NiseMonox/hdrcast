//! What the viewer asks of the host and what the host answers (sealed 'R' and 'r' datagrams).
//!
//! ```text
//! 'R', viewer → host
//!   1 keyframe   seq (u32): the viewer's request number, from 1; lost (u32): the newest picture it lost
//!   2 report     sent (u64): the viewer's clock in µs, echoed back; rtt (u32): its latest round trip in µs (0 =
//!                none yet); then its counts since it started (u32 each): pictures shown, pictures lost, shards the
//!                parity restored, keyframes it waited for
//! 'r', host → viewer
//!   1 keyframe   seq (u32), as asked; outcome (u8): 1 coming, 2 already sent after that picture, 3 cannot (the
//!                encoder is not running)
//!   2 echo       sent (u64): the report's, unchanged; held (u32): µs between the report arriving and this answer
//! ```
//!
//! All numbers are big-endian. The viewer sends a report every half second: the echo gives it the round trip, and
//! the host its counts for the page and the log.

pub const KEYFRAME: u8 = 1;
pub const REPORT: u8 = 2;
pub const ECHO: u8 = 2;

/// What happens about a keyframe request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The encoder makes one now (or one asked for is on its way).
    Coming = 1,
    /// One already went out after the picture that was lost.
    AlreadySent = 2,
    /// Nothing encodes at the moment.
    Cannot = 3,
}

impl Outcome {
    fn from_u8(v: u8) -> Option<Outcome> {
        match v {
            1 => Some(Outcome::Coming),
            2 => Some(Outcome::AlreadySent),
            3 => Some(Outcome::Cannot),
            _ => None,
        }
    }
}

/// The viewer's counts, as reported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub shown: u32,
    pub lost: u32,
    pub restored_shards: u32,
    pub keyframe_waits: u32,
}

/// A viewer's message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Keyframe { seq: u32, lost: u32 },
    Report { sent_us: u64, rtt_us: u32, counts: Counts },
}

/// A host's message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Keyframe { seq: u32, outcome: Outcome },
    Echo { sent_us: u64, held_us: u32 },
}

fn u32_at(m: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(m.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(m: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(m.get(at..at + 8)?.try_into().ok()?))
}

impl Request {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut m = Vec::with_capacity(29);
        match *self {
            Request::Keyframe { seq, lost } => {
                m.push(KEYFRAME);
                m.extend_from_slice(&seq.to_be_bytes());
                m.extend_from_slice(&lost.to_be_bytes());
            }
            Request::Report { sent_us, rtt_us, counts } => {
                m.push(REPORT);
                m.extend_from_slice(&sent_us.to_be_bytes());
                m.extend_from_slice(&rtt_us.to_be_bytes());
                for n in [counts.shown, counts.lost, counts.restored_shards, counts.keyframe_waits] {
                    m.extend_from_slice(&n.to_be_bytes());
                }
            }
        }
        m
    }

    pub fn parse(m: &[u8]) -> Option<Request> {
        match *m.first()? {
            KEYFRAME if m.len() == 9 => Some(Request::Keyframe { seq: u32_at(m, 1)?, lost: u32_at(m, 5)? }),
            REPORT if m.len() == 29 => Some(Request::Report {
                sent_us: u64_at(m, 1)?,
                rtt_us: u32_at(m, 9)?,
                counts: Counts { shown: u32_at(m, 13)?, lost: u32_at(m, 17)?, restored_shards: u32_at(m, 21)?, keyframe_waits: u32_at(m, 25)? },
            }),
            _ => None,
        }
    }
}

impl Answer {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut m = Vec::with_capacity(13);
        match *self {
            Answer::Keyframe { seq, outcome } => {
                m.push(KEYFRAME);
                m.extend_from_slice(&seq.to_be_bytes());
                m.push(outcome as u8);
            }
            Answer::Echo { sent_us, held_us } => {
                m.push(ECHO);
                m.extend_from_slice(&sent_us.to_be_bytes());
                m.extend_from_slice(&held_us.to_be_bytes());
            }
        }
        m
    }

    pub fn parse(m: &[u8]) -> Option<Answer> {
        match *m.first()? {
            KEYFRAME if m.len() == 6 => Some(Answer::Keyframe { seq: u32_at(m, 1)?, outcome: Outcome::from_u8(m[5])? }),
            ECHO if m.len() == 13 => Some(Answer::Echo { sent_us: u64_at(m, 1)?, held_us: u32_at(m, 9)? }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_answers_come_back_as_they_went() {
        let requests = [
            Request::Keyframe { seq: 1, lost: 4_000_000_000 },
            Request::Report { sent_us: 1 << 50, rtt_us: 2_500, counts: Counts { shown: 3600, lost: 2, restored_shards: 77, keyframe_waits: 1 } },
        ];
        for r in requests {
            assert_eq!(Request::parse(&r.to_bytes()), Some(r));
        }
        let answers = [
            Answer::Keyframe { seq: 9, outcome: Outcome::Coming },
            Answer::Keyframe { seq: 9, outcome: Outcome::AlreadySent },
            Answer::Keyframe { seq: 9, outcome: Outcome::Cannot },
            Answer::Echo { sent_us: 123_456_789, held_us: 15 },
        ];
        for a in answers {
            assert_eq!(Answer::parse(&a.to_bytes()), Some(a));
        }
        assert_eq!(Request::Keyframe { seq: 2, lost: 3 }.to_bytes(), [1, 0, 0, 0, 2, 0, 0, 0, 3]);
    }

    #[test]
    fn anything_else_is_not_understood() {
        for bad in [&[][..], &[1, 0, 0, 0, 1][..], &[3, 0][..], &[2; 25][..]] {
            assert_eq!(Request::parse(bad), None, "{bad:?}");
        }
        assert_eq!(Answer::parse(&[1, 0, 0, 0, 1, 4]), None, "outcome 4");
        assert_eq!(Answer::parse(&[2; 12]), None);
    }
}
