//! Typing into windows of programs that run above HdrCast: programs that were started as administrator.
//!
//! Windows only lets a program put input into another program's windows when its integrity level is at least that
//! program's (User Interface Privilege Isolation, part of UAC). HdrCast runs as the signed-in user at the normal
//! "medium" level, so while a window of a program that was started as administrator ("high") is in front,
//! `SendInput` still reports success and the input is thrown away. Parsec and Sunshine do not suffer from it
//! because their installers register a Windows service that runs a program at the highest level, SYSTEM, inside the
//! user's session. This does the same, in three parts:
//!
//! - A **service** (`HdrCastInput.exe service`, `service.rs`) that is set up once, with the user's approval of
//!   Windows' administrator prompt: the program is copied to the program files folder, where an ordinary program
//!   cannot change what runs as SYSTEM, and registered with an access list that lets the user start the service (and
//!   nothing else). It starts on demand, not at boot. A service lives in session 0, which cannot reach the user's
//!   desktop, so all it does is start the next part in the user's session and stop when that has left.
//! - The **input program** (`HdrCastInput.exe serve`, `server.rs`) runs in the user's session with the service's token
//!   (SYSTEM, every privilege taken off), listens on a pipe that nobody but the user may write to, and replays what
//!   comes in with `SendInput`, which from that level reaches administrator windows too. It lets go of every key when
//!   a client goes away and leaves a while after the last one.
//! - The **client** (`client.rs`) in HdrCast: when a viewer is given control it starts the service, connects to the
//!   pipe and sends the viewer's input there instead of injecting it itself.
//!
//! Without the service everything works as before. The wire format is [`Frame`]: one tag byte and fixed fields, client
//! to program only; the protocol number is part of the pipe's name, so an app and a program that disagree simply do
//! not see each other (the page then offers to set it up again).

// The wire and the replay are plain data and tested everywhere; only Windows has a pipe to put them on.
#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::HashSet;

use crate::inject::{Injector, ScreenRect};
use crate::keys::{Button, Key, Mods};

#[cfg(windows)]
pub(crate) mod client;
#[cfg(windows)]
pub use client::input_is_blocked;
#[cfg(windows)]
pub mod server;
#[cfg(windows)]
pub mod service;
#[cfg(windows)]
pub mod setup;
#[cfg(test)]
pub(crate) mod testing;

/// Version of the pipe protocol.
pub const PROTOCOL: u8 = 1;
/// The input program (and service), and the name it has in the program files folder.
pub const HELPER_EXE: &str = "HdrCastInput.exe";
/// The Windows service that starts the input program in the user's session.
pub const SERVICE_NAME: &str = "HdrCastInput";
/// What the list of services calls it.
pub const SERVICE_DISPLAY_NAME: &str = "HdrCast input (administrator windows)";

/// The pipe the input program of the user with this SID listens on.
pub fn pipe_name(user_sid: &str) -> String {
    format!(r"\\.\pipe\hdrcast-input-{PROTOCOL}-{user_sid}")
}

/// What the page shows about the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HelperStatus {
    /// Windows only: elsewhere there is nothing to set up.
    pub supported: bool,
    /// The program is in the protected folder and the service that runs it is registered.
    pub installed: bool,
    /// What is installed speaks another protocol than this app: it has to be set up again.
    pub outdated: bool,
    /// The input program is running and listening right now.
    pub running: bool,
    /// This app runs as administrator itself: its own input reaches every window, no service is needed.
    pub not_needed: bool,
}

/// Looks at the system. Cheap enough to call whenever the page opens.
pub fn status() -> HelperStatus {
    #[cfg(windows)]
    return setup::status();
    #[cfg(not(windows))]
    HelperStatus::default()
}

// ---- the wire -----------------------------------------------------------------------------------------------------

const HELLO: u8 = 1;
const POINTER: u8 = 2;
const BUTTON: u8 = 3;
const WHEEL: u8 = 4;
const MODIFIER: u8 = 5;
const KEY: u8 = 6;

