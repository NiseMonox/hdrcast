//! Sealing, opening and the two message bodies.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::ChaCha20Poly1305;

pub const VERSION: u8 = 1;
pub const DIR_VIEWER: u8 = 1;
pub const DIR_HOST: u8 = 2;
const AAD: &[u8] = b"HDRCCTL1";
const HEADER: usize = 9;
/// Events carried per datagram (the oldest unacknowledged ones): 14 + 32 * 8 bytes of plaintext stays far below
/// any path MTU.
pub const MAX_EVENTS_PER_PACKET: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EventKind {
    KeyDown = 1,
    KeyUp = 2,
    ButtonDown = 3,
    ButtonUp = 4,
    /// `code` 0 = vertical, 1 = horizontal; `arg` in 1/120 notches, positive = up / right.
    Wheel = 5,
    /// A character with no key of its own (IME output): `code` is the Unicode scalar.
    Text = 6,
}

impl EventKind {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::KeyDown,
            2 => Self::KeyUp,
            3 => Self::ButtonDown,
            4 => Self::ButtonUp,
            5 => Self::Wheel,
            6 => Self::Text,
            _ => return None,
        })
    }
}

/// One key, button or wheel step. `code` is a [`crate::keys::Key`] code or a [`crate::keys::Button`] number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    pub kind: EventKind,
    /// [`crate::keys::Mods`] bits held with it.
    pub mods: u8,
    pub code: u32,
    pub arg: i16,
}

/// What the viewer sends: where the pointer is, and the events the host has not acknowledged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerMessage {
    pub wants_control: bool,
    /// The keys come from a Mac's keyboard: Command arrives as Meta, Control as Ctrl (flags bit 1, 0.3.9 on; older
    /// hosts read bit 0 only).
    pub mac_keyboard: bool,
    pub pointer_x: u16,
    pub pointer_y: u16,
    pub pointer_inside: bool,
    /// Sequence number of `events[0]`; the following ones count up from it.
    pub first_seq: u32,
    pub events: Vec<Event>,
}

/// What the host answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostMessage {
    pub state: GrantState,
    /// The last event sequence number applied (0 = none yet).
    pub acked_seq: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GrantState {
    /// Not offered, refused, or taken back.
    Denied = 0,
    /// The host's user has been asked.
    Pending = 1,
    Granted = 2,
}

impl ViewerMessage {
    pub fn encode(&self) -> Vec<u8> {
        let n = self.events.len().min(MAX_EVENTS_PER_PACKET);
        let mut out = Vec::with_capacity(12 + n * 8);
        out.push(VERSION);
        out.push(self.wants_control as u8 | (self.mac_keyboard as u8) << 1);
        out.extend_from_slice(&self.pointer_x.to_be_bytes());
        out.extend_from_slice(&self.pointer_y.to_be_bytes());
        out.push(self.pointer_inside as u8);
        out.extend_from_slice(&self.first_seq.to_be_bytes());
        out.push(n as u8);
        for e in &self.events[..n] {
            out.push(e.kind as u8);
            out.push(e.mods);
            out.extend_from_slice(&e.code.to_be_bytes());
            out.extend_from_slice(&e.arg.to_be_bytes());
        }
        out
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < 12 || b[0] != VERSION {
            return None;
        }
        let n = b[11] as usize;
        if n > MAX_EVENTS_PER_PACKET || b.len() != 12 + n * 8 {
            return None;
        }
        let mut events = Vec::with_capacity(n);
        for chunk in b[12..].chunks_exact(8) {
            events.push(Event {
                kind: EventKind::from_u8(chunk[0])?,
                mods: chunk[1],
                code: u32::from_be_bytes([chunk[2], chunk[3], chunk[4], chunk[5]]),
                arg: i16::from_be_bytes([chunk[6], chunk[7]]),
            });
        }
        Some(Self {
            wants_control: b[1] & 1 != 0,
            mac_keyboard: b[1] & 2 != 0,
            pointer_x: u16::from_be_bytes([b[2], b[3]]),
            pointer_y: u16::from_be_bytes([b[4], b[5]]),
            pointer_inside: b[6] != 0,
            first_seq: u32::from_be_bytes([b[7], b[8], b[9], b[10]]),
            events,
        })
    }
}

impl HostMessage {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6);
        out.push(VERSION);
        out.push(self.state as u8);
        out.extend_from_slice(&self.acked_seq.to_be_bytes());
        out
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != 6 || b[0] != VERSION {
            return None;
        }
        let state = match b[1] {
            0 => GrantState::Denied,
            1 => GrantState::Pending,
            2 => GrantState::Granted,
            _ => return None,
        };
        Some(Self { state, acked_seq: u32::from_be_bytes([b[2], b[3], b[4], b[5]]) })
    }
}

/// One direction of the channel: seals what this side sends and opens what the other side sent.
pub struct Channel {
    cipher: ChaCha20Poly1305,
    send_dir: u8,
    recv_dir: u8,
    send_counter: u64,
    recv_counter: u64,
}

impl Channel {
    /// `key` is the session's control key (`GroupKeys::sub("ctl/" + sid)`).
    pub fn viewer(key: &[u8; 32]) -> Self {
        Self::new(key, DIR_VIEWER, DIR_HOST)
    }

