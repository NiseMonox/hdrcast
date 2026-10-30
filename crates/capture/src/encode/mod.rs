//! Encoding inside the engine (docs/design.md §2.8). With `--encode` the engine hands its frames to the GPU's
//! encoder itself and writes the video into the video pipe as an MPEG-TS of HEVC pictures; ffmpeg then only
//! adds the sound and copies the video (`-c:v copy`). What this buys is control while the stream runs: a
//! keyframe the moment the app asks for one (a viewer lost part of the stream), and a new bitrate without a
//! restart. ffmpeg's command line can do neither.
//!
//! * [`VideoEncoder`] is what a platform's encoder does (NVENC on Windows);
//! * [`KeyframeClock`] decides which frames become keyframes: the first, every `--gop` frames after the previous
//!   one, and any frame one was asked for;
//! * [`ts::TsWriter`] wraps the pictures into a transport stream, or in control mode (`--low-latency`, design.md
//!   section 11) [`frames`] gives each picture a record of its own that HdrCast reads;
//! * [`EncodeControl`] carries the app's commands (lines on stdin) to the thread that encodes, and
//!   [`EncoderEvent`]s go back as JSON lines on stdout;
//! * [`EncodeStage`] puts them together on the sink's writer thread.

pub mod frames;
pub mod ts;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use crate::clock::{now_ticks, TICKS_PER_SECOND};
use crate::frame::SharedFrame;
use crate::log;

/// One picture out of the encoder, in decode order: an HEVC access unit (Annex B, with the parameter sets in
/// front of every keyframe) and its times in frames of the stream. The presentation time is the frame's index;
/// with B-frames the decode time can be lower, and below 0 for the first pictures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedPicture {
    pub data: Vec<u8>,
    pub pts: i64,
    pub dts: i64,
    pub keyframe: bool,
}

/// What a platform's encoder does for the engine. Built on the engine's thread, then used from the sink's
/// writer thread only.
pub trait VideoEncoder: Send {
    /// What it is, for the log.
    fn describe(&self) -> String;

    /// Zero copy (docs/design.md §2.8): how many surfaces on the GPU the screen converts its pictures into for
    /// this encoder; the engine's frame pool then has one buffer without bytes per surface. 0: the frames come in
    /// memory.
    fn surfaces(&self) -> usize {
        0
    }

    /// Encodes `frame` as the frame of index `index` (0, 1, 2, … in presentation order); `keyframe`: it must
    /// become an IDR picture. Appends the pictures that came out: none while a B-frame waits for the picture
    /// after it, several once that came. `frame` holds the layout's raw format, or with surfaces nothing: the
    /// picture is in surface `frame.index()`. An encoder that still reads a surface after returning keeps a
    /// clone of `frame` until it is done, so the engine does not convert the next picture into it.
    fn encode(&mut self, frame: &SharedFrame, index: u64, keyframe: bool, out: &mut Vec<EncodedPicture>) -> Result<(), String>;

    /// Changes the bitrate and the rate buffer (kbit) from the next frame on, without a keyframe. An error says
    /// why it cannot; the stream goes on as before.
    fn set_bitrate(&mut self, kbps: u32, vbv_kbits: u32) -> Result<(), String>;

    /// The stream ends: appends whatever is still inside.
    fn finish(&mut self, out: &mut Vec<EncodedPicture>) -> Result<(), String>;
}

/// Which frames become keyframes: the first, then every `gop` frames after the previous keyframe, and any frame
/// one was asked for. One asked for restarts the count, so the next unasked one comes a whole interval later
/// instead of right after it. A `gop` of 0 (control mode) makes only the first and the ones asked for.
#[derive(Debug, Clone)]
pub struct KeyframeClock {
    gop: Option<u64>,
    last: Option<u64>,
}

impl KeyframeClock {
    pub fn new(gop_frames: u32) -> Self {
        Self { gop: (gop_frames > 0).then_some(u64::from(gop_frames)), last: None }
    }