/// One thing the app asks the helper to do. The first frame of a connection is always a [`Frame::Hello`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Frame {
    /// The shared display (physical pixels of the virtual desktop): what the pointer positions that follow mean.
    Hello(ScreenRect),
    Pointer(u16, u16),
    Button(Button, bool),
    Wheel(u8, i16),
    Modifier(u8, bool),
    Key(Key, bool),
}

/// Bytes that are not a frame this protocol knows: the connection is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BadFrame;

impl Frame {
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        match *self {
            Frame::Hello(rect) => {
                out.push(HELLO);
                for v in [rect.x, rect.y, rect.width, rect.height] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            Frame::Pointer(x, y) => {
                out.push(POINTER);
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            Frame::Button(button, down) => out.extend_from_slice(&[BUTTON, button as u8, down as u8]),
            Frame::Wheel(axis, delta) => {
                out.extend_from_slice(&[WHEEL, axis]);
                out.extend_from_slice(&delta.to_le_bytes());
            }
            Frame::Modifier(bit, down) => out.extend_from_slice(&[MODIFIER, bit, down as u8]),
            Frame::Key(key, down) => {
                out.push(KEY);
                out.extend_from_slice(&key.code().to_le_bytes());
                out.push(down as u8);
            }
        }
    }

    /// The first frame in `bytes` and how many bytes it took; `Ok(None)` while it is not all there yet.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Option<(Frame, usize)>, BadFrame> {
        let Some(&tag) = bytes.first() else { return Ok(None) };
        let len = match tag {
            HELLO => 33,
            POINTER => 5,
            BUTTON | MODIFIER => 3,
            WHEEL => 4,
            KEY => 6,
            _ => return Err(BadFrame),
        };
        if bytes.len() < len {
            return Ok(None);
        }
        let body = &bytes[1..len];
        let frame = match tag {
            HELLO => {
                let number = |i: usize| f64::from_le_bytes(body[i * 8..i * 8 + 8].try_into().unwrap());
                let rect = ScreenRect { x: number(0), y: number(1), width: number(2), height: number(3) };
                let sane = |v: f64| v.is_finite() && v.abs() <= 1.0e6;
                if !(sane(rect.x) && sane(rect.y) && sane(rect.width) && sane(rect.height)) || rect.width < 1.0 || rect.height < 1.0 {
                    return Err(BadFrame);
                }
                Frame::Hello(rect)
            }
            POINTER => Frame::Pointer(u16::from_le_bytes([body[0], body[1]]), u16::from_le_bytes([body[2], body[3]])),
            BUTTON => Frame::Button(Button::from_code(u32::from(body[0])).ok_or(BadFrame)?, body[1] != 0),
            WHEEL if body[0] <= 1 => Frame::Wheel(body[0], i16::from_le_bytes([body[1], body[2]])),
            MODIFIER if Mods::ALL.iter().any(|(bit, _)| *bit == body[0]) => Frame::Modifier(body[0], body[1] != 0),
            KEY => Frame::Key(Key::from_code(u32::from_le_bytes([body[0], body[1], body[2], body[3]])).ok_or(BadFrame)?, body[4] != 0),
            _ => return Err(BadFrame),
        };
        Ok(Some((frame, len)))
    }
}

/// More than this many keys held down at once is no person at a keyboard.
const MAX_HELD: usize = 256;

/// What the helper does with the frames of one connection: replays them through an injector and remembers what is
/// held down, so that a client that vanishes does not leave a key stuck.
pub(crate) struct Replay {
    make: Box<dyn FnMut(ScreenRect) -> Box<dyn Injector> + Send>,
    injector: Option<Box<dyn Injector>>,
    keys: HashSet<Key>,
    buttons: HashSet<Button>,
    mods: u8,
}

impl Replay {
    /// `make` builds the injector for the display of the connection's Hello.
    pub(crate) fn new(make: impl FnMut(ScreenRect) -> Box<dyn Injector> + Send + 'static) -> Self {
        Self { make: Box::new(make), injector: None, keys: HashSet::new(), buttons: HashSet::new(), mods: 0 }
    }

