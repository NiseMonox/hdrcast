//! Keeps pictures that transport loss damaged off the screen (docs/design.md §3.2).
//!
//! SRT retransmits what the network loses, but only until a datagram's play time: what it could not recover by
//! then it skips, and seven TS packets of the stream are simply gone. An HEVC decoder does not notice: it decodes
//! the broken picture without an error, and every picture after it that refers to it, up to the next keyframe,
//! comes out smeared and blocky. mpv shows all of that.
//!
//! The guard sits between the SRT receiver (ffmpeg, handing over the raw payload) and mpv. It finds the gaps from
//! the transport stream's continuity counters and the decode timestamps, and keeps the damage off the screen the
//! way Moonlight does: the picture holds on the last good frame until the next keyframe, then goes on. It does
//! not take the damaged pictures out of the stream (mpv would find its video queue empty and pause the sound to
//! buffer) but relabels their slices as RASL pictures, which a decoder skips by definition after an IDR or BLA
//! picture. The timestamps stay, so mpv sees an ordinary stream in which some frames produce nothing.
//!
//! Per access unit (one PES packet of the video stream, held until the next one starts):
//! * intact, while all is well: passed on unchanged;
//! * damaged, or after a damaged reference picture, or after whole pictures went missing: its slices become
//!   RASL_N and the NAL units after the gap are cut off (their start codes are broken), so the decoder parses
//!   one intact slice header and skips the picture;
//! * damaged with no intact slice header, or a damaged keyframe: left out (a one-picture gap is harmless);
//! * the first intact keyframe ends the freeze. A CRA there becomes a BLA so that its leading pictures, which
//!   refer to what was lost, are skipped too. Before the first keyframe of a stream nothing is passed on.
//!
//! The rest of the stream (audio, tables) passes through untouched. Only HEVC is guarded; another video codec
//! passes through as it came.

use std::fmt;

/// Bytes in a transport stream packet.
pub const PACKET_BYTES: usize = 188;
const SYNC: u8 = 0x47;
const PAT_PID: u16 = 0x0000;
const NULL_PID: u16 = 0x1FFF;
/// MPEG-TS stream_type of HEVC video.
const STREAM_TYPE_HEVC: u8 = 0x24;
/// The other video stream types a PMT may name (MPEG-1/2, MPEG-4 part 2, H.264, AVS2, VVC): passed through.
const OTHER_VIDEO_TYPES: [u8; 6] = [0x01, 0x02, 0x10, 0x1B, 0x33, 0x42];

/// HEVC NAL unit types (H.265 table 7-1) the guard looks at.
const NAL_RASL_N: u8 = 8;
const NAL_BLA_W_LP: u8 = 16;
const NAL_CRA: u8 = 21;
/// The IRAP types (BLA, IDR, CRA and the two reserved ones); everything below 32 is a slice (VCL).
const NAL_IRAP_FIRST: u8 = 16;
const NAL_IRAP_LAST: u8 = 23;
const NAL_VCL_END: u8 = 32;

/// Bytes of a slice that must have arrived before a gap for its header to be trusted (an NVENC slice header is
/// a few dozen bytes; the margin keeps a header that runs into the gap from being parsed).
const SLICE_HEADER_MARGIN: usize = 128;
/// 90 kHz ticks: a decode timestamp further than this from the previous one starts a new timeline (an encoder
/// restart), it does not mean lost pictures.
const TIMELINE_JUMP: u64 = 10 * 90_000;
const PTS_MASK: u64 = (1 << 33) - 1;

/// What the guard has done so far. Counts are pictures (access units) unless said otherwise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GuardStats {
    /// Places where the transport lost part of the video stream (continuity gaps).
    pub losses: u64,
    /// Pictures that arrived damaged.
    pub damaged: u64,
    /// Pictures the decoder was told to skip: damaged ones, and those that depend on one.
    pub hidden: u64,
    /// Damaged pictures left out of the stream altogether.
    pub dropped: u64,
    /// Pictures before the stream's first keyframe (a viewer joins between two): left out, nothing to show yet.
    pub before_keyframe: u64,
    /// Pictures passed on as they came.
    pub shown: u64,
    /// Times the picture froze because a reference picture was lost, and how many of those a keyframe ended.
    pub freezes: u64,
    pub recoveries: u64,
    /// The picture is frozen right now (waiting for a keyframe).
    pub frozen: bool,
    /// The video stream is HEVC and being guarded (false before the stream's tables were seen).
    pub guarding: bool,
}

impl fmt::Display for GuardStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} losses, {} damaged, {} hidden, {} left out, {} shown, {} freezes",
            self.losses, self.damaged, self.hidden, self.dropped, self.shown, self.freezes
        )
    }
}

/// Something worth a line in the log, returned by [`TsGuard::take_events`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardEvent {
    /// A reference picture was lost: the picture holds until the next keyframe.
    Froze,
    /// A keyframe ended the freeze. `skipped`: pictures not shown meanwhile; `ticks`: how long that was in
    /// 90 kHz ticks of the stream.
    Recovered { skipped: u64, ticks: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// No keyframe yet: nothing is passed on.
    Start,
    /// Pictures are passed on. `open_gop`: the last keyframe was a CRA the decoder treats as mid-stream, so it
    /// decodes RASL pictures and relabelling hides nothing; damaged pictures are left out instead.
    Pass { open_gop: bool },
    /// A reference picture was lost: everything up to the next keyframe is kept off the screen.
    Skip { open_gop: bool, since_dts: Option<u64>, skipped: u64 },
}