    pub fn host(key: &[u8; 32]) -> Self {
        Self::new(key, DIR_HOST, DIR_VIEWER)
    }

    /// The host's end of the messages about the shared display ([`crate::displays`]): the same key, with the kinds
    /// of those messages as direction bytes, so that no nonce is ever used twice.
    pub fn displays_host(key: &[u8; 32]) -> Self {
        Self::new(key, crate::displays::LIST, crate::displays::PICK)
    }

    pub fn displays_viewer(key: &[u8; 32]) -> Self {
        Self::new(key, crate::displays::PICK, crate::displays::LIST)
    }

    fn new(key: &[u8; 32], send_dir: u8, recv_dir: u8) -> Self {
        let cipher = ChaCha20Poly1305::new_from_slice(key).expect("a 32-byte key");
        Self { cipher, send_dir, recv_dir, send_counter: 0, recv_counter: 0 }
    }

    fn nonce(dir: u8, counter: u64) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[0] = dir;
        n[4..].copy_from_slice(&counter.to_be_bytes());
        n
    }

    pub fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        self.send_counter += 1;
        let nonce = Self::nonce(self.send_dir, self.send_counter);
        let sealed = self
            .cipher
            .encrypt((&nonce).into(), Payload { msg: plaintext, aad: AAD })
            .expect("sealing a short message cannot fail");
        let mut out = Vec::with_capacity(HEADER + sealed.len());
        out.push(self.send_dir);
        out.extend_from_slice(&self.send_counter.to_be_bytes());
        out.extend_from_slice(&sealed);
        out
    }

    /// The plaintext of an authentic datagram that is newer than every one opened before; `None` for anything
    /// else (forged, from our own direction, replayed, or overtaken on the way).
    pub fn open(&mut self, datagram: &[u8]) -> Option<Vec<u8>> {
        if datagram.len() < HEADER + 16 || datagram[0] != self.recv_dir {
            return None;
        }
        let counter = u64::from_be_bytes(datagram[1..9].try_into().ok()?);
        if counter <= self.recv_counter {
            return None;
        }
        let nonce = Self::nonce(self.recv_dir, counter);
        let plain = self.cipher.decrypt((&nonce).into(), Payload { msg: &datagram[HEADER..], aad: AAD }).ok()?;
        self.recv_counter = counter;
        Some(plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ViewerMessage {
        ViewerMessage {
            wants_control: true,
            mac_keyboard: false,
            pointer_x: 12345,
            pointer_y: 54321,
            pointer_inside: true,
            first_seq: 7,
            events: vec![
                Event { kind: EventKind::KeyDown, mods: 2, code: 'c' as u32, arg: 0 },
                Event { kind: EventKind::Wheel, mods: 0, code: 0, arg: -120 },
            ],
        }
    }

    #[test]
    fn messages_survive_the_round_trip() {
        let m = sample();
        assert_eq!(ViewerMessage::decode(&m.encode()), Some(m));
        let h = HostMessage { state: GrantState::Granted, acked_seq: 0xDEAD_BEEF };
        assert_eq!(HostMessage::decode(&h.encode()), Some(h));
    }

    #[test]
    fn a_macs_keyboard_is_said_in_a_bit_older_hosts_do_not_read() {
        let mac = ViewerMessage { mac_keyboard: true, ..sample() };
        let b = mac.encode();
        assert_eq!(b[1], 0b11);
        assert_eq!(ViewerMessage::decode(&b), Some(mac));
        // what 0.3.8 makes of it: it wants control (bit 0), nothing else changed
        assert_eq!(b[1] & 1, 1);
        let not = ViewerMessage { wants_control: false, ..sample() };
        assert_eq!(not.encode()[1], 0, "a PC's keyboard: the byte 0.3.8 sends");
    }

    #[test]
    fn malformed_messages_are_refused() {
        let mut b = sample().encode();
        assert!(ViewerMessage::decode(&b[..b.len() - 1]).is_none(), "truncated");
        b[0] = 9;
        assert!(ViewerMessage::decode(&b).is_none(), "unknown version");
        let mut b = sample().encode();
        b[12] = 99;
        assert!(ViewerMessage::decode(&b).is_none(), "unknown event kind");
        assert!(HostMessage::decode(&[VERSION, 7, 0, 0, 0, 0]).is_none(), "unknown grant state");
    }

    #[test]
    fn only_the_other_side_with_the_key_is_heard_and_only_once() {
        let key = [7u8; 32];
        let mut viewer = Channel::viewer(&key);
        let mut host = Channel::host(&key);
        let first = viewer.seal(b"one");
        let second = viewer.seal(b"two");
        assert_eq!(host.open(&second).as_deref(), Some(&b"two"[..]));
        assert!(host.open(&second).is_none(), "replayed");
        assert!(host.open(&first).is_none(), "older than what was already opened");
        assert!(viewer.open(&first).is_none(), "our own direction reflected back");

        let mut tampered = viewer.seal(b"three");
        *tampered.last_mut().unwrap() ^= 1;
        assert!(host.open(&tampered).is_none());

        let mut stranger = Channel::viewer(&[8u8; 32]);
        assert!(host.open(&stranger.seal(b"hello")).is_none());
        assert_eq!(viewer.open(&host.seal(b"ack")).as_deref(), Some(&b"ack"[..]));
    }
}