    /// Whether frame `index` is to be a keyframe; `asked`: somebody asked for one since the previous frame.
    pub fn is_keyframe(&mut self, index: u64, asked: bool) -> bool {
        let due = match (self.last, self.gop) {
            (None, _) => true,
            (Some(last), Some(gop)) => index.saturating_sub(last) >= gop,
            (Some(_), None) => false,
        };
        if due || asked {
            self.last = Some(index);
        }
        due || asked
    }

    /// Frames from `index` to the next keyframe nobody needs to ask for; `u64::MAX` when none will come.
    pub fn frames_to_next(&self, index: u64) -> u64 {
        match (self.last, self.gop) {
            (None, _) => 0,
            (Some(last), Some(gop)) => (last + gop).saturating_sub(index),
            (Some(_), None) => u64::MAX,
        }
    }
}

#[derive(Default)]
struct Requests {
    /// When the oldest keyframe request not served yet came in.
    keyframe: Option<Instant>,
    bitrate: Option<(u32, u32)>,
}

/// The app's commands on their way from stdin to the thread that encodes. One line each:
/// `keyframe`, or `bitrate <kbps> [<vbv kbit>]` (the buffer defaults to a quarter second).
#[derive(Default)]
pub struct EncodeControl {
    requests: Mutex<Requests>,
}

impl EncodeControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn requests(&self) -> std::sync::MutexGuard<'_, Requests> {
        self.requests.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A keyframe as soon as possible. Requests until the next frame count as one, timed from the first.
    pub fn ask_keyframe(&self) {
        self.requests().keyframe.get_or_insert_with(Instant::now);
    }

    pub fn ask_bitrate(&self, kbps: u32, vbv_kbits: u32) {
        self.requests().bitrate = Some((kbps, vbv_kbits));
    }

    fn take_keyframe(&self) -> Option<Instant> {
        self.requests().keyframe.take()
    }

    fn take_bitrate(&self) -> Option<(u32, u32)> {
        self.requests().bitrate.take()
    }

    /// One command line from stdin; the error is for the log.
    pub fn command(&self, line: &str) -> Result<(), String> {
        let mut words = line.split_whitespace();
        match (words.next(), words.next(), words.next(), words.next()) {
            (None, ..) => Ok(()),
            (Some("keyframe"), None, ..) => {
                self.ask_keyframe();
                Ok(())
            }
            (Some("bitrate"), Some(kbps), vbv, None) => {
                let number = |text: &str| text.parse::<u32>().ok().filter(|v| (100..=1_000_000).contains(v));
                let kbps = number(kbps).ok_or_else(|| format!("not a bitrate in kbit/s: {line}"))?;
                let vbv = match vbv {
                    Some(text) => text.parse::<u32>().ok().filter(|v| *v > 0).ok_or_else(|| format!("not a buffer size in kbit: {line}"))?,
                    None => kbps / 4,
                };
                self.ask_bitrate(kbps, vbv);
                Ok(())
            }
            _ => Err(format!("unknown command: {line}")),
        }
    }
}

/// What the engine tells the app about its encoder: one JSON line on stdout each.
/// `{"keyframe":{"frame":1234,"asked":true,"bytes":286320,"waited_ms":4.2}}` and
/// `{"bitrate":{"kbps":30000,"ok":true}}`. Neither is a flat object, so neither reads as a statistics line.
#[derive(Debug, Clone, PartialEq)]
pub enum EncoderEvent {
    /// A keyframe went into the pipe. `asked`: somebody had asked for one; `waited_ms`: how long ago that was
    /// (from the command arriving to the picture being written).
    Keyframe { frame: u64, asked: bool, bytes: usize, waited_ms: Option<f64> },
    /// A new bitrate was asked for: whether the encoder took it.
    Bitrate { kbps: u32, ok: bool },
}

