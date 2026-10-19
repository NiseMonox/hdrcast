//! The viewer's half: mpv's pointer position and key messages in, sealed datagrams out.

use std::collections::VecDeque;

use crate::keys::{parse_mpv_key, Input};
use crate::wire::{Channel, Event, EventKind, GrantState, HostMessage, ViewerMessage, MAX_EVENTS_PER_PACKET};

/// Pointer moves are sent at most this often (125 Hz); the newest position wins.
const MOVE_INTERVAL_US: i64 = 8_000;
/// Unacknowledged keys and buttons are resent this often until the host confirms them.
const RESEND_INTERVAL_US: i64 = 40_000;
/// With nothing to say, still tell the host we want control (and learn its answer) this often.
const HEARTBEAT_US: i64 = 500_000;
/// The host has not answered for this long: it does not speak this protocol (an older version).
const UNSUPPORTED_AFTER_US: i64 = 5_000_000;
/// Events kept while waiting for acknowledgement; beyond this the oldest are dropped and the host resynchronises.
const MAX_PENDING: usize = 256;

/// What to show the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlState {
    /// Not asked for, and the host has not said yet what it would do.
    Off,
    /// Not asked for; the host would ask its user.
    Offered,
    /// Not asked for; the host would allow it at once (this device may control it).
    Allowed,
    /// Asked; the host's user has not answered (or no answer has arrived yet).
    Requested,
    Granted,
    Denied,
    /// The host never answered: it runs a version without remote control.
    Unsupported,
}

pub struct ViewerControl {
    channel: Channel,
    wants: bool,
    grant: Option<GrantState>,
    pointer: (u16, u16, bool),
    dirty: bool,
    next_seq: u32,
    pending: VecDeque<(u32, Event)>,
    started_us: i64,
    last_send_us: i64,
}

impl ViewerControl {
    /// `key` is the session's control key; `now_us` any monotonic clock (the same one for every later call).
    /// Without `wants_control` the viewer still says it is there (every half second), so the host's answer
    /// tells it whether control would be allowed, asked about, or refused.
    pub fn new(key: &[u8; 32], wants_control: bool, now_us: i64) -> Self {
        Self {
            channel: Channel::viewer(key),
            wants: wants_control,
            grant: None,
            pointer: (0, 0, false),
            dirty: false,
            next_seq: 1,
            pending: VecDeque::new(),
            started_us: now_us,
            last_send_us: i64::MIN / 2,
        }
    }

    pub fn state(&self, now_us: i64) -> ControlState {
        let silent = now_us - self.started_us > UNSUPPORTED_AFTER_US;
        match (self.wants, self.grant) {
            (_, Some(GrantState::Denied)) => ControlState::Denied,
            (true, Some(GrantState::Granted)) => ControlState::Granted,
            (true, Some(GrantState::Pending)) => ControlState::Requested,
            (false, Some(GrantState::Granted)) => ControlState::Allowed,
            (false, Some(GrantState::Pending)) => ControlState::Offered,
            (_, None) if silent => ControlState::Unsupported,
            (true, None) => ControlState::Requested,
            (false, None) => ControlState::Off,
        }
    }

    pub fn granted(&self) -> bool {
        self.wants && self.grant == Some(GrantState::Granted)
    }

    pub fn wants(&self) -> bool {
        self.wants
    }

    /// Asks for control (true) or gives it back (false), from the next datagram on. Giving it back drops what
    /// was not sent yet; the host lets go of anything still held down.
    pub fn set_wants(&mut self, wants: bool) {
        if wants == self.wants {
            return;
        }
        self.wants = wants;
        if !wants {
            self.pending.clear();
        }
        self.dirty = true;
    }

    /// The pointer, as fractions of the picture (0,0 = top left). `inside` is false over the black bars or
    /// outside the window; the host then leaves its pointer where it is.
    pub fn pointer(&mut self, x: f64, y: f64, inside: bool) {
        let to_u16 = |v: f64| (v.clamp(0.0, 1.0) * 65535.0).round() as u16;
        let next = (to_u16(x), to_u16(y), inside);
        if next != self.pointer {
            self.pointer = next;
            self.dirty = true;
        }
    }