/// One video PES packet being collected.
struct AccessUnit {
    packets: Vec<[u8; PACKET_BYTES]>,
    /// ES bytes collected so far.
    es_len: usize,
    /// ES offset of the first gap; `None` while intact.
    gap_at: Option<usize>,
    /// The first packet carries a usable PES header.
    pes_ok: bool,
    dts: Option<u64>,
}

/// Where a NAL unit starts inside an access unit's packets.
#[derive(Debug, Clone, Copy)]
struct NalStart {
    /// ES offset of the NAL header's first byte.
    es_offset: usize,
    /// (packet, byte) of the last byte of the start code (the 0x01).
    code_at: (usize, usize),
    /// (packet, byte) of the NAL header's first byte; `None` when the access unit ends right after the start code.
    header_at: Option<(usize, usize)>,
    nal_type: u8,
}

pub struct TsGuard {
    /// Bytes of a packet not complete yet.
    pending: Vec<u8>,
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    video_is_hevc: bool,
    /// Last continuity counter per PID (index = PID); 0xFF = none yet.
    last_cc: Vec<u8>,
    au: Option<AccessUnit>,
    state: State,
    out_cc: Option<u8>,
    last_dts: Option<u64>,
    frame_ticks: Option<u64>,
    stats: GuardStats,
    events: Vec<GuardEvent>,
}

impl Default for TsGuard {
    fn default() -> Self {
        TsGuard::new()
    }
}

impl TsGuard {
    pub fn new() -> TsGuard {
        TsGuard {
            pending: Vec::with_capacity(2 * PACKET_BYTES),
            pmt_pid: None,
            video_pid: None,
            video_is_hevc: false,
            last_cc: vec![0xFF; 8192],
            au: None,
            state: State::Start,
            out_cc: None,
            last_dts: None,
            frame_ticks: None,
            stats: GuardStats::default(),
            events: Vec::new(),
        }
    }

    pub fn stats(&self) -> GuardStats {
        GuardStats {
            frozen: matches!(self.state, State::Skip { .. }),
            guarding: self.video_pid.is_some() && self.video_is_hevc,
            ..self.stats
        }
    }

    /// What happened since the last call (freezes and recoveries), oldest first.
    pub fn take_events(&mut self) -> Vec<GuardEvent> {
        std::mem::take(&mut self.events)
    }

    /// Takes the next bytes of the stream (in any chunks) and appends what may go on to `out`.
    pub fn push(&mut self, input: &[u8], out: &mut Vec<u8>) {
        self.pending.extend_from_slice(input);
        let mut at = 0;
        while self.pending.len() - at >= PACKET_BYTES {
            if self.pending[at] != SYNC {
                // out of step (not something SRT does, whose datagrams are whole packets): find the next sync byte
                match self.pending[at..].iter().position(|&b| b == SYNC) {
                    Some(skip) => at += skip,
                    None => at = self.pending.len(),
                }
                continue;
            }
            let mut packet = [0u8; PACKET_BYTES];
            packet.copy_from_slice(&self.pending[at..at + PACKET_BYTES]);
            at += PACKET_BYTES;
            self.packet(packet, out);
        }
        self.pending.drain(..at);
    }

    /// The stream ended: the picture still being held may be incomplete, so it is let go without being shown.
    pub fn finish(&mut self) {
        self.au = None;
        self.pending.clear();
    }

    fn packet(&mut self, mut p: [u8; PACKET_BYTES], out: &mut Vec<u8>) {
        let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
        if pid == NULL_PID {
            return;
        }
        let pusi = p[1] & 0x40 != 0;
        let afc = (p[3] >> 4) & 3;
        let discontinuity = afc & 2 != 0 && p[4] > 0 && p[5] & 0x80 != 0;
        let mut gap = p[1] & 0x80 != 0; // transport_error_indicator
        if afc & 1 != 0 {
            let cc = p[3] & 0x0F;
            let last = self.last_cc[usize::from(pid)];
            if last != 0xFF && !discontinuity {
                if cc == last {
                    return; // a duplicate (the standard allows one): the first copy went on already
                }
                gap |= cc != (last + 1) & 0x0F;
            }
            self.last_cc[usize::from(pid)] = cc;
        }

        if pid == PAT_PID {
            if pusi {
                self.read_pat(&p);
            }
            out.extend_from_slice(&p);
        } else if Some(pid) == self.pmt_pid {
            if pusi {
                self.read_pmt(&p);
            }
            out.extend_from_slice(&p);
        } else if Some(pid) == self.video_pid {
            if self.video_is_hevc {
                self.video_packet(&mut p, pusi, gap, out);
            } else {
                out.extend_from_slice(&p);
            }
        } else if self.video_pid.is_some() || pid < 0x20 {
            // audio and the other tables; before the program is known only the service tables go on
            out.extend_from_slice(&p);
        }
    }

