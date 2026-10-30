//! Which of the host's displays is shared, chosen by the viewer who controls it (docs/design.md §9.2). The host
//! tells a viewer in control which displays it has and which one it shares; the viewer asks it to share another
//! one, and the host does as if its own user had picked that display on the Share page.
//!
//! Two kinds of datagram on the side channel, sealed like remote control's (the same key, so only the viewer of the
//! session reads or sends them), with their kind as the direction byte, so that no nonce is used twice:
//!
//! ```text
//!  0  1   kind     'D' the host's list, host -> viewer; 'd' a pick, viewer -> host
//!  1  8   counter  big-endian, strictly increasing per direction
//!  9  ..  ChaCha20-Poly1305(key = controlKey(sid), nonce = kind || 0 0 0 || counter, aad = "HDRCCTL1")
//! ```
//!
//! The list: version 1, the number of the last pick answered (u32, 0 = none), flags (bit 0 = that pick was refused:
//! no such display), the id of the display shared (0xff = none of these), how many displays follow (at most 8),
//! and per display its id (the host's own number for it), width, height and refresh rate in Hz (u16 each), flags
//! (bit 0 = in HDR mode), the length of its name and the name (UTF-8, at most 48 bytes). A pick: version 1, its
//! number (u32, from 1 in each session) and the id of the display wanted.
//!
//! The host sends the list to a viewer in control whenever it changes and every second besides; a viewer sends its
//! pick again every 250 ms until the list answers it. Hosts before 0.3.9 never send a list; viewers before 0.3.9
//! cannot open one (it is not sealed in either of their directions) and drop it.

use crate::wire::Channel;

/// The host's list, host -> viewer.
pub const LIST: u8 = b'D';
/// A pick, viewer -> host.
pub const PICK: u8 = b'd';
/// The most displays a list names (the datagram stays far below any path MTU).
pub const MAX_DISPLAYS: usize = 8;
const MAX_NAME: usize = 48;
const VERSION: u8 = 1;
const NONE: u8 = 0xff;
/// The host says it all again this often while nothing changes (datagrams get lost).
const LIST_EVERY_US: i64 = 1_000_000;
/// A pick is sent again this often until the host answers it.
const PICK_EVERY_US: i64 = 250_000;
/// A pick the host answered stays "switching" until the host shares that display, at most this long (the host
/// starts its capture again for it, which takes a second or two).
const PICK_SHOWN_US: i64 = 8_000_000;

