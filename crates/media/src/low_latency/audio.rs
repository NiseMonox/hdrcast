//! The sound of control mode: the capture engine's 48 kHz stereo, as 16-bit PCM in 5 ms packets (sealed 'A'
//! datagrams), played by the viewer from a small buffer of its own.
//!
//! ```text
//!  0  4  seq      the packet's number, one more for every packet (big-endian)
//!  4  960         240 frames of two channels, 16-bit little-endian, left first
//! ```
//!
//! A lost packet is played as silence; a packet that comes too late for its turn is dropped. The viewer's
//! [`AudioJitter`] keeps about [`TARGET_MS`] buffered: enough for the jitter of a good path, little enough that the
//! sound stays with the picture. The two ends' sound clocks drift apart a little: the buffer makes up for it by
//! dropping or repeating one frame in a hundred while it is off its target, which nobody hears.

use std::collections::VecDeque;

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// 5 ms.
pub const PACKET_FRAMES: usize = 240;
pub const PACKET_BYTES: usize = 4 + PACKET_FRAMES * CHANNELS * 2;
/// What the viewer keeps buffered, in ms.
pub const TARGET_MS: usize = 30;
/// A gap of more packets than this is a new start, not a loss to fill with silence.
const MAX_GAP: u32 = 40;

/// Cuts the engine's samples (f32, interleaved stereo) into packets, host side.
#[derive(Default)]
pub struct AudioPacketizer {
    pending: Vec<i16>,
    seq: u32,
    packet: Vec<u8>,
}

impl AudioPacketizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes interleaved stereo samples; hands each complete packet to `send`.
    pub fn push(&mut self, samples: &[f32], mut send: impl FnMut(&[u8])) {
        self.pending.extend(samples.iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0).round() as i16));
        let per_packet = PACKET_FRAMES * CHANNELS;
        let mut used = 0;
        while self.pending.len() - used >= per_packet {
            self.packet.clear();
            self.packet.extend_from_slice(&self.seq.to_be_bytes());
            for sample in &self.pending[used..used + per_packet] {
                self.packet.extend_from_slice(&sample.to_le_bytes());
            }
            send(&self.packet);
            self.seq = self.seq.wrapping_add(1);
            used += per_packet;
        }
        self.pending.drain(..used);
    }
}

/// The packet's number and its samples (interleaved stereo); `None` for anything that is not a packet.
pub fn parse(message: &[u8]) -> Option<(u32, Vec<i16>)> {
    if message.len() != PACKET_BYTES {
        return None;
    }
    let seq = u32::from_be_bytes(message[0..4].try_into().ok()?);
    Some((seq, message[4..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()))
}

/// Counts of the viewer's sound.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitterStats {
    pub packets: u64,
    /// Packets that never came (played as silence).
    pub lost: u64,
    /// Packets that came after their turn.
    pub late: u64,
    /// Times the buffer ran dry.
    pub underruns: u64,
}

/// The viewer's buffer between the network and the sound card.
pub struct AudioJitter {
    samples: VecDeque<i16>,
    next_seq: Option<u32>,
    target_frames: usize,
    /// Playing; false while (re)filling up to the target.
    playing: bool,
    /// Frames pulled since the last drop or repeat (the 1-in-100 correction).
    since_adjust: usize,
    stats: JitterStats,
}

impl Default for AudioJitter {
    fn default() -> Self {
        Self::new(TARGET_MS)
    }
}

impl AudioJitter {
    pub fn new(target_ms: usize) -> Self {
        Self {
            samples: VecDeque::new(),
            next_seq: None,
            target_frames: (target_ms * RATE as usize / 1000).max(PACKET_FRAMES),
            playing: false,
            since_adjust: 0,
            stats: JitterStats::default(),
        }
    }

    pub fn stats(&self) -> JitterStats {
        self.stats
    }

    /// Frames waiting to be played.
    pub fn buffered_frames(&self) -> usize {
        self.samples.len() / CHANNELS
    }

    /// One packet from the network (interleaved stereo samples).
    pub fn push(&mut self, seq: u32, samples: &[i16]) {
        if let Some(next) = self.next_seq {
            let ahead = seq.wrapping_sub(next);
            if ahead as i32 <= -1 {
                self.stats.late += 1;
                return;
            }
            if ahead > MAX_GAP {
                self.samples.clear();
                self.playing = false;
            } else if ahead > 0 {
                // the missing ones as silence, so what follows keeps its place
                self.stats.lost += u64::from(ahead);
                self.samples.extend(std::iter::repeat_n(0, ahead as usize * PACKET_FRAMES * CHANNELS));
            }
        }
        self.stats.packets += 1;
        self.next_seq = Some(seq.wrapping_add(1));
        self.samples.extend(samples);
        // after a stall the network delivers in a burst: never keep more than a packet past twice the target
        let most = (2 * self.target_frames + PACKET_FRAMES) * CHANNELS;
        if self.samples.len() > most {
            let excess = self.samples.len() - self.target_frames * CHANNELS;
            self.samples.drain(..excess - excess % CHANNELS);
        }
    }

