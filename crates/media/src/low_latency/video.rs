//! Pictures in pieces: the host cuts each picture into shards of equal size that fit a datagram and adds
//! Reed-Solomon parity shards ([`Packetizer`]); the viewer puts the picture together from any `data` of its
//! `data + parity` shards ([`Assembler`]). A picture that cannot be put together is given up as soon as a shard of a
//! later picture arrives, or when its shards stop coming ([`STALL`]): nothing is sent again, so waiting longer
//! would only hold the stream back (Moonlight's rule).
//!
//! A shard (the message inside a sealed 'V' datagram):
//!
//! ```text
//!  0  4  frame    the picture's number (big-endian): one more for every picture the host sends
//!  4  1  flags    1 = a keyframe
//!  5  1  0
//!  6  2  index    0 .. data + parity - 1: the data shards first, then the parity
//!  8  2  data     how many data shards the picture has
//! 10  2  parity   how many parity shards
//! 12  ..          the shard: the same even number of bytes for every shard of the picture
//! ```
//!
//! The data shards together are the picture's payload, zero-padded at the end: a header (the picture's length in
//! bytes, how long the host held it from the capture to this point, how long its encoder took, each a big-endian
//! u32) and the picture itself (an HEVC access unit, Annex B).

use std::time::{Duration, Instant};

use reed_solomon_simd::{ReedSolomonDecoder, ReedSolomonEncoder};

use super::{seal, MAX_DATAGRAM};

pub const SHARD_HEADER: usize = 12;
/// The largest shard: what a side datagram has room for after the seal and the shard's header (even, as
/// Reed-Solomon over GF(2^16) needs).
pub const MAX_SHARD: usize = (MAX_DATAGRAM - seal::OVERHEAD - SHARD_HEADER) & !1;
pub const PAYLOAD_HEADER: usize = 12;
/// Parity shards per picture at least: one lost packet of a small picture costs nothing, nor do two.
pub const MIN_PARITY: usize = 2;
/// Parity in percent of the data shards (Moonlight's default).
pub const DEFAULT_FEC_PERCENT: u32 = 20;
/// A picture whose shards stopped coming this long ago will not be completed: the packets of one picture leave the
/// host back to back, so a gap this long means the rest was lost.
pub const STALL: Duration = Duration::from_millis(50);

const KEYFRAME: u8 = 1;

/// A picture to send.
#[derive(Debug, Clone, Copy)]
pub struct Picture<'a> {
    pub frame: u32,
    pub keyframe: bool,
    /// From the capture of the picture to its packets, in µs.
    pub host_us: u32,
    /// The encoder's part of it, in µs.
    pub encode_us: u32,
    pub data: &'a [u8],
}

/// How a picture was cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cut {
    pub data: usize,
    pub parity: usize,
    pub shard_bytes: usize,
}

/// Cuts pictures into shards with parity, host side.
pub struct Packetizer {
    fec_percent: u32,
    encoder: Option<ReedSolomonEncoder>,
    payload: Vec<u8>,
    shard: Vec<u8>,
}

impl Packetizer {
    /// `fec_percent`: parity shards in percent of the data shards (at least [`MIN_PARITY`] per picture).
    pub fn new(fec_percent: u32) -> Self {
        Self { fec_percent, encoder: None, payload: Vec::new(), shard: Vec::new() }
    }

    pub fn fec_percent(&self) -> u32 {
        self.fec_percent
    }

    pub fn set_fec_percent(&mut self, percent: u32) {
        self.fec_percent = percent;
    }

    /// How a payload of `bytes` is cut: as few data shards as fit it, of a size that leaves almost no padding.
    pub fn cut(&self, bytes: usize) -> Cut {
        let data = bytes.div_ceil(MAX_SHARD).max(1);
        let shard_bytes = (bytes.div_ceil(data).max(2) + 1) & !1;
        let parity = (data * self.fec_percent as usize).div_ceil(100).max(MIN_PARITY);
        Cut { data, parity, shard_bytes }
    }