    /// `false` when the frame has no business here (anything before the Hello, more keys held than hands exist):
    /// the connection is to be dropped.
    pub(crate) fn apply(&mut self, frame: Frame) -> bool {
        if let Frame::Hello(rect) = frame {
            self.release_all();
            self.injector = Some((self.make)(rect));
            return true;
        }
        if self.keys.len() >= MAX_HELD {
            return false;
        }
        let Some(injector) = self.injector.as_mut() else { return false };
        match frame {
            Frame::Hello(_) => {}
            Frame::Pointer(x, y) => injector.pointer(x, y),
            Frame::Button(button, down) => {
                if down {
                    self.buttons.insert(button);
                } else {
                    self.buttons.remove(&button);
                }
                injector.button(button, down);
            }
            Frame::Wheel(axis, delta) => injector.wheel(axis, delta),
            Frame::Modifier(bit, down) => {
                if down {
                    self.mods |= bit;
                } else {
                    self.mods &= !bit;
                }
                injector.modifier(bit, down);
            }
            Frame::Key(key, down) => {
                if down {
                    self.keys.insert(key);
                } else {
                    self.keys.remove(&key);
                }
                injector.key(key, down);
            }
        }
        true
    }

    /// Lets go of everything the client was holding.
    pub(crate) fn release_all(&mut self) {
        let Some(injector) = self.injector.as_mut() else { return };
        for key in std::mem::take(&mut self.keys) {
            injector.key(key, false);
        }
        for button in std::mem::take(&mut self.buttons) {
            injector.button(button, false);
        }
        for (bit, _) in Mods::ALL {
            if self.mods & bit != 0 {
                injector.modifier(bit, false);
            }
        }
        self.mods = 0;
    }
}

impl Drop for Replay {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Recorder;
    use super::*;
    use crate::keys::NamedKey;
    use std::sync::{Arc, Mutex};

    const SCREEN: ScreenRect = ScreenRect { x: -1920.0, y: 0.0, width: 1920.0, height: 1080.0 };

    fn every_kind() -> Vec<Frame> {
        vec![
            Frame::Hello(SCREEN),
            Frame::Pointer(0, 65535),
            Frame::Pointer(32768, 1),
            Frame::Button(Button::Left, true),
            Frame::Button(Button::Forward, false),
            Frame::Wheel(0, -120),
            Frame::Wheel(1, 240),
            Frame::Modifier(Mods::CTRL, true),
            Frame::Modifier(Mods::META, false),
            Frame::Key(Key::Char('a'), true),
            Frame::Key(Key::Char('中'), false),
            Frame::Key(Key::Named(NamedKey::F12), true),
            Frame::Key(Key::Named(NamedKey::Menu), false),
        ]
    }

    #[test]
    fn every_frame_reads_back_from_a_stream_and_waits_for_the_rest() {
        let frames = every_kind();
        let mut stream = Vec::new();
        for f in &frames {
            f.encode(&mut stream);
        }
        let mut at = 0;
        let mut back = Vec::new();
        while at < stream.len() {
            let (frame, used) = Frame::decode(&stream[at..]).unwrap().expect("a whole frame");
            back.push(frame);
            at += used;
        }
        assert_eq!(back, frames);

        // cut anywhere inside a frame: not an error, just not yet
        for f in &frames {
            let mut one = Vec::new();
            f.encode(&mut one);
            for cut in 0..one.len() {
                assert_eq!(Frame::decode(&one[..cut]), Ok(None), "{f:?} cut at {cut}");
            }
        }
    }