/// The datagram is about the shared display (a list or a pick), whatever else is wrong with it.
pub fn is_display_datagram(payload: &[u8]) -> bool {
    matches!(payload.first(), Some(&LIST) | Some(&PICK))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    /// The host's own number for it.
    pub id: u8,
    pub name: String,
    pub width: u16,
    pub height: u16,
    pub refresh_hz: u16,
    pub hdr: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DisplayList {
    /// The number of the last pick the host answered (0 = none yet).
    pub answered: u32,
    /// The host could not do that pick: it has no such display (any more).
    pub refused: bool,
    /// The display being shared, when it is one of `displays`.
    pub shared: Option<u8>,
    pub displays: Vec<Display>,
}

/// The longest start of `name` that fits in `max` bytes, cut between characters.
fn clipped(name: &str, max: usize) -> &str {
    let mut end = name.len().min(max);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

impl DisplayList {
    pub fn encode(&self) -> Vec<u8> {
        let displays = &self.displays[..self.displays.len().min(MAX_DISPLAYS)];
        let mut out = Vec::with_capacity(8 + displays.len() * 24);
        out.push(VERSION);
        out.extend_from_slice(&self.answered.to_be_bytes());
        out.push(self.refused as u8);
        out.push(self.shared.unwrap_or(NONE));
        out.push(displays.len() as u8);
        for d in displays {
            out.push(d.id);
            for n in [d.width, d.height, d.refresh_hz] {
                out.extend_from_slice(&n.to_be_bytes());
            }
            out.push(d.hdr as u8);
            let name = clipped(&d.name, MAX_NAME);
            out.push(name.len() as u8);
            out.extend_from_slice(name.as_bytes());
        }
        out
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < 8 || b[0] != VERSION || b[7] as usize > MAX_DISPLAYS {
            return None;
        }
        let mut displays = Vec::with_capacity(b[7] as usize);
        let mut rest = &b[8..];
        for _ in 0..b[7] {
            let (head, tail) = rest.split_at_checked(9)?;
            let length = head[8] as usize;
            if length > MAX_NAME {
                return None;
            }
            let (name, tail) = tail.split_at_checked(length)?;
            let u16_at = |i: usize| u16::from_be_bytes([head[i], head[i + 1]]);
            displays.push(Display {
                id: head[0],
                name: std::str::from_utf8(name).ok()?.to_string(),
                width: u16_at(1),
                height: u16_at(3),
                refresh_hz: u16_at(5),
                hdr: head[7] & 1 != 0,
            });
            rest = tail;
        }
        if !rest.is_empty() {
            return None;
        }
        Some(Self {
            answered: u32::from_be_bytes([b[1], b[2], b[3], b[4]]),
            refused: b[5] & 1 != 0,
            shared: (b[6] != NONE).then_some(b[6]),
            displays,
        })
    }
}

fn encode_pick(seq: u32, id: u8) -> Vec<u8> {
    let mut out = vec![VERSION];
    out.extend_from_slice(&seq.to_be_bytes());
    out.push(id);
    out
}

fn decode_pick(b: &[u8]) -> Option<(u32, u8)> {
    if b.len() != 6 || b[0] != VERSION {
        return None;
    }
    Some((u32::from_be_bytes([b[1], b[2], b[3], b[4]]), b[5]))
}

/// The host's half, one per viewer session.
pub struct HostDisplays {
    channel: Channel,
    /// What the viewer was told last, and when.
    sent: Option<(Vec<u8>, i64)>,
    answered: u32,
    refused: bool,
}

impl HostDisplays {
    /// `key` is the session's control key.
    pub fn new(key: &[u8; 32]) -> Self {
        Self { channel: Channel::displays_host(key), sent: None, answered: 0, refused: false }
    }

    /// The list to send now, if one is due: `displays` as they are, `shared` the one being shared.
    pub fn poll(&mut self, displays: &[Display], shared: Option<u8>, now_us: i64) -> Option<Vec<u8>> {
        let list = DisplayList { answered: self.answered, refused: self.refused, shared, displays: displays.to_vec() };
        let plain = list.encode();
        let due = self.sent.as_ref().is_none_or(|(last, at)| *last != plain || now_us - at >= LIST_EVERY_US);
        if !due {
            return None;
        }
        let datagram = self.channel.seal(&plain);
        self.sent = Some((plain, now_us));
        Some(datagram)
    }

    /// A pick from the viewer: the display it wants shared, the first time that pick arrives (copies of it are
    /// answered, not acted on again). The pick counts as done until [`HostDisplays::refuse`] says otherwise.
    pub fn on_datagram(&mut self, datagram: &[u8]) -> Option<u8> {
        let (seq, id) = decode_pick(&self.channel.open(datagram)?)?;
        if seq <= self.answered {
            return None;
        }
        self.answered = seq;
        self.refused = false;
        Some(id)
    }

    /// The pick just taken cannot be done.
    pub fn refuse(&mut self) {
        self.refused = true;
    }

    /// The viewer no longer controls: it is told everything again (at once) when it does next time.
    pub fn reset(&mut self) {
        self.sent = None;
    }
}

struct Pending {
    seq: u32,
    id: u8,
    since_us: i64,
    sent_us: i64,
    answered: bool,
}

/// The viewer's half, one per session.
pub struct ViewerDisplays {
    channel: Channel,
    list: Option<DisplayList>,
    next_seq: u32,
    pending: Option<Pending>,
}

impl ViewerDisplays {
    /// `key` is the session's control key.
    pub fn new(key: &[u8; 32]) -> Self {
        Self { channel: Channel::displays_viewer(key), list: None, next_seq: 1, pending: None }
    }

    /// A datagram from the host's side.
    pub fn on_datagram(&mut self, datagram: &[u8], now_us: i64) {
        let Some(list) = self.channel.open(datagram).as_deref().and_then(DisplayList::decode) else { return };
        if let Some(p) = &mut self.pending {
            p.answered |= list.answered >= p.seq;
            // shared now, refused, or nothing came of it
            if (p.answered && (list.refused || list.shared == Some(p.id))) || now_us - p.since_us > PICK_SHOWN_US {
                self.pending = None;
            }
        }
        self.list = Some(list);
    }

    /// The user wants display `id` shared: asked from the next [`ViewerDisplays::poll`] on. False when the host's
    /// list has no such display (or there is no list).
    pub fn pick(&mut self, id: u8, now_us: i64) -> bool {
        if !self.list.as_ref().is_some_and(|l| l.displays.iter().any(|d| d.id == id)) {
            return false;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending = Some(Pending { seq, id, since_us: now_us, sent_us: i64::MIN / 2, answered: false });
        true
    }

    /// The datagram to send now, if one is due. Call every few milliseconds.
    pub fn poll(&mut self, now_us: i64) -> Option<Vec<u8>> {
        let p = self.pending.as_mut()?;
        if now_us - p.since_us > PICK_SHOWN_US {
            self.pending = None;
            return None;
        }
        if p.answered || now_us - p.sent_us < PICK_EVERY_US {
            return None;
        }
        p.sent_us = now_us;
        let plain = encode_pick(p.seq, p.id);
        Some(self.channel.seal(&plain))
    }

    /// What the host said last.
    pub fn list(&self) -> Option<&DisplayList> {
        self.list.as_ref()
    }

    /// The display asked for that the host does not share yet.
    pub fn pending(&self) -> Option<u8> {
        self.pending.as_ref().map(|p| p.id)
    }

    /// Control ended: the host stops sending, so what it said no longer holds.
    pub fn forget(&mut self) {
        self.list = None;
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [5; 32];

    fn displays() -> Vec<Display> {
        vec![
            Display { id: 0, name: "AW3225QF".into(), width: 3840, height: 2160, refresh_hz: 240, hdr: true },
            Display { id: 1, name: "AW2523HF".into(), width: 1920, height: 1080, refresh_hz: 360, hdr: false },
            Display { id: 4, name: "Living Room TV".into(), width: 1920, height: 1080, refresh_hz: 60, hdr: false },
        ]
    }

    #[test]
    fn lists_and_picks_survive_the_round_trip() {
        let list = DisplayList { answered: 7, refused: true, shared: Some(4), displays: displays() };
        assert_eq!(DisplayList::decode(&list.encode()), Some(list));
        let none = DisplayList { shared: None, ..DisplayList::default() };
        assert_eq!(DisplayList::decode(&none.encode()), Some(none));
        assert_eq!(decode_pick(&encode_pick(9, 4)), Some((9, 4)));
    }

    #[test]
    fn long_names_are_cut_between_characters_and_malformed_lists_are_refused() {
        let long = Display { name: "显示器".repeat(10), ..displays()[0].clone() };
        let list = DisplayList { displays: vec![long], ..DisplayList::default() };
        let back = DisplayList::decode(&list.encode()).unwrap();
        assert_eq!(back.displays[0].name, "显示器".repeat(5) + "显", "48 bytes are 16 of these characters");
        let b = DisplayList { displays: displays(), ..DisplayList::default() }.encode();
        assert!(DisplayList::decode(&b[..b.len() - 1]).is_none(), "truncated");
        assert!(DisplayList::decode(&[b.as_slice(), &[0]].concat()).is_none(), "trailing bytes");
        let mut many = b.clone();
        many[7] = 9;
        assert!(DisplayList::decode(&many).is_none(), "more than eight");
        assert!(decode_pick(&[2, 0, 0, 0, 1, 0]).is_none(), "unknown version");
        let nine = (0..9).map(|i| Display { id: i, ..displays()[1].clone() }).collect();
        let clipped = DisplayList::decode(&DisplayList { displays: nine, ..DisplayList::default() }.encode()).unwrap();
        assert_eq!(clipped.displays.len(), MAX_DISPLAYS, "a list names eight at most");
    }

    #[test]
    fn a_pick_reaches_the_host_once_and_shows_until_the_host_shares_that_display() {
        let mut host = HostDisplays::new(&KEY);
        let mut viewer = ViewerDisplays::new(&KEY);
        let mut now = 0;
        assert!(!viewer.pick(1, now), "no list yet");
        let d = host.poll(&displays(), Some(0), now).unwrap();
        assert_eq!(d[0], LIST);
        viewer.on_datagram(&d, now);
        assert_eq!(viewer.list().unwrap().shared, Some(0));
        assert!(host.poll(&displays(), Some(0), now + 10_000).is_none(), "nothing new to say");
        assert!(!viewer.pick(3, now), "no such display");

        assert!(viewer.pick(1, now));
        let pick = viewer.poll(now).unwrap();
        assert_eq!(pick[0], PICK);
        assert!(viewer.poll(now + 100_000).is_none(), "not again so soon");
        let again = viewer.poll(now + 250_000).unwrap();
        assert_eq!(host.on_datagram(&pick), Some(1));
        assert_eq!(host.on_datagram(&again), None, "a copy of the same pick");
        now += 300_000;
        // answered at once, but the host shares display 1 only once its capture has started again
        viewer.on_datagram(&host.poll(&displays(), Some(0), now).unwrap(), now);
        assert_eq!(viewer.pending(), Some(1));
        assert!(viewer.poll(now + 500_000).is_none(), "answered: no more copies");
        now += 1_000_000;
        viewer.on_datagram(&host.poll(&displays(), Some(1), now).unwrap(), now);
        assert_eq!(viewer.pending(), None);
        assert_eq!(viewer.list().unwrap().shared, Some(1));

        // a display that went away meanwhile: refused, and the viewer stops waiting
        assert!(viewer.pick(4, now));
        let pick = viewer.poll(now).unwrap();
        assert_eq!(host.on_datagram(&pick), Some(4));
        host.refuse();
        let fewer = &displays()[..2];
        viewer.on_datagram(&host.poll(fewer, Some(1), now).unwrap(), now);
        assert_eq!(viewer.pending(), None);
        assert!(viewer.list().unwrap().refused);
    }

    #[test]
    fn the_list_is_said_again_and_strangers_are_not_heard() {
        let mut host = HostDisplays::new(&KEY);
        assert!(host.poll(&displays(), None, 0).is_some());
        assert!(host.poll(&displays(), None, 999_999).is_none());
        assert!(host.poll(&displays(), None, 1_000_000).is_some(), "every second");
        host.reset();
        assert!(host.poll(&displays(), None, 1_000_001).is_some(), "at once after control was taken again");

        let mut stranger = ViewerDisplays::new(&[6; 32]);
        stranger.list = Some(DisplayList { displays: displays(), ..DisplayList::default() });
        assert!(stranger.pick(0, 0));
        assert_eq!(host.on_datagram(&stranger.poll(0).unwrap()), None);
        // nor its own direction, nor remote control's datagrams
        let mut viewer = ViewerDisplays::new(&KEY);
        let own = Channel::displays_viewer(&KEY).seal(&encode_pick(1, 0));
        viewer.on_datagram(&own, 0);
        assert!(viewer.list().is_none());
        let control = Channel::host(&KEY).seal(&DisplayList::default().encode());
        viewer.on_datagram(&control, 0);
        assert!(viewer.list().is_none());
        assert!(!is_display_datagram(&control) && is_display_datagram(&own));
    }

    #[test]
    fn a_pick_nobody_answers_is_given_up() {
        let mut host = HostDisplays::new(&KEY);
        let mut viewer = ViewerDisplays::new(&KEY);
        viewer.on_datagram(&host.poll(&displays(), Some(0), 0).unwrap(), 0);
        assert!(viewer.pick(1, 0));
        assert!(viewer.poll(0).is_some());
        assert!(viewer.poll(PICK_SHOWN_US + 1).is_none());
        assert_eq!(viewer.pending(), None);
    }
}