    fn read_pat(&mut self, p: &[u8; PACKET_BYTES]) {
        let Some(section) = psi_section(p, 0x00) else { return };
        // program loop after the 8 header bytes: program_number(16) reserved(3) PID(13)
        for entry in section.get(8..).unwrap_or_default().chunks_exact(4) {
            let program = (u16::from(entry[0]) << 8) | u16::from(entry[1]);
            if program != 0 {
                self.pmt_pid = Some((u16::from(entry[2] & 0x1F) << 8) | u16::from(entry[3]));
                return;
            }
        }
    }

    fn read_pmt(&mut self, p: &[u8; PACKET_BYTES]) {
        let Some(section) = psi_section(p, 0x02) else { return };
        if section.len() < 12 {
            return;
        }
        let program_info = usize::from(section[10] & 0x0F) << 8 | usize::from(section[11]);
        let mut i = 12 + program_info;
        let mut video: Option<(u16, bool)> = None;
        while i + 5 <= section.len() {
            let stream_type = section[i];
            let pid = (u16::from(section[i + 1] & 0x1F) << 8) | u16::from(section[i + 2]);
            let info = usize::from(section[i + 3] & 0x0F) << 8 | usize::from(section[i + 4]);
            if stream_type == STREAM_TYPE_HEVC {
                video = Some((pid, true));
                break;
            }
            if video.is_none() && OTHER_VIDEO_TYPES.contains(&stream_type) {
                video = Some((pid, false));
            }
            i += 5 + info;
        }
        let Some((pid, hevc)) = video else { return };
        if self.video_pid != Some(pid) || self.video_is_hevc != hevc {
            // a different program: what was being held belonged to the old one
            self.au = None;
            self.video_pid = Some(pid);
            self.video_is_hevc = hevc;
            self.state = State::Start;
            self.last_dts = None;
            self.frame_ticks = None;
            self.out_cc = None;
        }
    }

    fn video_packet(&mut self, p: &mut [u8; PACKET_BYTES], pusi: bool, gap: bool, out: &mut Vec<u8>) {
        if gap {
            self.stats.losses += 1;
            // Also when this packet starts the next picture: then the lost data was the end of this one, or whole
            // pictures in between (the decode timestamps tell). Without a picture in hand (the stream's first
            // packets) there is nothing to mark: nothing is shown before a keyframe anyway.
            if let Some(au) = &mut self.au {
                au.gap_at.get_or_insert(au.es_len);
            }
        }
        if pusi {
            self.finish_au(out);
            self.au = Some(start_au(p));
        } else if let Some(au) = &mut self.au {
            au.es_len += payload(p).len();
            au.packets.push(*p);
        }
    }

    fn finish_au(&mut self, out: &mut Vec<u8>) {
        let Some(mut au) = self.au.take() else { return };
        let nals = if au.pes_ok { scan_nals(&au) } else { Vec::new() };
        let slices = || nals.iter().filter(|n| n.nal_type < NAL_VCL_END);
        let irap = slices().any(|n| (NAL_IRAP_FIRST..=NAL_IRAP_LAST).contains(&n.nal_type));
        let cra = slices().any(|n| n.nal_type == NAL_CRA);
        // sub-layer reference pictures have the odd types below 16 (TRAIL_R, TSA_R, STSA_R, RADL_R, RASL_R, ...)
        let reference = irap || slices().any(|n| n.nal_type < NAL_IRAP_FIRST && n.nal_type % 2 == 1);
        let damaged = au.gap_at.is_some();
        let intact_keyframe = irap && !damaged && au.pes_ok;
        let missing = self.pictures_missing(au.dts);
        if damaged {
            self.stats.damaged += 1;
        }

        match self.state {
            State::Start if intact_keyframe => {
                self.state = State::Pass { open_gop: false };
                self.emit(&mut au, &nals, true, out);
            }
            State::Start => self.stats.before_keyframe += 1,
            State::Pass { .. } if missing && intact_keyframe => {
                // pictures went missing just before a keyframe: it starts afresh, nothing to hide
                self.state = State::Pass { open_gop: false };
                self.emit(&mut au, &nals, true, out);
            }
            State::Pass { open_gop } if missing || (damaged && reference) => {
                // a lost picture may have been a reference: freeze until the next keyframe
                self.stats.freezes += 1;
                self.events.push(GuardEvent::Froze);
                self.state = State::Skip { open_gop, since_dts: au.dts, skipped: 0 };
                self.hide(&mut au, &nals, irap, out);
            }
            State::Pass { .. } if damaged => {
                // a damaged picture nothing refers to: it is the only one kept off the screen
                self.hide(&mut au, &nals, irap, out);
            }
            State::Pass { .. } => {
                if irap {
                    self.state = State::Pass { open_gop: cra };
                }
                self.emit(&mut au, &nals, false, out);
            }
            State::Skip { since_dts, skipped, .. } if intact_keyframe => {
                let ticks = match (since_dts, au.dts) {
                    (Some(from), Some(to)) => to.wrapping_sub(from) & PTS_MASK,
                    _ => 0,
                };
                self.stats.recoveries += 1;
                self.events.push(GuardEvent::Recovered { skipped, ticks });
                self.state = State::Pass { open_gop: false };
                self.emit(&mut au, &nals, true, out);
            }
            State::Skip { .. } => self.hide(&mut au, &nals, irap, out),
        }
    }