    #[test]
    fn bytes_that_are_no_frame_are_refused() {
        assert_eq!(Frame::decode(&[0]), Err(BadFrame));
        assert_eq!(Frame::decode(&[200, 1, 2, 3]), Err(BadFrame));
        assert_eq!(Frame::decode(&[BUTTON, 9, 1]), Err(BadFrame), "no such button");
        assert_eq!(Frame::decode(&[WHEEL, 2, 0, 0]), Err(BadFrame), "no such axis");
        assert_eq!(Frame::decode(&[MODIFIER, 3, 1]), Err(BadFrame), "two modifiers at once");
        let mut key = vec![KEY];
        key.extend_from_slice(&0x0020_0000u32.to_le_bytes());
        key.push(1);
        assert_eq!(Frame::decode(&key), Err(BadFrame), "no such key");
        for bad in [f64::NAN, f64::INFINITY, 1.0e9, 0.0] {
            let mut hello = Vec::new();
            Frame::Hello(ScreenRect { x: 0.0, y: 0.0, width: bad, height: 1080.0 }).encode(&mut hello);
            assert_eq!(Frame::decode(&hello), Err(BadFrame), "a display {bad} wide");
        }
    }

    fn replay_into(rec: &Recorder) -> (Replay, Arc<Mutex<Vec<ScreenRect>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (rec, seen_by_make) = (rec.clone(), seen.clone());
        let replay = Replay::new(move |display| {
            seen_by_make.lock().unwrap().push(display);
            Box::new(rec.clone())
        });
        (replay, seen)
    }

    #[test]
    fn nothing_is_replayed_before_the_hello_and_the_display_comes_from_it() {
        let rec = Recorder::default();
        let (mut replay, seen) = replay_into(&rec);
        assert!(!replay.apply(Frame::Pointer(1, 2)));
        assert!(!replay.apply(Frame::Key(Key::Char('a'), true)));
        assert!(rec.lines().is_empty());
        assert!(replay.apply(Frame::Hello(SCREEN)));
        assert_eq!(*seen.lock().unwrap(), [SCREEN]);
        assert!(replay.apply(Frame::Pointer(10, 20)));
        assert!(replay.apply(Frame::Wheel(0, 120)));
        assert_eq!(rec.lines(), ["move 10,20", "wheel 0 120"]);
    }

    #[test]
    fn a_client_that_vanishes_lets_go_of_what_it_held() {
        let rec = Recorder::default();
        let (mut replay, _) = replay_into(&rec);
        replay.apply(Frame::Hello(SCREEN));
        replay.apply(Frame::Modifier(Mods::SHIFT, true));
        replay.apply(Frame::Button(Button::Left, true));
        replay.apply(Frame::Key(Key::Named(NamedKey::Tab), true));
        replay.apply(Frame::Key(Key::Char('x'), true));
        replay.apply(Frame::Key(Key::Char('x'), false)); // released properly: not released again
        let before = rec.lines().len();
        drop(replay);
        let mut after: Vec<String> = rec.lines()[before..].to_vec();
        after.sort();
        assert_eq!(after, ["Left up", "Named(Tab) up", "mod1 up"]);
    }

    #[test]
    fn a_second_hello_starts_over_with_the_new_display() {
        let rec = Recorder::default();
        let (mut replay, seen) = replay_into(&rec);
        replay.apply(Frame::Hello(SCREEN));
        replay.apply(Frame::Key(Key::Char('a'), true));
        let other = ScreenRect { x: 0.0, y: 0.0, width: 3840.0, height: 2160.0 };
        replay.apply(Frame::Hello(other));
        assert_eq!(*seen.lock().unwrap(), [SCREEN, other]);
        assert_eq!(rec.lines().last().unwrap(), "Char('a') up");
    }

    #[test]
    fn more_keys_held_than_hands_exist_drops_the_connection() {
        let rec = Recorder::default();
        let (mut replay, _) = replay_into(&rec);
        replay.apply(Frame::Hello(SCREEN));
        let mut ok = true;
        for c in (0x4e00u32..).filter_map(char::from_u32).take(MAX_HELD + 1) {
            ok = replay.apply(Frame::Key(Key::Char(c), true));
        }
        assert!(!ok);
    }

    #[test]
    fn the_pipe_belongs_to_one_user_and_one_protocol() {
        let name = pipe_name("S-1-5-21-1-2-3-1001");
        assert!(name.starts_with(r"\\.\pipe\"));
        assert!(name.ends_with("S-1-5-21-1-2-3-1001"));
        assert!(name.contains(&format!("-{PROTOCOL}-")));
    }
}