    /// Hands every shard of `picture` to `send` (data first, then parity), each a message for the seal. Fails only
    /// for a picture too large to cut (tens of MB).
    pub fn split(&mut self, picture: &Picture, mut send: impl FnMut(&[u8])) -> Result<Cut, String> {
        self.payload.clear();
        self.payload.extend_from_slice(&(picture.data.len() as u32).to_be_bytes());
        self.payload.extend_from_slice(&picture.host_us.to_be_bytes());
        self.payload.extend_from_slice(&picture.encode_us.to_be_bytes());
        self.payload.extend_from_slice(picture.data);
        let cut = self.cut(self.payload.len());
        if cut.data + cut.parity > usize::from(u16::MAX) || !ReedSolomonEncoder::supports(cut.data, cut.parity) {
            return Err(format!("a picture of {} bytes is too large to send", picture.data.len()));
        }
        self.payload.resize(cut.data * cut.shard_bytes, 0);

        let header = |index: usize, shard: &mut Vec<u8>| {
            shard.clear();
            shard.extend_from_slice(&picture.frame.to_be_bytes());
            shard.push(if picture.keyframe { KEYFRAME } else { 0 });
            shard.push(0);
            shard.extend_from_slice(&(index as u16).to_be_bytes());
            shard.extend_from_slice(&(cut.data as u16).to_be_bytes());
            shard.extend_from_slice(&(cut.parity as u16).to_be_bytes());
        };
        for (index, chunk) in self.payload.chunks(cut.shard_bytes).enumerate() {
            header(index, &mut self.shard);
            self.shard.extend_from_slice(chunk);
            send(&self.shard);
        }
        let encoder = match self.encoder.as_mut() {
            Some(encoder) => {
                encoder.reset(cut.data, cut.parity, cut.shard_bytes).map_err(|e| e.to_string())?;
                encoder
            }
            None => self.encoder.insert(ReedSolomonEncoder::new(cut.data, cut.parity, cut.shard_bytes).map_err(|e| e.to_string())?),
        };
        for chunk in self.payload.chunks(cut.shard_bytes) {
            encoder.add_original_shard(chunk).map_err(|e| e.to_string())?;
        }
        let result = encoder.encode().map_err(|e| e.to_string())?;
        for (index, parity) in result.recovery_iter().enumerate() {
            header(cut.data + index, &mut self.shard);
            self.shard.extend_from_slice(parity);
            send(&self.shard);
        }
        Ok(cut)
    }
}

/// A picture put together by the viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assembled {
    pub frame: u32,
    pub keyframe: bool,
    pub host_us: u32,
    pub encode_us: u32,
    pub data: Vec<u8>,
    /// Data shards the parity had to stand in for.
    pub recovered: usize,
}

/// What the assembler found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Picture(Assembled),
    /// Pictures `first..=last` are lost: never seen, or not enough of them arrived.
    Lost { first: u32, last: u32 },
}

/// Counts since the assembler started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssemblerStats {
    pub pictures: u64,
    /// Pictures the parity saved.
    pub recovered_pictures: u64,
    pub recovered_shards: u64,
    pub lost_pictures: u64,
    pub shards: u64,
    /// Shards of pictures already finished or given up (overtaken on the way, or a parity shard nobody needed).
    pub late_shards: u64,
    pub bad_shards: u64,
}

struct Pending {
    frame: u32,
    keyframe: bool,
    cut: Cut,
    /// `data + parity` shards of `shard_bytes` each, in index order.
    shards: Vec<u8>,
    have: Vec<bool>,
    data_have: usize,
    parity_have: usize,
    last_shard: Instant,
}

/// Puts pictures together on the viewer, in order.
#[derive(Default)]
pub struct Assembler {
    /// The number of the picture expected next (the one after the last finished or given up).
    next: Option<u32>,
    pending: Option<Pending>,
    decoder: Option<ReedSolomonDecoder>,
    stats: AssemblerStats,
}

/// `a` comes after `b` (picture numbers wrap around).
fn after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

