//! The transport stream the engine writes when it encodes itself (docs/design.md §2.8): one HEVC video stream,
//! nothing else. Its only reader is the ffmpeg that adds the sound and copies the video into the stream viewers
//! get (`-f mpegts -i <pipe> … -c:v copy`), so it carries what ffmpeg's demuxer needs and no more: a PAT and a
//! PMT (at the start and again before every keyframe), one PES packet per picture with its presentation and
//! decode time, and a PCR on the video PID.

use super::EncodedPicture;

pub const PACKET_BYTES: usize = 188;
pub const PMT_PID: u16 = 0x1000;
pub const VIDEO_PID: u16 = 0x0100;
/// The 90 kHz clock of PTS and DTS.
pub const CLOCK_HZ: u64 = 90_000;
const PROGRAM_NUMBER: u16 = 1;
const STREAM_TYPE_HEVC: u8 = 0x24;
const STREAM_ID_VIDEO: u8 = 0xE0;
const PTS_MASK: u64 = (1 << 33) - 1;
const PAYLOAD_BYTES: usize = PACKET_BYTES - 4;

pub struct TsWriter {
    fps: u64,
    cc_pat: u8,
    cc_pmt: u8,
    cc_video: u8,
    tables_written: bool,
}

impl TsWriter {
    pub fn new(fps: u32) -> Self {
        Self {
            fps: u64::from(fps.max(1)),
            cc_pat: 0,
            cc_pmt: 0,
            cc_video: 0,
            tables_written: false,
        }
    }

    /// The 90 kHz time of frame `frames` of the stream. The timeline starts a second in, so that the decode
    /// times of the first pictures (earlier than their presentation times when there are B-frames) are not
    /// negative; ffmpeg moves the start to 0 anyway.
    pub fn ticks(&self, frames: i64) -> u64 {
        let n = (frames + self.fps as i64).max(0) as u64;
        ((n * CLOCK_HZ + self.fps / 2) / self.fps) & PTS_MASK
    }

    /// Appends the packets of one picture to `out`.
    pub fn write(&mut self, picture: &EncodedPicture, out: &mut Vec<u8>) {
        if picture.keyframe || !self.tables_written {
            self.write_tables(out);
        }
        let pts = self.ticks(picture.pts);
        let dts = self.ticks(picture.dts);
        let mut pes = Vec::with_capacity(picture.data.len() + 19);
        pes_header(&mut pes, pts, (dts != pts).then_some(dts), picture.data.len());
        pes.extend_from_slice(&picture.data);

        let mut rest = &pes[..];
        let mut first = true;
        while !rest.is_empty() {
            let mut fields = Vec::new();
            if first {
                // flags: PCR present, plus random access on a keyframe; then the PCR at the decode time
                fields.push(0x10 | if picture.keyframe { 0x40 } else { 0 });
                put_pcr(&mut fields, dts);
            }
            let adaptation = (!fields.is_empty()).then_some(fields.as_slice());
            let used = packet(&mut self.cc_video, VIDEO_PID, first, adaptation, rest, out);
            rest = &rest[used..];
            first = false;
        }
    }

    fn write_tables(&mut self, out: &mut Vec<u8>) {
        let mut pat = vec![0x00, 0xB0, 0, 0x00, 0x01, 0xC1, 0x00, 0x00];
        pat.extend_from_slice(&PROGRAM_NUMBER.to_be_bytes());
        pat.extend_from_slice(&(0xE000 | PMT_PID).to_be_bytes());
        section(&mut self.cc_pat, 0x0000, pat, out);

        let mut pmt = vec![0x02, 0xB0, 0];
        pmt.extend_from_slice(&PROGRAM_NUMBER.to_be_bytes());
        pmt.extend_from_slice(&[0xC1, 0x00, 0x00]);
        pmt.extend_from_slice(&(0xE000 | VIDEO_PID).to_be_bytes()); // PCR PID
        pmt.extend_from_slice(&[0xF0, 0x00]); // no program descriptors
        pmt.push(STREAM_TYPE_HEVC);
        pmt.extend_from_slice(&(0xE000 | VIDEO_PID).to_be_bytes());
        pmt.extend_from_slice(&[0xF0, 0x00]); // no stream descriptors
        section(&mut self.cc_pmt, PMT_PID, pmt, out);
        self.tables_written = true;
    }
}