    /// Learns the frame duration from the decode timestamps and says whether pictures are missing before the
    /// one with `dts`.
    fn pictures_missing(&mut self, dts: Option<u64>) -> bool {
        let Some(dts) = dts else { return false };
        let Some(previous) = self.last_dts.replace(dts) else { return false };
        let delta = dts.wrapping_sub(previous) & PTS_MASK;
        if delta == 0 || delta > TIMELINE_JUMP {
            // a new timeline, or a broken timestamp: learn again
            self.frame_ticks = None;
            return false;
        }
        match self.frame_ticks {
            Some(frame) if delta > frame + frame / 2 => true,
            Some(frame) => {
                self.frame_ticks = Some(frame.min(delta));
                false
            }
            None => {
                self.frame_ticks = Some(delta);
                false
            }
        }
    }

    /// Passes a picture on; `resume`: it starts the stream or ends a freeze, so a CRA becomes a BLA.
    fn emit(&mut self, au: &mut AccessUnit, nals: &[NalStart], resume: bool, out: &mut Vec<u8>) {
        if resume {
            for nal in nals.iter().filter(|n| n.nal_type == NAL_CRA) {
                set_nal_type(au, nal, NAL_BLA_W_LP);
            }
        }
        self.stats.shown += 1;
        self.write(au, out);
    }

    /// Makes the decoder skip a picture, or leaves it out when that cannot be done safely.
    fn hide(&mut self, au: &mut AccessUnit, nals: &[NalStart], irap: bool, out: &mut Vec<u8>) {
        let open_gop = match &mut self.state {
            State::Skip { open_gop, skipped, .. } => {
                *skipped += 1;
                *open_gop
            }
            State::Pass { open_gop } => *open_gop,
            State::Start => false,
        };
        let gap = au.gap_at.unwrap_or(usize::MAX);
        let first_slice = nals.iter().find(|n| n.nal_type < NAL_VCL_END);
        let header_intact =
            first_slice.is_some_and(|n| n.header_at.is_some() && n.es_offset + SLICE_HEADER_MARGIN <= gap);
        if !au.pes_ok || irap || open_gop || !header_intact {
            self.stats.dropped += 1;
            return;
        }
        for nal in nals {
            if nal.es_offset < gap {
                if nal.nal_type < NAL_VCL_END {
                    set_nal_type(au, nal, NAL_RASL_N);
                }
            } else {
                // after the gap: whatever this was, it is cut off; its bytes stay in the slice before, unread
                let (k, i) = nal.code_at;
                au.packets[k][i] = 0x03;
            }
        }
        self.stats.hidden += 1;
        self.write(au, out);
    }

    /// Writes a picture's packets, the video stream's continuity counter renumbered over what was lost or left out.
    fn write(&mut self, au: &mut AccessUnit, out: &mut Vec<u8>) {
        for p in &mut au.packets {
            if p[3] & 0x10 != 0 {
                let cc = match self.out_cc {
                    None => p[3] & 0x0F,
                    Some(last) => (last + 1) & 0x0F,
                };
                self.out_cc = Some(cc);
                p[3] = (p[3] & 0xF0) | cc;
            }
            out.extend_from_slice(p);
        }
    }
}

fn start_au(p: &[u8; PACKET_BYTES]) -> AccessUnit {
    let payload = payload(p);
    let mut au = AccessUnit { packets: vec![*p], es_len: 0, gap_at: None, pes_ok: false, dts: None };
    // PES header: 00 00 01, stream_id, length(16), flags(16), header_data_length(8), then PTS / DTS
    if payload.len() >= 9 && payload[..3] == [0, 0, 1] && (0xE0..=0xEF).contains(&payload[3]) {
        let header = 9 + usize::from(payload[8]);
        if header <= payload.len() {
            au.pes_ok = true;
            au.es_len = payload.len() - header;
            let flags = payload[7] >> 6;
            let pts = (flags & 2 != 0 && payload[8] >= 5).then(|| timestamp(&payload[9..14]));
            let dts = (flags == 3 && payload[8] >= 10).then(|| timestamp(&payload[14..19]));
            au.dts = dts.or(pts);
        }
    }
    au
}

/// A 33-bit PES timestamp from its 5 bytes.
fn timestamp(b: &[u8]) -> u64 {
    (u64::from(b[0] >> 1) & 0x07) << 30
        | u64::from(b[1]) << 22
        | u64::from(b[2] >> 1) << 15
        | u64::from(b[3]) << 7
        | u64::from(b[4] >> 1)
}

/// Where a packet's payload starts (after the adaptation field); [`PACKET_BYTES`] when it has none.
fn payload_start(p: &[u8; PACKET_BYTES]) -> usize {
    let afc = (p[3] >> 4) & 3;
    if afc & 1 == 0 {
        return PACKET_BYTES;
    }
    let start = if afc & 2 != 0 { 5 + usize::from(p[4]) } else { 4 };
    start.min(PACKET_BYTES)
}

