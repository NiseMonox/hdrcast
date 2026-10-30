//! The video pipe in control mode (`--low-latency`, docs/design.md §11.2): HdrCast reads the pictures itself and
//! sends each one the moment it arrives, so the pipe carries one record per picture instead of an MPEG-TS for
//! ffmpeg. A record says where the picture ends (nothing waits for the next one to find that out) and how long the
//! engine held it.
//!
//! ```text
//!  0  4  n        bytes that follow (u32, little-endian)
//!  4  8  index    the picture's frame number (u64 LE)
//! 12  1  flags    1 keyframe, 2 a repeat of the previous picture (nothing new on the screen), 4 a keyframe asked for
//! 13  4  age      µs from the capture of the picture to its record (u32 LE)
//! 17  4  encode   µs the encoder took for it (u32 LE)
//! 21  ..          the picture: an HEVC access unit, Annex B, the parameter sets in front of every keyframe
//! ```

use std::io::{self, Read};

/// Bytes in front of the picture, the length included.
pub const HEADER_BYTES: usize = 21;
/// A record longer than this is taken for a broken pipe: no picture comes near it (a 4K keyframe is a few MB).
pub const MAX_RECORD_BYTES: usize = 64 << 20;

const KEYFRAME: u8 = 1;
const REPEAT: u8 = 2;
const ASKED: u8 = 4;

/// One picture as the engine hands it over in control mode.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrameRecord {
    pub index: u64,
    pub keyframe: bool,
    /// The screen had nothing new: the engine sent the previous picture again, so the viewer sees the stream go on.
    pub repeat: bool,
    /// A keyframe somebody asked for.
    pub asked: bool,
    /// From the capture of the picture to its record, in µs.
    pub age_us: u32,
    /// The encoder's part of that, in µs.
    pub encode_us: u32,
    pub data: Vec<u8>,
}

impl FrameRecord {
    /// Appends the record to `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        write_header(self.index, self.flags(), self.age_us, self.encode_us, self.data.len(), out);
        out.extend_from_slice(&self.data);
    }

    fn flags(&self) -> u8 {
        (if self.keyframe { KEYFRAME } else { 0 }) | (if self.repeat { REPEAT } else { 0 }) | (if self.asked { ASKED } else { 0 })
    }

    /// The next record from `reader`; `Ok(None)` when the stream ended cleanly between two records.
    pub fn read(reader: &mut impl Read) -> io::Result<Option<FrameRecord>> {
        let mut length = [0u8; 4];
        match read_full(reader, &mut length)? {
            0 => return Ok(None),
            4 => {}
            _ => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the pipe ended inside a picture's length")),
        }
        let n = u32::from_le_bytes(length) as usize;
        if !(HEADER_BYTES - 4..=MAX_RECORD_BYTES).contains(&n) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("a picture record of {n} bytes")));
        }
        let mut header = [0u8; HEADER_BYTES - 4];
        reader.read_exact(&mut header)?;
        let mut data = vec![0u8; n - header.len()];
        reader.read_exact(&mut data)?;
        let flags = header[8];
        Ok(Some(FrameRecord {
            index: u64::from_le_bytes(header[0..8].try_into().expect("8 bytes")),
            keyframe: flags & KEYFRAME != 0,
            repeat: flags & REPEAT != 0,
            asked: flags & ASKED != 0,
            age_us: u32::from_le_bytes(header[9..13].try_into().expect("4 bytes")),
            encode_us: u32::from_le_bytes(header[13..17].try_into().expect("4 bytes")),
            data,
        }))
    }
}

/// Appends a record's header for a picture of `picture_bytes` (the picture follows it).
pub(crate) fn write_header(index: u64, flags: u8, age_us: u32, encode_us: u32, picture_bytes: usize, out: &mut Vec<u8>) {
    out.extend_from_slice(&((HEADER_BYTES - 4 + picture_bytes) as u32).to_le_bytes());
    out.extend_from_slice(&index.to_le_bytes());
    out.push(flags);
    out.extend_from_slice(&age_us.to_le_bytes());
    out.extend_from_slice(&encode_us.to_le_bytes());
}

pub(crate) fn flags(keyframe: bool, repeat: bool, asked: bool) -> u8 {
    FrameRecord { keyframe, repeat, asked, ..FrameRecord::default() }.flags()
}

/// Fills `buf` unless the stream ends first; returns how much it read.
fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_come_back_as_they_went_and_the_stream_ends_cleanly_between_them() {
        let records = [
            FrameRecord { index: 0, keyframe: true, asked: false, repeat: false, age_us: 4_100, encode_us: 2_900, data: vec![0, 0, 1, 0x40, 1] },
            FrameRecord { index: 1, repeat: true, age_us: 104_000, encode_us: 300, data: vec![7; 3], ..FrameRecord::default() },
            FrameRecord { index: 1 << 40, keyframe: true, asked: true, data: vec![], ..FrameRecord::default() },
        ];
        let mut pipe = Vec::new();
        for record in &records {
            record.write(&mut pipe);
        }
        assert_eq!(pipe.len(), records.iter().map(|r| HEADER_BYTES + r.data.len()).sum::<usize>());
        let mut reader = &pipe[..];
        for record in &records {
            assert_eq!(FrameRecord::read(&mut reader).unwrap().as_ref(), Some(record));
        }
        assert_eq!(FrameRecord::read(&mut reader).unwrap(), None);
    }

    #[test]
    fn a_cut_or_absurd_record_is_an_error() {
        let mut pipe = Vec::new();
        FrameRecord { index: 3, data: vec![1; 100], ..FrameRecord::default() }.write(&mut pipe);
        for cut in [2, 4, 10, HEADER_BYTES, pipe.len() - 1] {
            assert!(FrameRecord::read(&mut &pipe[..cut]).is_err(), "cut at {cut}");
        }
        let mut huge = ((MAX_RECORD_BYTES + 1) as u32).to_le_bytes().to_vec();
        huge.extend_from_slice(&[0; 20]);
        assert_eq!(FrameRecord::read(&mut &huge[..]).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(FrameRecord::read(&mut &[3u8, 0, 0, 0][..]).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