impl Assembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stats(&self) -> AssemblerStats {
        self.stats
    }

    /// One shard (an opened 'V' message) that arrived at `now`; what it completed or showed lost goes into `out`.
    pub fn push(&mut self, message: &[u8], now: Instant, out: &mut Vec<Event>) {
        self.stats.shards += 1;
        if message.len() <= SHARD_HEADER {
            self.stats.bad_shards += 1;
            return;
        }
        let frame = u32::from_be_bytes(message[0..4].try_into().expect("4 bytes"));
        let keyframe = message[4] & KEYFRAME != 0;
        let index = usize::from(u16::from_be_bytes([message[6], message[7]]));
        let data = usize::from(u16::from_be_bytes([message[8], message[9]]));
        let parity = usize::from(u16::from_be_bytes([message[10], message[11]]));
        let shard = &message[SHARD_HEADER..];
        let cut = Cut { data, parity, shard_bytes: shard.len() };
        if data == 0 || index >= data + parity || shard.len() % 2 != 0 || shard.len() > MAX_SHARD {
            self.stats.bad_shards += 1;
            return;
        }
        if self.next.is_some_and(|next| after(next, frame)) {
            self.stats.late_shards += 1; // finished or given up already
            return;
        }
        match &self.pending {
            Some(pending) if pending.frame == frame => {}
            Some(pending) if after(frame, pending.frame) => {
                // a later picture began: the one in progress is not going to be completed
                let given_up = pending.frame;
                self.pending = None;
                self.lose(given_up, frame.wrapping_sub(1), out);
                self.begin(frame, keyframe, cut, now);
            }
            Some(_) => {
                // older than the one in progress, which already gave it up
                self.stats.late_shards += 1;
                return;
            }
            None => {
                if let Some(next) = self.next.filter(|&next| next != frame) {
                    self.lose(next, frame.wrapping_sub(1), out);
                }
                self.begin(frame, keyframe, cut, now);
            }
        }
        let Some(pending) = self.pending.as_mut() else { return };
        if pending.cut != cut {
            self.stats.bad_shards += 1;
            return;
        }
        if pending.have[index] {
            self.stats.late_shards += 1;
            return;
        }
        pending.have[index] = true;
        pending.last_shard = now;
        pending.shards[index * cut.shard_bytes..(index + 1) * cut.shard_bytes].copy_from_slice(shard);
        if index < data {
            pending.data_have += 1;
        } else {
            pending.parity_have += 1;
        }
        if pending.data_have + pending.parity_have >= data {
            self.finish(out);
        }
    }

    /// Gives up a picture whose shards stopped coming [`STALL`] ago (the rest of it was lost on the way).
    pub fn tick(&mut self, now: Instant, out: &mut Vec<Event>) {
        let stalled = self.pending.as_ref().filter(|p| now.saturating_duration_since(p.last_shard) >= STALL).map(|p| p.frame);
        if let Some(frame) = stalled {
            self.pending = None;
            self.lose(frame, frame, out);
        }
    }

    /// Forgets where the stream was: the next shard starts it again (after the host's stream restarted).
    pub fn reset(&mut self) {
        self.next = None;
        self.pending = None;
    }

    fn begin(&mut self, frame: u32, keyframe: bool, cut: Cut, now: Instant) {
        let total = cut.data + cut.parity;
        self.pending = Some(Pending {
            frame,
            keyframe,
            cut,
            shards: vec![0; total * cut.shard_bytes],
            have: vec![false; total],
            data_have: 0,
            parity_have: 0,
            last_shard: now,
        });
    }

    fn lose(&mut self, first: u32, last: u32, out: &mut Vec<Event>) {
        self.stats.lost_pictures += u64::from(last.wrapping_sub(first)) + 1;
        self.next = Some(last.wrapping_add(1));
        out.push(Event::Lost { first, last });
    }

    fn finish(&mut self, out: &mut Vec<Event>) {
        let Some(mut pending) = self.pending.take() else { return };
        let cut = pending.cut;
        let missing = cut.data - pending.data_have;
        if missing > 0 && !self.restore(&mut pending) {
            self.stats.bad_shards += 1;
            self.lose(pending.frame, pending.frame, out);
            return;
        }
        self.next = Some(pending.frame.wrapping_add(1));
        let payload = &pending.shards[..cut.data * cut.shard_bytes];
        let word = |at: usize| u32::from_be_bytes(payload[at..at + 4].try_into().expect("4 bytes"));
        let length = word(0) as usize;
        if payload.len() < PAYLOAD_HEADER || length > payload.len() - PAYLOAD_HEADER {
            self.stats.bad_shards += 1;
            self.lose(pending.frame, pending.frame, out);
            return;
        }
        self.stats.pictures += 1;
        if missing > 0 {
            self.stats.recovered_pictures += 1;
            self.stats.recovered_shards += missing as u64;
        }
        out.push(Event::Picture(Assembled {
            frame: pending.frame,
            keyframe: pending.keyframe,
            host_us: word(4),
            encode_us: word(8),
            data: payload[PAYLOAD_HEADER..PAYLOAD_HEADER + length].to_vec(),
            recovered: missing,
        }));
    }

    /// Fills the missing data shards from the parity. False when the shards do not add up (a broken sender).
    fn restore(&mut self, pending: &mut Pending) -> bool {
        let cut = pending.cut;
        let decoder = match self.decoder.as_mut() {
            Some(decoder) => {
                if decoder.reset(cut.data, cut.parity, cut.shard_bytes).is_err() {
                    return false;
                }
                decoder
            }
            None => match ReedSolomonDecoder::new(cut.data, cut.parity, cut.shard_bytes) {
                Ok(decoder) => self.decoder.insert(decoder),
                Err(_) => return false,
            },
        };
        let shard = |index: usize| &pending.shards[index * cut.shard_bytes..(index + 1) * cut.shard_bytes];
        for index in (0..cut.data + cut.parity).filter(|&i| pending.have[i]) {
            let added = if index < cut.data {
                decoder.add_original_shard(index, shard(index))
            } else {
                decoder.add_recovery_shard(index - cut.data, shard(index))
            };
            if added.is_err() {
                return false;
            }
        }
        let Ok(result) = decoder.decode() else { return false };
        for (index, restored) in result.restored_original_iter() {
            pending.shards[index * cut.shard_bytes..(index + 1) * cut.shard_bytes].copy_from_slice(restored);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(frame: u32, bytes: usize) -> Vec<u8> {
        (0..bytes).map(|i| (i as u32).wrapping_mul(31).wrapping_add(frame) as u8).collect()
    }

    /// The shards of picture `frame` (`bytes` long).
    fn shards(packetizer: &mut Packetizer, frame: u32, bytes: usize, keyframe: bool) -> Vec<Vec<u8>> {
        let data = picture(frame, bytes);
        let mut out = Vec::new();
        packetizer
            .split(&Picture { frame, keyframe, host_us: frame.wrapping_add(4000), encode_us: 2000, data: &data }, |s| out.push(s.to_vec()))
            .unwrap();
        out
    }

    fn pictures(events: &[Event]) -> Vec<(u32, usize)> {
        events.iter().filter_map(|e| if let Event::Picture(p) = e { Some((p.frame, p.recovered)) } else { None }).collect()
    }

    #[test]
    fn a_picture_is_cut_into_datagram_sized_shards_with_a_fifth_more_in_parity() {
        let packetizer = Packetizer::new(DEFAULT_FEC_PERCENT);
        assert_eq!(MAX_SHARD, 1162);
        assert_eq!(SHARD_HEADER + MAX_SHARD + seal::OVERHEAD, MAX_DATAGRAM - 1);
        // a tiny picture (the screen did not change): one shard, two parity
        assert_eq!(packetizer.cut(40), Cut { data: 1, parity: 2, shard_bytes: 40 });
        assert_eq!(packetizer.cut(41), Cut { data: 1, parity: 2, shard_bytes: 42 });
        // 40 Mbps at 60 fps: about 83 KB, 72 shards, 15 parity, the padding under 2 bytes a shard
        let cut = packetizer.cut(83_333);
        assert_eq!((cut.data, cut.parity), (72, 15));
        assert!(cut.data * cut.shard_bytes - 83_333 < 2 * cut.data && cut.shard_bytes <= MAX_SHARD);
        // a 4K keyframe of 2 MB: one block, no Moonlight-style limit of 255 shards
        let big = packetizer.cut(2_000_000);
        assert_eq!((big.data, big.parity), (1722, 345));
    }

    #[test]
    fn whole_pictures_come_through_in_order_with_their_times() {
        let mut packetizer = Packetizer::new(DEFAULT_FEC_PERCENT);
        let mut assembler = Assembler::new();
        let now = Instant::now();
        let mut events = Vec::new();
        for (frame, bytes) in [(0u32, 200_000usize), (1, 3), (2, 1162 * 3 - PAYLOAD_HEADER), (3, 50_000)] {
            for shard in shards(&mut packetizer, frame, bytes, frame == 0) {
                assembler.push(&shard, now, &mut events);
            }
        }
        let got: Vec<&Assembled> = events.iter().map(|e| if let Event::Picture(p) = e { p } else { panic!("{e:?}") }).collect();
        assert_eq!(got.len(), 4);
        for (p, bytes) in got.iter().zip([200_000usize, 3, 1162 * 3 - PAYLOAD_HEADER, 50_000]) {
            assert_eq!(p.data, picture(p.frame, bytes));
            assert_eq!((p.host_us, p.encode_us, p.recovered), (4000 + p.frame, 2000, 0));
            assert_eq!(p.keyframe, p.frame == 0);
        }
        // the parity that was not needed arrives after its picture was done: counted, nothing else
        assert!(assembler.stats().late_shards > 0);
        assert_eq!(assembler.stats().pictures, 4);
    }

    #[test]
    fn up_to_parity_shards_may_be_lost_anywhere_in_a_picture() {
        let mut packetizer = Packetizer::new(DEFAULT_FEC_PERCENT);
        for (bytes, lose_every) in [(100usize, 3usize), (83_333, 6), (500_000, 6), (1_000, 2)] {
            let all = shards(&mut packetizer, 9, bytes, false);
            let cut = packetizer.cut(bytes + PAYLOAD_HEADER);
            // lose data and parity alike, no more than the parity can make up
            let mut kept: Vec<Vec<u8>> = Vec::new();
            let mut lost = 0;
            for (i, shard) in all.into_iter().enumerate() {
                if i % lose_every == 1 && lost < cut.parity {
                    lost += 1;
                } else {
                    kept.push(shard);
                }
            }
            kept.reverse(); // and in any order
            let mut assembler = Assembler::new();
            let mut events = Vec::new();
            for shard in &kept {
                assembler.push(shard, Instant::now(), &mut events);
            }
            let Some(Event::Picture(p)) = events.first() else { panic!("{bytes}: {events:?}") };
            assert_eq!(p.data, picture(9, bytes), "{bytes} bytes, {lost} shards lost");
            assert!(assembler.stats().recovered_shards > 0 || lost == 0);
        }
    }

    #[test]
    fn a_picture_missing_more_than_its_parity_is_given_up_when_the_next_one_begins() {
        let mut packetizer = Packetizer::new(DEFAULT_FEC_PERCENT);
        let mut assembler = Assembler::new();
        let now = Instant::now();
        let mut events = Vec::new();
        let first = shards(&mut packetizer, 10, 30_000, true);
        let cut = packetizer.cut(30_000 + PAYLOAD_HEADER);
        for shard in &first[cut.parity + 1..] {
            assembler.push(shard, now, &mut events);
        }
        assert!(events.is_empty(), "still hoping: {events:?}");
        for shard in shards(&mut packetizer, 11, 5_000, false) {
            assembler.push(&shard, now, &mut events);
        }
        assert_eq!(events[0], Event::Lost { first: 10, last: 10 });
        assert_eq!(pictures(&events), vec![(11, 0)]);
        // a late piece of 10 changes nothing
        assembler.push(&first[0], now, &mut events);
        assert_eq!(events.len(), 2);
        // 12 and 13 never arrive: 14 says so
        for shard in shards(&mut packetizer, 14, 5_000, false) {
            assembler.push(&shard, now, &mut events);
        }
        assert_eq!(events[2], Event::Lost { first: 12, last: 13 });
        assert_eq!(pictures(&events), vec![(11, 0), (14, 0)]);
        assert_eq!(assembler.stats().lost_pictures, 3);
    }

    #[test]
    fn a_picture_whose_shards_stop_coming_is_given_up_after_the_stall() {
        let mut packetizer = Packetizer::new(DEFAULT_FEC_PERCENT);
        let mut assembler = Assembler::new();
        let start = Instant::now();
        let mut events = Vec::new();
        let all = shards(&mut packetizer, 5, 20_000, false);
        for shard in &all[..3] {
            assembler.push(shard, start, &mut events);
        }
        assembler.tick(start + STALL / 2, &mut events);
        assert!(events.is_empty());
        assembler.tick(start + STALL, &mut events);
        assert_eq!(events, vec![Event::Lost { first: 5, last: 5 }]);
        // its other shards, should they come after all, are late
        assembler.push(&all[5], start + STALL * 2, &mut events);
        assert_eq!(events.len(), 1);
        assert_eq!(assembler.stats().late_shards, 1);
    }

    #[test]
    fn numbers_wrap_around_and_nonsense_is_counted_not_trusted() {
        let mut packetizer = Packetizer::new(10);
        let mut assembler = Assembler::new();
        let now = Instant::now();
        let mut events = Vec::new();
        for frame in [u32::MAX - 1, u32::MAX, 0, 1] {
            for shard in shards(&mut packetizer, frame, 2_000, false) {
                assembler.push(&shard, now, &mut events);
            }
        }
        assert_eq!(pictures(&events), vec![(u32::MAX - 1, 0), (u32::MAX, 0), (0, 0), (1, 0)]);
        let mut bad = shards(&mut packetizer, 2, 2_000, false)[0].clone();
        bad[9] = 0; // no data shards
        assembler.push(&bad, now, &mut events);
        assembler.push(&[1, 2, 3], now, &mut events);
        let mut odd = shards(&mut packetizer, 2, 2_000, false)[0].clone();
        odd.pop(); // an odd shard size
        assembler.push(&odd, now, &mut events);
        assert_eq!(assembler.stats().bad_shards, 3);
        assert_eq!(events.len(), 4);
    }
}