fn payload(p: &[u8; PACKET_BYTES]) -> &[u8] {
    &p[payload_start(p)..]
}

/// Every Annex B start code in the access unit's ES bytes, with where its NAL header is.
fn scan_nals(au: &AccessUnit) -> Vec<NalStart> {
    let mut nals = Vec::new();
    let mut zeros = 0u32;
    let mut es_offset = 0usize;
    let mut expecting_header: Option<(usize, (usize, usize))> = None;
    for (k, p) in au.packets.iter().enumerate() {
        let mut start = payload_start(p);
        if k == 0 {
            // skip the PES header (start_au checked that it fits)
            start += 9 + usize::from(p[start + 8]);
        }
        for (i, &b) in p.iter().enumerate().skip(start) {
            if let Some((offset, code_at)) = expecting_header.take() {
                nals.push(NalStart { es_offset: offset, code_at, header_at: Some((k, i)), nal_type: (b >> 1) & 0x3F });
            }
            if b == 1 && zeros >= 2 {
                expecting_header = Some((es_offset + 1, (k, i)));
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            es_offset += 1;
        }
    }
    if let Some((offset, code_at)) = expecting_header {
        nals.push(NalStart { es_offset: offset, code_at, header_at: None, nal_type: 0xFF });
    }
    nals
}

fn set_nal_type(au: &mut AccessUnit, nal: &NalStart, nal_type: u8) {
    if let Some((k, i)) = nal.header_at {
        let b = &mut au.packets[k][i];
        *b = (*b & 0x81) | (nal_type << 1);
    }
}

/// The section of a PSI packet with `table_id`, without its CRC (only sections that fit one packet: that is how
/// muxers write PAT and PMT for a single program).
fn psi_section(p: &[u8; PACKET_BYTES], table_id: u8) -> Option<&[u8]> {
    let payload = payload(p);
    let pointer = usize::from(*payload.first()?);
    let section = payload.get(1 + pointer..)?;
    if section.len() < 3 || section[0] != table_id {
        return None;
    }
    let length = usize::from(section[1] & 0x0F) << 8 | usize::from(section[2]);
    section.get(..(3 + length).checked_sub(4)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PMT_PID: u16 = 0x1000;
    const VIDEO: u16 = 0x100;
    const AUDIO: u16 = 0x101;
    const FRAME: u64 = 1500; // 60 fps in 90 kHz ticks
    const NAL_TRAIL_N: u8 = 0;
    const NAL_TRAIL_R: u8 = 1;
    const NAL_IDR_W_RADL: u8 = 19;

    fn pid_of(p: &[u8]) -> u16 {
        (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2])
    }

    /// Builds transport streams the way ffmpeg's muxer lays them out.
    struct Mux {
        cc: std::collections::HashMap<u16, u8>,
        packets: Vec<[u8; PACKET_BYTES]>,
    }

    impl Mux {
        fn new() -> Mux {
            let mut mux = Mux { cc: Default::default(), packets: Vec::new() };
            mux.tables();
            mux
        }

        fn next_cc(&mut self, pid: u16) -> u8 {
            let cc = self.cc.entry(pid).or_insert(15);
            *cc = (*cc + 1) & 15;
            *cc
        }

        fn section(&mut self, pid: u16, body: &[u8]) {
            let mut p = [0xFFu8; PACKET_BYTES];
            p[0] = SYNC;
            p[1] = 0x40 | (pid >> 8) as u8;
            p[2] = pid as u8;
            p[3] = 0x10 | self.next_cc(pid);
            p[4] = 0; // pointer field
            p[5..5 + body.len()].copy_from_slice(body);
            self.packets.push(p);
        }

        fn tables(&mut self) {
            // PAT: section length 13, program 1 on the PMT PID, a (dummy) CRC
            let (pmt_hi, pmt_lo) = (0xE0 | (PMT_PID >> 8) as u8, PMT_PID as u8);
            self.section(PAT_PID, &[0x00, 0xB0, 13, 0, 1, 0xC1, 0, 0, 0, 1, pmt_hi, pmt_lo, 0, 0, 0, 0]);
            // PMT: program 1, PCR on the video PID, HEVC video and AAC audio
            let (v_hi, v_lo) = (0xE0 | (VIDEO >> 8) as u8, VIDEO as u8);
            let (a_hi, a_lo) = (0xE0 | (AUDIO >> 8) as u8, AUDIO as u8);
            #[rustfmt::skip]
            let pmt = [
                0x02, 0xB0, 23, 0, 1, 0xC1, 0, 0, v_hi, v_lo, 0xF0, 0,
                STREAM_TYPE_HEVC, v_hi, v_lo, 0xF0, 0,
                0x0F, a_hi, a_lo, 0xF0, 0,
                0, 0, 0, 0,
            ];
            self.section(PMT_PID, &pmt);
        }

        /// One PES packet: `es` split over as many packets as it takes, the last one padded with stuffing.
        fn pes(&mut self, pid: u16, dts: u64, es: &[u8]) {
            let mut data = vec![0, 0, 1, if pid == VIDEO { 0xE0 } else { 0xC0 }, 0, 0, 0x80, 0xC0, 10];
            data.extend_from_slice(&ts_bytes(3, dts + 2 * FRAME));
            data.extend_from_slice(&ts_bytes(1, dts));
            data.extend_from_slice(es);
            for (n, chunk) in data.chunks(184).enumerate() {
                let mut p = [0u8; PACKET_BYTES];
                p[0] = SYNC;
                p[1] = if n == 0 { 0x40 } else { 0 } | (pid >> 8) as u8;
                p[2] = pid as u8;
                let cc = self.next_cc(pid);
                if chunk.len() == 184 {
                    p[3] = 0x10 | cc;
                    p[4..].copy_from_slice(chunk);
                } else {
                    let stuffing = 184 - chunk.len() - 1;
                    p[3] = 0x30 | cc;
                    p[4] = stuffing as u8;
                    if stuffing > 0 {
                        p[5] = 0;
                        p[6..5 + stuffing].fill(0xFF);
                    }
                    p[5 + stuffing..].copy_from_slice(chunk);
                }
                self.packets.push(p);
            }
        }

        /// A picture like NVENC writes it: AUD, a prefix SEI, the parameter sets before a keyframe, one slice
        /// with `size` bytes of slice data.
        fn picture(&mut self, n: u64, nal_type: u8, size: usize) {
            let mut es = vec![0, 0, 0, 1, 0x46, 0x01, 0x50, 0, 0, 1, 0x4E, 0x01, 0x05, 0x02, 0xAA, 0xBB, 0x80];
            if (NAL_IRAP_FIRST..=NAL_IRAP_LAST).contains(&nal_type) {
                for (t, len) in [(32u8, 20usize), (33, 40), (34, 8)] {
                    es.extend_from_slice(&[0, 0, 1, t << 1, 1]);
                    es.extend(std::iter::repeat_n(0x5A, len));
                }
            }
            es.extend_from_slice(&[0, 0, 0, 1, nal_type << 1, 1]);
            // slice data: never two zero bytes in a row, so no start code is emulated
            es.extend((0..size).map(|i| (i % 251) as u8 | 1));
            self.pes(VIDEO, n * FRAME, &es);
        }

        fn audio(&mut self, n: u64) {
            self.pes(AUDIO, n * FRAME, &[0xFF, 0xF1, 0x50, 0x80, 0x10, 0x1F, 0xFC, 0x21, 0x10]);
        }
    }

    fn ts_bytes(prefix: u8, t: u64) -> [u8; 5] {
        [
            prefix << 4 | ((t >> 29) & 0x0E) as u8 | 1,
            (t >> 22) as u8,
            ((t >> 14) & 0xFE) as u8 | 1,
            (t >> 7) as u8,
            ((t << 1) & 0xFE) as u8 | 1,
        ]
    }

    /// `pictures` pictures with a keyframe every `gop`, P B B like NVENC with -bf 2 -b_ref_mode middle
    /// (TRAIL_R, TRAIL_R, TRAIL_N), and one more keyframe that only ends the last picture (the guard holds each
    /// picture until the next one starts). Returns the packets and where each picture's video packets are.
    fn stream(pictures: u64, gop: u64) -> (Vec<[u8; PACKET_BYTES]>, Vec<(usize, usize)>) {
        let mut mux = Mux::new();
        let mut spans = Vec::new();
        for n in 0..=pictures {
            if n % 20 == 0 {
                mux.tables();
            }
            mux.audio(n);
            let from = mux.packets.len();
            let nal_type = match n {
                n if n % gop == 0 || n == pictures => NAL_IDR_W_RADL,
                n if n % 3 == 0 => NAL_TRAIL_N,
                _ => NAL_TRAIL_R,
            };
            mux.picture(n, nal_type, if nal_type == NAL_IDR_W_RADL { 3000 } else { 1500 });
            spans.push((from, mux.packets.len()));
        }
        spans.pop();
        (mux.packets, spans)
    }

    /// What reaches the decoder: (picture number, type of its first slice) for every picture in `bytes`.
    fn decoded(bytes: &[u8]) -> Vec<(u64, u8)> {
        let mut pictures = Vec::new();
        let mut current: Option<(u64, Vec<u8>)> = None;
        let mut flush = |current: &mut Option<(u64, Vec<u8>)>| {
            if let Some((dts, es)) = current.take() {
                let first_slice = es.windows(4).filter(|w| w[..3] == [0, 0, 1]).map(|w| (w[3] >> 1) & 0x3F).find(|t| *t < NAL_VCL_END);
                if let Some(t) = first_slice {
                    pictures.push((dts / FRAME, t));
                }
            }
        };
        let mut last_cc: Option<u8> = None;
        for p in bytes.chunks(PACKET_BYTES) {
            let p: &[u8; PACKET_BYTES] = p.try_into().unwrap();
            if pid_of(p) != VIDEO {
                continue;
            }
            let cc = p[3] & 15;
            if let Some(last) = last_cc {
                assert_eq!(cc, (last + 1) & 15, "the output's continuity counter has a gap");
            }
            last_cc = Some(cc);
            if p[1] & 0x40 != 0 {
                flush(&mut current);
                let au = start_au(p);
                let payload = payload(p);
                current = Some((au.dts.unwrap(), payload[9 + usize::from(payload[8])..].to_vec()));
            } else if let Some((_, es)) = &mut current {
                es.extend_from_slice(payload(p));
            }
        }
        flush(&mut current);
        pictures
    }

    fn run(packets: &[[u8; PACKET_BYTES]]) -> (Vec<u8>, TsGuard) {
        let mut guard = TsGuard::new();
        let mut out = Vec::new();
        // packets arrive split anywhere
        let bytes: Vec<u8> = packets.iter().flatten().copied().collect();
        for chunk in bytes.chunks(1000) {
            guard.push(chunk, &mut out);
        }
        guard.finish();
        (out, guard)
    }

    fn without(packets: &[[u8; PACKET_BYTES]], lost: std::ops::Range<usize>) -> Vec<[u8; PACKET_BYTES]> {
        packets.iter().enumerate().filter(|(i, _)| !lost.contains(i)).map(|(_, p)| *p).collect()
    }

    fn hidden(pictures: &[(u64, u8)]) -> Vec<u64> {
        pictures.iter().filter(|(_, t)| *t == NAL_RASL_N).map(|(n, _)| *n).collect()
    }

    fn flat(packets: &[[u8; PACKET_BYTES]]) -> Vec<u8> {
        packets.iter().flatten().copied().collect()
    }

    /// The packets of `bytes` that are (or are not) video.
    fn video_or_not(bytes: &[u8], video: bool) -> Vec<&[u8]> {
        bytes.chunks(PACKET_BYTES).filter(|p| (pid_of(p) == VIDEO) == video).collect()
    }

    #[test]
    fn an_intact_stream_passes_unchanged() {
        let (packets, spans) = stream(130, 60);
        let (out, guard) = run(&packets);
        let pictures = decoded(&out);
        assert_eq!(pictures.len(), 130);
        assert!(hidden(&pictures).is_empty());
        let s = guard.stats();
        assert_eq!((s.losses, s.damaged, s.hidden, s.dropped, s.shown, s.freezes), (0, 0, 0, 0, 130, 0));
        assert!(s.guarding && !s.frozen);
        // every packet but the closing picture's, the video ones byte for byte (only their order against the
        // audio may change: a picture is held until the next one starts)
        let last = spans.last().unwrap().1;
        assert_eq!(video_or_not(&out, true), video_or_not(&flat(&packets[..last]), true));
        assert_eq!(video_or_not(&out, false).len(), video_or_not(&flat(&packets), false).len());
    }

    #[test]
    fn a_damaged_reference_picture_freezes_the_picture_until_the_next_keyframe() {
        let (packets, spans) = stream(130, 60);
        // picture 10 (a TRAIL_R) loses its packets after the first three, like a lost SRT datagram
        let (from, to) = spans[10];
        let (out, mut guard) = run(&without(&packets, from + 3..to));
        let pictures = decoded(&out);
        assert_eq!(pictures.len(), 130, "every picture is still there, with its timestamp");
        assert_eq!(hidden(&pictures), (10..60).collect::<Vec<u64>>());
        assert_eq!(pictures[60], (60, NAL_IDR_W_RADL));
        let s = guard.stats();
        assert_eq!((s.losses, s.damaged, s.hidden, s.freezes, s.recoveries, s.dropped), (1, 1, 50, 1, 1, 0));
        assert!(!s.frozen);
        assert_eq!(guard.take_events(), vec![GuardEvent::Froze, GuardEvent::Recovered { skipped: 50, ticks: 50 * FRAME }]);
        assert!(guard.take_events().is_empty());
    }

    #[test]
    fn a_damaged_picture_nothing_refers_to_is_the_only_one_hidden() {
        let (packets, spans) = stream(70, 60);
        let (from, _) = spans[12];
        assert_eq!(12 % 3, 0, "a TRAIL_N");
        let (out, guard) = run(&without(&packets, from + 2..from + 4));
        assert_eq!(hidden(&decoded(&out)), vec![12]);
        assert_eq!((guard.stats().freezes, guard.stats().hidden), (0, 1));
    }

    #[test]
    fn a_damaged_keyframe_is_left_out_and_the_freeze_lasts_to_the_next() {
        let (packets, spans) = stream(130, 60);
        let (from, _) = spans[60];
        let (out, guard) = run(&without(&packets, from + 5..from + 12));
        let pictures = decoded(&out);
        assert!(!pictures.iter().any(|(n, _)| *n == 60), "the broken keyframe is not passed on");
        assert_eq!(hidden(&pictures), (61..120).collect::<Vec<u64>>());
        assert_eq!(pictures.iter().find(|(n, _)| *n == 120).map(|(_, t)| *t), Some(NAL_IDR_W_RADL));
        let s = guard.stats();
        assert_eq!((s.dropped, s.hidden, s.freezes, s.recoveries), (1, 59, 1, 1));
    }

    #[test]
    fn whole_pictures_lost_are_noticed_from_the_timestamps() {
        let (packets, spans) = stream(130, 60);
        // the video of pictures 20..=35 vanishes: 16 pictures of 9 packets, a multiple of 16 packets, so the
        // continuity counter shows no gap at all
        let (from, to) = (spans[20].0, spans[35].1);
        let lossy: Vec<[u8; PACKET_BYTES]> =
            packets.iter().enumerate().filter(|(i, p)| !(from..to).contains(i) || pid_of(&p[..]) != VIDEO).map(|(_, p)| *p).collect();
        assert_eq!((packets.len() - lossy.len()) % 16, 0);
        let (out, guard) = run(&lossy);
        let pictures = decoded(&out);
        assert_eq!(guard.stats().losses, 0, "nothing the counter could see");
        assert_eq!(hidden(&pictures), (36..60).collect::<Vec<u64>>());
        assert_eq!(guard.stats().freezes, 1);
    }

    #[test]
    fn nothing_before_the_first_keyframe_is_passed_on() {
        let (packets, spans) = stream(130, 60);
        // a viewer that joins at picture 33
        let mut late = Mux::new().packets;
        late.extend_from_slice(&packets[spans[33].0..]);
        let (out, guard) = run(&late);
        let pictures = decoded(&out);
        assert_eq!(pictures.first(), Some(&(60, NAL_IDR_W_RADL)));
        assert_eq!((guard.stats().before_keyframe, guard.stats().dropped, guard.stats().freezes), (27, 0, 0));
    }

    #[test]
    fn a_cra_becomes_a_bla_where_the_pictures_before_it_are_gone() {
        let mut mux = Mux::new();
        let mut spans = Vec::new();
        for n in 0..=70u64 {
            mux.audio(n);
            let from = mux.packets.len();
            mux.picture(n, if n % 30 == 0 { NAL_CRA } else { NAL_TRAIL_R }, 1500);
            spans.push((from, mux.packets.len()));
        }
        let (from, _) = spans[40];
        let (out, guard) = run(&without(&mux.packets, from + 3..from + 4));
        let pictures = decoded(&out);
        // the stream starts at a CRA: it becomes a BLA too (its leading pictures may refer to nothing)
        assert_eq!(pictures[0], (0, NAL_BLA_W_LP));
        assert_eq!(pictures[30], (30, NAL_CRA), "a CRA in an intact stream is left alone");
        // after a mid-stream CRA the decoder decodes RASL pictures, so the damaged ones are left out instead
        assert!(!pictures.iter().any(|(n, _)| (40..60).contains(n)), "{pictures:?}");
        assert_eq!(pictures.iter().find(|(n, _)| *n == 60).map(|(_, t)| *t), Some(NAL_BLA_W_LP));
        assert_eq!((guard.stats().recoveries, guard.stats().dropped), (1, 20));
    }

    #[test]
    fn audio_and_tables_pass_through_and_other_codecs_are_left_alone() {
        let (packets, spans) = stream(70, 60);
        let (from, _) = spans[30];
        let lossy = without(&packets, from + 1..from + 2);
        let (out, _) = run(&lossy);
        let count = |bytes: &[u8], pid: u16| bytes.chunks(PACKET_BYTES).filter(|p| pid_of(p) == pid).count();
        let input: Vec<u8> = lossy.iter().flatten().copied().collect();
        assert_eq!(count(&out, AUDIO), count(&input, AUDIO));
        assert_eq!(count(&out, PAT_PID), count(&input, PAT_PID));

        // the same stream announced as H.264 (stream type 0x1B): not guarded, every byte goes through
        let mut h264 = lossy.clone();
        for p in h264.iter_mut().filter(|p| pid_of(&p[..]) == PMT_PID) {
            p[5 + 12] = 0x1B;
        }
        let (out, guard) = run(&h264);
        assert_eq!(out, h264.iter().flatten().copied().collect::<Vec<u8>>());
        assert!(!guard.stats().guarding);
    }

    #[test]
    fn sync_is_found_again_after_stray_bytes() {
        let (packets, _) = stream(70, 60);
        let mut bytes: Vec<u8> = vec![1, 2, 3];
        for (i, p) in packets.iter().enumerate() {
            bytes.extend_from_slice(p);
            if i == 50 {
                bytes.extend_from_slice(&[9, 9]);
            }
        }
        let mut guard = TsGuard::new();
        let mut out = Vec::new();
        for chunk in bytes.chunks(77) {
            guard.push(chunk, &mut out);
        }
        assert!(out.chunks(PACKET_BYTES).all(|p| p.len() == PACKET_BYTES && p[0] == SYNC));
        assert_eq!(decoded(&out).len(), 70);
    }

    #[test]
    fn a_duplicate_packet_is_passed_on_once() {
        let (packets, spans) = stream(70, 60);
        let mut doubled = packets.clone();
        let (from, _) = spans[5];
        doubled.insert(from + 2, packets[from + 1]);
        let (out, guard) = run(&doubled);
        assert!(hidden(&decoded(&out)).is_empty());
        assert_eq!(guard.stats().losses, 0);
    }
}