/// The PES header of a video picture: data aligned, PTS and (when it differs) DTS. The length field is 0
/// ("unbounded", allowed for video) when the packet would not fit in it.
fn pes_header(out: &mut Vec<u8>, pts: u64, dts: Option<u64>, payload: usize) {
    let header_data = if dts.is_some() { 10 } else { 5 };
    let length = 3 + header_data + payload;
    let length = if length <= 0xFFFF { length as u16 } else { 0 };
    out.extend_from_slice(&[0x00, 0x00, 0x01, STREAM_ID_VIDEO]);
    out.extend_from_slice(&length.to_be_bytes());
    out.push(0x84); // '10', not scrambled, data_alignment_indicator
    out.push(if dts.is_some() { 0xC0 } else { 0x80 });
    out.push(header_data as u8);
    match dts {
        Some(dts) => {
            put_timestamp(out, 0x3, pts);
            put_timestamp(out, 0x1, dts);
        }
        None => put_timestamp(out, 0x2, pts),
    }
}

/// A 33-bit PTS / DTS in its five bytes, with the four-bit `prefix` and the marker bits.
fn put_timestamp(out: &mut Vec<u8>, prefix: u8, t: u64) {
    out.push(prefix << 4 | ((t >> 29) & 0x0E) as u8 | 1);
    out.push((t >> 22) as u8);
    out.push(((t >> 14) & 0xFE) as u8 | 1);
    out.push((t >> 7) as u8);
    out.push(((t << 1) & 0xFE) as u8 | 1);
}

/// A PCR whose base is `base` (90 kHz) and whose extension is 0.
fn put_pcr(out: &mut Vec<u8>, base: u64) {
    let base = base & PTS_MASK;
    out.extend_from_slice(&[
        (base >> 25) as u8,
        (base >> 17) as u8,
        (base >> 9) as u8,
        (base >> 1) as u8,
        ((base & 1) << 7) as u8 | 0x7E,
        0x00,
    ]);
}

/// Writes one packet of `pid` carrying as much of `payload` as fits and returns how much that was.
/// `adaptation`: the adaptation field's flags and fields (without its length byte); a packet that does not
/// fill up is padded with stuffing bytes in the adaptation field.
fn packet(cc: &mut u8, pid: u16, unit_start: bool, adaptation: Option<&[u8]>, payload: &[u8], out: &mut Vec<u8>) -> usize {
    let fixed = adaptation.map_or(0, |fields| 1 + fields.len());
    let take = payload.len().min(PAYLOAD_BYTES - fixed);
    let room = PAYLOAD_BYTES - take; // the adaptation field's whole length, its length byte included
    let start = out.len();
    out.resize(start + PACKET_BYTES, 0xFF);
    let p = &mut out[start..];
    p[0] = 0x47;
    p[1] = if unit_start { 0x40 } else { 0x00 } | (pid >> 8) as u8;
    p[2] = pid as u8;
    p[3] = if room == 0 { 0x10 } else { 0x30 } | *cc;
    *cc = (*cc + 1) & 0x0F;
    if room > 0 {
        p[4] = (room - 1) as u8;
        if room > 1 {
            match adaptation {
                Some(fields) => p[5..5 + fields.len()].copy_from_slice(fields),
                None => p[5] = 0x00,
            }
        }
    }
    p[4 + room..].copy_from_slice(&payload[..take]);
    take
}