impl EncoderEvent {
    pub fn to_json_line(&self) -> String {
        match self {
            EncoderEvent::Keyframe { frame, asked, bytes, waited_ms } => match waited_ms {
                Some(ms) => format!(r#"{{"keyframe":{{"frame":{frame},"asked":{asked},"bytes":{bytes},"waited_ms":{:.1}}}}}"#, ms),
                None => format!(r#"{{"keyframe":{{"frame":{frame},"asked":{asked},"bytes":{bytes}}}}}"#),
            },
            EncoderEvent::Bitrate { kbps, ok } => format!(r#"{{"bitrate":{{"kbps":{kbps},"ok":{ok}}}}}"#),
        }
    }

    /// Parses a [`to_json_line`](Self::to_json_line) line; `None` for anything else.
    pub fn try_parse(line: &str) -> Option<EncoderEvent> {
        let line = line.trim();
        if !line.starts_with("{\"keyframe\"") && !line.starts_with("{\"bitrate\"") {
            return None;
        }
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        if let Some(k) = value.get("keyframe") {
            return Some(EncoderEvent::Keyframe {
                frame: k.get("frame")?.as_u64()?,
                asked: k.get("asked")?.as_bool()?,
                bytes: k.get("bytes")?.as_u64()? as usize,
                waited_ms: k.get("waited_ms").and_then(serde_json::Value::as_f64),
            });
        }
        let b = value.get("bitrate")?;
        Some(EncoderEvent::Bitrate { kbps: u32::try_from(b.get("kbps")?.as_u64()?).ok()?, ok: b.get("ok")?.as_bool()? })
    }
}

/// Turns the sink's frames into the transport stream that goes into the pipe, on the sink's writer thread:
/// [`push`](Self::push) a frame, write [`output`](Self::output), then call [`written`](Self::written).
pub struct EncodeStage {
    encoder: Box<dyn VideoEncoder>,
    ts: ts::TsWriter,
    /// Control mode: each picture gets a record of its own ([`frames`]) instead of going into the transport stream.
    framed: bool,
    clock: KeyframeClock,
    control: Arc<EncodeControl>,
    next_index: u64,
    pictures: Vec<EncodedPicture>,
    output: Vec<u8>,
    /// The keyframe request this stage is serving: when it came in.
    asked_at: Option<Instant>,
    /// The frame of the last push: when it was captured, whether it repeats the one before, how long it took.
    captured: i64,
    repeat: bool,
    encode_us: u32,
    events: Box<dyn FnMut(&EncoderEvent) + Send>,
}

impl EncodeStage {
    /// `events` gets what the app is told (normally printed to stdout).
    pub fn new(
        encoder: Box<dyn VideoEncoder>,
        fps: u32,
        gop_frames: u32,
        control: Arc<EncodeControl>,
        events: Box<dyn FnMut(&EncoderEvent) + Send>,
    ) -> Self {
        Self {
            encoder,
            ts: ts::TsWriter::new(fps),
            framed: false,
            clock: KeyframeClock::new(gop_frames),
            control,
            next_index: 0,
            pictures: Vec::new(),
            output: Vec::with_capacity(1 << 20),
            asked_at: None,
            captured: 0,
            repeat: false,
            encode_us: 0,
            events,
        }
    }

    /// Control mode (design.md section 11.2): the output is one [`frames::FrameRecord`] per picture.
    pub fn framed(mut self) -> Self {
        self.framed = true;
        self
    }

    /// Encodes the next frame. What it produced is in [`output`](Self::output) (possibly nothing).
    pub fn push(&mut self, frame: &SharedFrame) -> Result<(), String> {
        if let Some((kbps, vbv)) = self.control.take_bitrate() {
            let ok = match self.encoder.set_bitrate(kbps, vbv) {
                Ok(()) => {
                    log::info(format!("capture: the encoder now runs at {kbps} kbit/s (buffer {vbv} kbit)"));
                    true
                }
                Err(e) => {
                    log::warn(format!("capture: the encoder cannot change its bitrate to {kbps} kbit/s: {e}"));
                    false
                }
            };
            (self.events)(&EncoderEvent::Bitrate { kbps, ok });
        }
        let asked = self.control.take_keyframe();
        if let Some(at) = asked {
            self.asked_at.get_or_insert(at);
        }
        let keyframe = self.clock.is_keyframe(self.next_index, asked.is_some());
        // the same buffer again, captured at the same moment: the engine had nothing new to send
        self.repeat = frame.captured_ticks() != 0 && frame.captured_ticks() == self.captured;
        self.captured = frame.captured_ticks();
        self.pictures.clear();
        let began = Instant::now();
        self.encoder.encode(frame, self.next_index, keyframe, &mut self.pictures)?;
        self.encode_us = micros(began.elapsed().as_secs_f64());
        self.next_index += 1;
        self.wrap();
        Ok(())
    }

    /// The stream ends: whatever the encoder still holds, in [`output`](Self::output).
    pub fn finish(&mut self) -> Result<(), String> {
        self.pictures.clear();
        self.encoder.finish(&mut self.pictures)?;
        self.wrap();
        Ok(())
    }

    fn wrap(&mut self) {
        self.output.clear();
        if self.framed {
            // no B-frames in control mode: the picture that came out is the frame just pushed
            let age = if self.captured != 0 { micros((now_ticks() - self.captured) as f64 / TICKS_PER_SECOND as f64) } else { 0 };
            for picture in &self.pictures {
                let asked = picture.keyframe && self.asked_at.is_some();
                let flags = frames::flags(picture.keyframe, self.repeat, asked);
                frames::write_header(picture.pts.max(0) as u64, flags, age, self.encode_us, picture.data.len(), &mut self.output);
                self.output.extend_from_slice(&picture.data);
            }
            return;
        }
        for picture in &self.pictures {
            self.ts.write(picture, &mut self.output);
        }
    }

    /// The transport stream bytes of the last [`push`](Self::push) or [`finish`](Self::finish).
    pub fn output(&self) -> &[u8] {
        &self.output
    }

    /// The output is in the pipe: says which keyframes went, and how long the one asked for took.
    pub fn written(&mut self) {
        for picture in self.pictures.iter().filter(|p| p.keyframe) {
            let waited = self.asked_at.take().map(|at| at.elapsed().as_secs_f64() * 1000.0);
            (self.events)(&EncoderEvent::Keyframe {
                frame: picture.pts.max(0) as u64,
                asked: waited.is_some(),
                bytes: picture.data.len(),
                waited_ms: waited,
            });
        }
    }
}

/// Seconds as whole µs, saturating.
fn micros(seconds: f64) -> u32 {
    (seconds * 1e6).round().clamp(0.0, f64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::FramePool;

    #[test]
    fn with_no_interval_only_the_first_keyframe_and_those_asked_for_come() {
        let mut clock = KeyframeClock::new(0);
        let keyframes: Vec<u64> = (0..1000).filter(|&i| clock.is_keyframe(i, i == 400 || i == 401)).collect();
        assert_eq!(keyframes, vec![0, 400, 401]);
        assert_eq!(clock.frames_to_next(500), u64::MAX);
    }

    #[test]
    fn framed_output_is_one_record_per_picture_with_its_age_and_whether_it_repeats() {
        let control = EncodeControl::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let mut stage = EncodeStage::new(
            Box::new(Fake { held: None, hold_odd: false, bitrates: Arc::new(Mutex::new(Vec::new())) }),
            60,
            0,
            control.clone(),
            Box::new(move |e| sink.lock().unwrap().push(e.clone())),
        )
        .framed();
        let pool = FramePool::new(3, 4);
        let mut stream = Vec::new();
        let mut shared = None;
        for i in 0..6u8 {
            if i == 4 {
                control.command("keyframe").unwrap();
            }
            // frames 2 and 3 repeat frame 1's buffer (nothing new on the screen)
            if i != 2 && i != 3 {
                let mut frame = pool.try_rent().unwrap();
                frame.fill(i);
                frame.set_captured_ticks(now_ticks() - 3 * crate::clock::TICKS_PER_MS);
                shared = Some(Arc::new(frame));
            }
            stage.push(shared.as_ref().unwrap()).unwrap();
            stream.extend_from_slice(stage.output());
            stage.written();
        }
        let mut reader = &stream[..];
        let mut records = Vec::new();
        while let Some(record) = frames::FrameRecord::read(&mut reader).unwrap() {
            records.push(record);
        }
        let summary: Vec<(u64, bool, bool, bool, u8)> =
            records.iter().map(|r| (r.index, r.keyframe, r.repeat, r.asked, r.data[0])).collect();
        assert_eq!(
            summary,
            vec![(0, true, false, false, 0), (1, false, false, false, 1), (2, false, true, false, 1), (3, false, true, false, 1), (4, true, false, true, 4), (5, false, false, false, 5)]
        );
        assert!(records.iter().all(|r| r.age_us >= 3000), "{records:?}");
        assert!(events.lock().unwrap().iter().any(|e| matches!(e, EncoderEvent::Keyframe { frame: 4, asked: true, .. })));
    }

    #[test]
    fn keyframes_come_every_gop_frames_and_one_asked_for_restarts_the_count() {
        let mut clock = KeyframeClock::new(60);
        let keyframes: Vec<u64> = (0..200).filter(|&i| clock.is_keyframe(i, i == 75)).collect();
        assert_eq!(keyframes, vec![0, 60, 75, 135, 195]);
        assert_eq!(clock.frames_to_next(196), 59);
        // asked for on a frame that is due anyway: one keyframe, the count restarts as usual
        let mut clock = KeyframeClock::new(10);
        assert!(clock.is_keyframe(0, true));
        assert!(!clock.is_keyframe(9, false));
        assert!(clock.is_keyframe(10, true));
        assert!(clock.is_keyframe(20, false));
    }

    #[test]
    fn commands_are_parsed_and_requests_until_the_next_frame_count_as_one() {
        let control = EncodeControl::new();
        control.command("keyframe").unwrap();
        let first = control.requests().keyframe.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        control.command("keyframe").unwrap();
        assert_eq!(control.take_keyframe(), Some(first), "timed from the first request");
        assert_eq!(control.take_keyframe(), None);

        control.command("bitrate 30000").unwrap();
        assert_eq!(control.take_bitrate(), Some((30000, 7500)));
        control.command("  bitrate 30000 10000 ").unwrap();
        assert_eq!(control.take_bitrate(), Some((30000, 10000)));
        control.command("").unwrap();
        for bad in ["bitrate", "bitrate x", "bitrate 5", "bitrate 30000 0", "keyframe now", "idr", "bitrate 1 2 3"] {
            assert!(control.command(bad).is_err(), "{bad}");
        }
        assert_eq!(control.take_bitrate(), None);
    }

    #[test]
    fn events_go_out_as_lines_that_come_back_and_are_not_statistics() {
        let events = [
            EncoderEvent::Keyframe { frame: 1234, asked: true, bytes: 286320, waited_ms: Some(4.2) },
            EncoderEvent::Keyframe { frame: 60, asked: false, bytes: 1, waited_ms: None },
            EncoderEvent::Bitrate { kbps: 30000, ok: false },
        ];
        for event in events {
            let line = event.to_json_line();
            assert_eq!(EncoderEvent::try_parse(&line), Some(event.clone()), "{line}");
            assert_eq!(crate::CaptureStats::try_parse(&line), None, "{line}");
            assert_eq!(crate::FrameLayout::try_parse(&line), None, "{line}");
        }
        assert_eq!(
            EncoderEvent::Keyframe { frame: 7, asked: true, bytes: 9, waited_ms: Some(12.345) }.to_json_line(),
            r#"{"keyframe":{"frame":7,"asked":true,"bytes":9,"waited_ms":12.3}}"#
        );
        assert_eq!(EncoderEvent::try_parse(r#"{"t":1.0,"fps":60.0}"#), None);
    }

    /// Hands out each frame's first byte as its picture; holds one frame back when told to, like a B-frame.
    struct Fake {
        held: Option<(u64, u8)>,
        hold_odd: bool,
        bitrates: Arc<Mutex<Vec<u32>>>,
    }

    impl VideoEncoder for Fake {
        fn describe(&self) -> String {
            "fake".into()
        }

        fn encode(&mut self, frame: &SharedFrame, index: u64, keyframe: bool, out: &mut Vec<EncodedPicture>) -> Result<(), String> {
            let picture = |i: u64, byte: u8, key: bool| EncodedPicture { data: vec![byte; 10], pts: i as i64, dts: i as i64 - 1, keyframe: key };
            if self.hold_odd && index % 2 == 1 && !keyframe {
                self.held = Some((index, frame[0]));
                return Ok(());
            }
            out.push(picture(index, frame[0], keyframe));
            if let Some((i, byte)) = self.held.take() {
                out.push(picture(i, byte, false));
            }
            Ok(())
        }

        fn set_bitrate(&mut self, kbps: u32, _vbv_kbits: u32) -> Result<(), String> {
            self.bitrates.lock().unwrap().push(kbps);
            if kbps > 50_000 {
                Err("too much".into())
            } else {
                Ok(())
            }
        }

        fn finish(&mut self, out: &mut Vec<EncodedPicture>) -> Result<(), String> {
            if let Some((i, byte)) = self.held.take() {
                out.push(EncodedPicture { data: vec![byte; 10], pts: i as i64, dts: i as i64 - 1, keyframe: false });
            }
            Ok(())
        }
    }

    #[test]
    fn the_stage_asks_the_encoder_for_keyframes_and_reports_them_once_written() {
        let control = EncodeControl::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let bitrates = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let mut stage = EncodeStage::new(
            Box::new(Fake { held: None, hold_odd: true, bitrates: bitrates.clone() }),
            60,
            30,
            control.clone(),
            Box::new(move |e| sink.lock().unwrap().push(e.clone())),
        );
        let pool = FramePool::new(2, 4);
        let mut stream = Vec::new();
        for i in 0..40u8 {
            if i == 9 {
                control.command("keyframe").unwrap();
                control.command("bitrate 20000").unwrap();
            }
            if i == 20 {
                control.command("bitrate 60000").unwrap();
            }
            let mut frame = pool.try_rent().unwrap();
            frame.fill(i);
            stage.push(&Arc::new(frame)).unwrap();
            stream.extend_from_slice(stage.output());
            stage.written();
        }
        stage.finish().unwrap();
        stream.extend_from_slice(stage.output());
        assert_eq!(stream.len() % ts::PACKET_BYTES, 0);
        assert_eq!(*bitrates.lock().unwrap(), vec![20000, 60000]);
        let events = events.lock().unwrap();
        let keyframes: Vec<(u64, bool)> = events
            .iter()
            .filter_map(|e| match e {
                EncoderEvent::Keyframe { frame, asked, .. } => Some((*frame, *asked)),
                _ => None,
            })
            .collect();
        // frame 9 was asked for; the next unasked one is a whole interval later
        assert_eq!(keyframes, vec![(0, false), (9, true), (39, false)]);
        assert!(events.contains(&EncoderEvent::Bitrate { kbps: 20000, ok: true }));
        assert!(events.contains(&EncoderEvent::Bitrate { kbps: 60000, ok: false }));
        assert!(events.iter().any(|e| matches!(e, EncoderEvent::Keyframe { frame: 9, waited_ms: Some(ms), .. } if *ms >= 0.0)));
    }
}