    /// One `key-binding` client message from mpv: `state` is its second argument ("d-", "u-", "r-", "p-", with
    /// "m" in place of "-" for mouse buttons), `key_name` the third ("Ctrl+a", "MBTN_LEFT").
    pub fn key_binding(&mut self, state: &str, key_name: &str) {
        if !self.granted() {
            return;
        }
        let Some((mods, input)) = parse_mpv_key(key_name) else { return };
        let phase = state.as_bytes().first().copied().unwrap_or(b'p');
        match input {
            Input::Wheel { axis, direction } => {
                if phase != b'u' {
                    self.push(Event { kind: EventKind::Wheel, mods: mods.0, code: axis as u32, arg: direction as i16 * 120 });
                }
            }
            Input::Button(button) => match phase {
                b'd' => self.push(Event { kind: EventKind::ButtonDown, mods: mods.0, code: button as u32, arg: 0 }),
                b'u' => self.push(Event { kind: EventKind::ButtonUp, mods: mods.0, code: button as u32, arg: 0 }),
                b'p' => {
                    self.push(Event { kind: EventKind::ButtonDown, mods: mods.0, code: button as u32, arg: 0 });
                    self.push(Event { kind: EventKind::ButtonUp, mods: mods.0, code: button as u32, arg: 0 });
                }
                _ => {}
            },
            Input::Key(key) => match phase {
                // A repeat is another key-down: injected keys do not auto-repeat on the host by themselves.
                b'd' | b'r' => self.push(Event { kind: EventKind::KeyDown, mods: mods.0, code: key.code(), arg: 0 }),
                b'u' => self.push(Event { kind: EventKind::KeyUp, mods: mods.0, code: key.code(), arg: 0 }),
                _ => {
                    self.push(Event { kind: EventKind::KeyDown, mods: mods.0, code: key.code(), arg: 0 });
                    self.push(Event { kind: EventKind::KeyUp, mods: mods.0, code: key.code(), arg: 0 });
                }
            },
        }
    }

    fn push(&mut self, event: Event) {
        if self.pending.len() >= MAX_PENDING {
            self.pending.pop_front();
        }
        self.pending.push_back((self.next_seq, event));
        self.next_seq = self.next_seq.wrapping_add(1).max(1);
        self.dirty = true;
    }

    /// A datagram from the host's side of the channel.
    pub fn on_datagram(&mut self, datagram: &[u8]) {
        let Some(plain) = self.channel.open(datagram) else { return };
        let Some(msg) = HostMessage::decode(&plain) else { return };
        let was_granted = self.granted();
        self.grant = Some(msg.state);
        while self.pending.front().is_some_and(|(seq, _)| *seq <= msg.acked_seq) {
            self.pending.pop_front();
        }
        if was_granted && !self.granted() {
            self.pending.clear();
        }
    }

    /// The datagram to send now, if one is due. Call every few milliseconds.
    pub fn poll(&mut self, now_us: i64) -> Option<Vec<u8>> {
        let since = now_us - self.last_send_us;
        let due = (self.dirty && since >= MOVE_INTERVAL_US)
            || (!self.pending.is_empty() && since >= RESEND_INTERVAL_US)
            || since >= HEARTBEAT_US;
        if !due {
            return None;
        }
        self.dirty = false;
        self.last_send_us = now_us;
        let events: Vec<Event> = self.pending.iter().take(MAX_EVENTS_PER_PACKET).map(|(_, e)| *e).collect();
        let msg = ViewerMessage {
            wants_control: self.wants,
            pointer_x: self.pointer.0,
            pointer_y: self.pointer.1,
            pointer_inside: self.pointer.2 && self.granted(),
            first_seq: self.pending.front().map_or(self.next_seq, |(seq, _)| *seq),
            events,
        };
        Some(self.channel.seal(&msg.encode()))
    }
}