    /// Fills `out` (interleaved stereo) for the sound card: silence while filling up or when the buffer runs dry.
    pub fn pull(&mut self, out: &mut [i16]) {
        if !self.playing {
            if self.buffered_frames() < self.target_frames {
                out.fill(0);
                return;
            }
            self.playing = true;
        }
        let frames = out.len() / CHANNELS;
        let level = self.buffered_frames();
        let (high, low) = (self.target_frames + PACKET_FRAMES, self.target_frames / 2);
        for frame in 0..frames {
            self.since_adjust += 1;
            if self.since_adjust >= 100 && self.buffered_frames() > CHANNELS {
                if level > high {
                    // too much buffered: skip one frame
                    self.samples.drain(..CHANNELS);
                    self.since_adjust = 0;
                } else if level < low && frame > 0 {
                    // too little: play the previous frame once more
                    let at = (frame - 1) * CHANNELS;
                    out.copy_within(at..at + CHANNELS, frame * CHANNELS);
                    self.since_adjust = 0;
                    continue;
                }
            }
            if self.samples.len() < CHANNELS {
                out[frame * CHANNELS..].fill(0);
                self.stats.underruns += 1;
                self.playing = false;
                return;
            }
            for channel in 0..CHANNELS {
                out[frame * CHANNELS + channel] = self.samples.pop_front().unwrap_or(0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(count: usize) -> Vec<Vec<u8>> {
        let mut packetizer = AudioPacketizer::new();
        let mut out = Vec::new();
        // 1.5 packets per push, so the packetizer has to carry samples over
        let samples: Vec<f32> = (0..count * PACKET_FRAMES * CHANNELS).map(|i| ((i % 1000) as f32 / 1000.0) - 0.5).collect();
        for chunk in samples.chunks(PACKET_FRAMES * CHANNELS * 3 / 2) {
            packetizer.push(chunk, |p| out.push(p.to_vec()));
        }
        out
    }

    #[test]
    fn samples_go_out_in_numbered_5_ms_packets() {
        let out = packets(4);
        assert_eq!(out.len(), 4);
        for (n, packet) in out.iter().enumerate() {
            assert_eq!(packet.len(), PACKET_BYTES);
            let (seq, samples) = parse(packet).unwrap();
            assert_eq!(seq as usize, n);
            assert_eq!(samples.len(), PACKET_FRAMES * CHANNELS);
        }
        let (_, first) = parse(&out[0]).unwrap();
        assert_eq!(first[0], (-0.5f32 * 32767.0).round() as i16);
        assert_eq!(parse(&out[0][..10]), None);
    }

    #[test]
    fn the_buffer_fills_to_its_target_before_it_plays_and_fills_gaps_with_silence() {
        let mut jitter = AudioJitter::new(10); // 480 frames: two packets
        let block = vec![1i16; PACKET_FRAMES * CHANNELS];
        let mut out = vec![9i16; 480];
        jitter.push(0, &block);
        jitter.pull(&mut out);
        assert!(out.iter().all(|&s| s == 0), "not yet at the target");
        jitter.push(1, &block);
        jitter.push(3, &block); // 2 lost
        assert_eq!(jitter.stats().lost, 1);
        assert_eq!(jitter.buffered_frames(), 4 * PACKET_FRAMES);
        jitter.push(2, &block); // too late now
        assert_eq!(jitter.stats().late, 1);
        let mut played = Vec::new();
        for _ in 0..5 {
            jitter.pull(&mut out);
            played.extend_from_slice(&out);
        }
        // packets 0 and 1, then the silence for 2, then 3 (allowing for the 1-in-100 corrections)
        assert_eq!(played[0], 1);
        assert!(played[2 * PACKET_FRAMES * CHANNELS..(2 * PACKET_FRAMES + 200) * CHANNELS].iter().all(|&s| s == 0));
        assert_eq!(jitter.stats().underruns, 1, "ran dry at the end");
    }

    #[test]
    fn a_burst_is_cut_back_and_a_long_gap_starts_over() {
        let mut jitter = AudioJitter::new(30);
        let block = vec![5i16; PACKET_FRAMES * CHANNELS];
        for seq in 0..40 {
            jitter.push(seq, &block);
        }
        assert!(jitter.buffered_frames() <= 2 * jitter.target_frames + PACKET_FRAMES, "{}", jitter.buffered_frames());
        jitter.push(1000, &block);
        assert_eq!(jitter.buffered_frames(), PACKET_FRAMES, "started over");
        assert_eq!(jitter.stats().lost, 0);
    }

    #[test]
    fn the_level_drifts_back_to_the_target() {
        let mut jitter = AudioJitter::new(30);
        let block = vec![3i16; PACKET_FRAMES * CHANNELS];
        let mut seq = 0;
        for _ in 0..15 {
            jitter.push(seq, &block);
            seq += 1;
        }
        // the host's clock runs fast: a packet more now and then, the level must not climb
        let mut out = vec![0i16; PACKET_FRAMES * CHANNELS];
        for round in 0..2000 {
            jitter.push(seq, &block);
            seq += 1;
            if round % 200 == 0 {
                jitter.push(seq, &block);
                seq += 1;
            }
            jitter.pull(&mut out);
        }
        let level = jitter.buffered_frames();
        assert!(level <= jitter.target_frames + 2 * PACKET_FRAMES, "{level}");
        assert_eq!(jitter.stats().underruns, 0);
    }
}