/// Writes a PSI section (without its CRC; `section[1..3]` gets the length) in one packet of its own.
fn section(cc: &mut u8, pid: u16, mut section: Vec<u8>, out: &mut Vec<u8>) {
    let length = section.len() - 3 + 4;
    section[1] = 0xB0 | (length >> 8) as u8;
    section[2] = length as u8;
    let crc = crc32_mpeg2(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    let start = out.len();
    out.resize(start + PACKET_BYTES, 0xFF);
    let p = &mut out[start..];
    p[0] = 0x47;
    p[1] = 0x40 | (pid >> 8) as u8;
    p[2] = pid as u8;
    p[3] = 0x10 | *cc;
    *cc = (*cc + 1) & 0x0F;
    p[4] = 0x00; // pointer field
    p[5..5 + section.len()].copy_from_slice(&section);
}

/// CRC-32/MPEG-2: polynomial 0x04C11DB7, initial value all ones, no reflection, no final XOR.
pub fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(pts: i64, dts: i64, keyframe: bool, bytes: usize) -> EncodedPicture {
        EncodedPicture { data: (0..bytes).map(|i| i as u8).collect(), pts, dts, keyframe }
    }

    fn pid(p: &[u8]) -> u16 {
        u16::from(p[1] & 0x1F) << 8 | u16::from(p[2])
    }

    fn payload(p: &[u8]) -> &[u8] {
        match (p[3] >> 4) & 3 {
            1 => &p[4..],
            3 => &p[5 + usize::from(p[4])..],
            _ => &[],
        }
    }

    fn read_timestamp(b: &[u8]) -> u64 {
        (u64::from(b[0] & 0x0E) << 29) | (u64::from(b[1]) << 22) | (u64::from(b[2] & 0xFE) << 14) | (u64::from(b[3]) << 7) | (u64::from(b[4]) >> 1)
    }

    #[test]
    fn the_crc_is_mpeg2s() {
        // the check value of CRC-32/MPEG-2
        assert_eq!(crc32_mpeg2(b"123456789"), 0x0376_E6E7);
    }

    #[test]
    fn a_keyframe_comes_after_the_tables_and_its_pes_carries_pts_dts_and_a_pcr() {
        let mut ts = TsWriter::new(60);
        let mut out = Vec::new();
        ts.write(&picture(2, 0, true, 1000), &mut out);
        assert_eq!(out.len() % PACKET_BYTES, 0);
        let packets: Vec<&[u8]> = out.chunks(PACKET_BYTES).collect();
        assert!(packets.iter().all(|p| p[0] == 0x47));
        assert_eq!((pid(packets[0]), pid(packets[1]), pid(packets[2])), (0, PMT_PID, VIDEO_PID));
        // both tables check out
        for p in &packets[..2] {
            let section = &p[5..];
            let length = usize::from(section[1] & 0x0F) << 8 | usize::from(section[2]);
            assert_eq!(crc32_mpeg2(&section[..3 + length]), 0, "a section with its CRC sums to 0");
        }
        let pmt = &packets[1][5..];
        assert_eq!(pmt[0], 0x02);
        assert_eq!(pmt[12], STREAM_TYPE_HEVC);

        let first = packets[2];
        assert_eq!(first[1] & 0x40, 0x40, "unit start");
        assert_eq!(first[5] & 0x50, 0x50, "PCR and random access");
        let pes = payload(first);
        assert_eq!(&pes[..4], &[0, 0, 1, 0xE0]);
        assert_eq!(pes[7], 0xC0, "PTS and DTS");
        assert_eq!(read_timestamp(&pes[9..14]), ts.ticks(2));
        assert_eq!(read_timestamp(&pes[14..19]), ts.ticks(0));
        assert_eq!(ts.ticks(0), 90_000);
        assert_eq!(ts.ticks(2) - ts.ticks(0), 3000);
        // the payload comes back whole, in order
        let mut es: Vec<u8> = pes[19..].to_vec();
        for p in &packets[3..] {
            assert_eq!(pid(p), VIDEO_PID);
            assert_eq!(p[1] & 0x40, 0);
            es.extend_from_slice(payload(p));
        }
        assert_eq!(es, picture(0, 0, true, 1000).data);
        // the PES length counts the header's rest and the payload
        assert_eq!(usize::from(u16::from_be_bytes([pes[4], pes[5]])), 3 + 10 + 1000);
    }

    #[test]
    fn continuity_counters_count_per_pid_and_tables_come_again_only_before_keyframes() {
        let mut ts = TsWriter::new(60);
        let mut out = Vec::new();
        ts.write(&picture(0, -1, true, 500), &mut out);
        for i in 1..40 {
            ts.write(&picture(i, i - 1, false, 300 + i as usize * 37), &mut out);
        }
        ts.write(&picture(40, 39, true, 5000), &mut out);
        let mut last: [Option<u8>; 0x2000] = [None; 0x2000];
        let mut tables = 0;
        for p in out.chunks(PACKET_BYTES) {
            let pid = usize::from(pid(p));
            if pid == 0 {
                tables += 1;
            }
            let cc = p[3] & 0x0F;
            if let Some(previous) = last[pid] {
                assert_eq!(cc, (previous + 1) & 0x0F, "PID {pid:#x}");
            }
            last[pid] = Some(cc);
        }
        assert_eq!(tables, 2);
    }

    #[test]
    fn a_picture_of_exactly_one_packet_and_one_of_a_byte_more_both_fit() {
        for bytes in [184 - 8 - 19, 184 - 8 - 19 + 1, 184 - 8 - 19 - 1, 1] {
            let mut ts = TsWriter::new(30);
            let mut out = Vec::new();
            ts.write(&picture(5, 5, false, bytes), &mut out);
            let video: Vec<&[u8]> = out.chunks(PACKET_BYTES).filter(|p| pid(p) == VIDEO_PID).collect();
            let mut pes = Vec::new();
            for p in &video {
                pes.extend_from_slice(payload(p));
            }
            assert_eq!(pes[7], 0x80, "PTS only when the times are equal");
            assert_eq!(pes.len(), 14 + bytes, "{bytes} bytes");
        }
    }

    #[test]
    fn a_large_picture_has_an_unbounded_pes() {
        let mut ts = TsWriter::new(60);
        let mut out = Vec::new();
        ts.write(&picture(0, 0, true, 300_000), &mut out);
        let pes = out.chunks(PACKET_BYTES).find(|p| pid(p) == VIDEO_PID).map(payload).unwrap();
        assert_eq!(&pes[4..6], &[0, 0]);
    }
}
