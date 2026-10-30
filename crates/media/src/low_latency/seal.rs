//! Sealing control mode's datagrams: ChaCha20-Poly1305, a counter per direction as the nonce, and a window of the
//! counters seen lately so that nothing is opened twice (see the module above for the layout).

use chacha20poly1305::aead::{AeadInOut, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce, Tag};

/// The kind byte and the counter.
pub const HEADER_BYTES: usize = 9;
pub const TAG_BYTES: usize = 16;
/// What sealing adds to a message.
pub const OVERHEAD: usize = HEADER_BYTES + TAG_BYTES;

/// Authenticated with every datagram, so a datagram of another protocol under the same key could never pass.
const AAD_PREFIX: &[u8] = b"HDRCLL1";

/// Which way a datagram goes: part of the nonce, so the two directions never share one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ToHost = 1,
    ToViewer = 2,
}

fn nonce(direction: Direction, counter: u64) -> Nonce {
    let mut bytes = [0u8; 12];
    bytes[0] = direction as u8;
    bytes[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::from(bytes)
}

fn aad(header: &[u8]) -> [u8; AAD_PREFIX.len() + HEADER_BYTES] {
    let mut aad = [0u8; AAD_PREFIX.len() + HEADER_BYTES];
    aad[..AAD_PREFIX.len()].copy_from_slice(AAD_PREFIX);
    aad[AAD_PREFIX.len()..].copy_from_slice(&header[..HEADER_BYTES]);
    aad
}

/// Seals what one side sends.
pub struct Sealer {
    cipher: ChaCha20Poly1305,
    direction: Direction,
    counter: u64,
}

impl Sealer {
    pub fn new(key: &[u8; 32], direction: Direction) -> Self {
        Self { cipher: ChaCha20Poly1305::new(&(*key).into()), direction, counter: 0 }
    }

    /// Writes the datagram for `message` of `kind` into `out` (replacing what was there).
    pub fn seal(&mut self, kind: u8, message: &[u8], out: &mut Vec<u8>) {
        self.counter += 1;
        out.clear();
        out.push(kind);
        out.extend_from_slice(&self.counter.to_be_bytes());
        out.extend_from_slice(message);
        let aad = aad(out);
        let tag = self
            .cipher
            .encrypt_inout_detached(&nonce(self.direction, self.counter), &aad, (&mut out[HEADER_BYTES..]).into())
            .expect("sealing a datagram cannot fail");
        out.extend_from_slice(&tag);
    }
}

/// Opens what the other side sent: only authentic datagrams of its direction, each once.
pub struct Opener {
    cipher: ChaCha20Poly1305,
    direction: Direction,
    window: ReplayWindow,
    plain: Vec<u8>,
    refused: u64,
}

impl Opener {
    /// `direction`: the direction of what this side receives.
    pub fn new(key: &[u8; 32], direction: Direction) -> Self {
        Self { cipher: ChaCha20Poly1305::new(&(*key).into()), direction, window: ReplayWindow::default(), plain: Vec::new(), refused: 0 }
    }

    /// The kind and the message of an authentic datagram not opened before; `None` for anything else (forged,
    /// sealed for the other direction or another session, replayed, or older than the window).
    pub fn open(&mut self, datagram: &[u8]) -> Option<(u8, &[u8])> {
        if datagram.len() < OVERHEAD {
            self.refused += 1;
            return None;
        }
        let counter = u64::from_be_bytes(datagram[1..HEADER_BYTES].try_into().expect("8 bytes"));
        if !self.window.fresh(counter) {
            self.refused += 1;
            return None;
        }
        let (sealed, tag) = datagram[HEADER_BYTES..].split_at(datagram.len() - OVERHEAD);
        self.plain.clear();
        self.plain.extend_from_slice(sealed);
        let tag = Tag::try_from(tag).expect("16 bytes");
        let aad = aad(datagram);
        if self.cipher.decrypt_inout_detached(&nonce(self.direction, counter), &aad, (&mut self.plain[..]).into(), &tag).is_err() {
            self.refused += 1;
            return None;
        }
        self.window.mark(counter);
        Some((datagram[0], &self.plain))
    }

    /// Datagrams refused so far.
    pub fn refused(&self) -> u64 {
        self.refused
    }
}

/// The counters seen among the last [`ReplayWindow::SIZE`] below the highest one; anything older is refused.
#[derive(Clone)]
struct ReplayWindow {
    top: u64,
    seen: [u64; ReplayWindow::SIZE / 64],
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self { top: 0, seen: [0; ReplayWindow::SIZE / 64] }
    }
}

impl ReplayWindow {
    /// About a second of a busy stream: packets that come later than that are no use anyway.
    const SIZE: usize = 8192;

    fn bit(counter: u64) -> (usize, u64) {
        let at = (counter % Self::SIZE as u64) as usize;
        (at / 64, 1 << (at % 64))
    }

    fn fresh(&self, counter: u64) -> bool {
        if counter == 0 {
            return false;
        }
        if counter > self.top {
            return true;
        }
        if self.top - counter >= Self::SIZE as u64 {
            return false;
        }
        let (word, mask) = Self::bit(counter);
        self.seen[word] & mask == 0
    }

    fn mark(&mut self, counter: u64) {
        if counter > self.top {
            if counter - self.top >= Self::SIZE as u64 {
                self.seen = [0; Self::SIZE / 64];
            } else {
                for skipped in self.top + 1..counter {
                    let (word, mask) = Self::bit(skipped);
                    self.seen[word] &= !mask;
                }
            }
            self.top = counter;
        }
        let (word, mask) = Self::bit(counter);
        self.seen[word] |= mask;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_datagram_opens_once_on_the_other_side_and_nowhere_else() {
        let key = [7u8; 32];
        let mut host = Sealer::new(&key, Direction::ToViewer);
        let mut viewer = Opener::new(&key, Direction::ToViewer);
        let mut datagram = Vec::new();
        host.seal(b'V', b"a picture", &mut datagram);
        assert_eq!(datagram.len(), b"a picture".len() + OVERHEAD);
        assert_eq!(datagram[0], b'V');
        assert_eq!(&datagram[1..9], &1u64.to_be_bytes());
        assert!(!datagram.windows(9).any(|w| w == b"a picture"), "the message is not in the clear");
        assert_eq!(viewer.open(&datagram), Some((b'V', &b"a picture"[..])));
        assert_eq!(viewer.open(&datagram), None, "replayed");

        // another key, the other direction, a changed kind / counter / byte: refused
        let mut other = Opener::new(&[8u8; 32], Direction::ToViewer);
        let mut backwards = Opener::new(&key, Direction::ToHost);
        host.seal(b'V', b"two", &mut datagram);
        assert_eq!(other.open(&datagram), None);
        assert_eq!(backwards.open(&datagram), None);
        for at in [0, 3, 9, datagram.len() - 1] {
            let mut bent = datagram.clone();
            bent[at] ^= 1;
            assert_eq!(viewer.open(&bent), None, "byte {at}");
        }
        assert_eq!(viewer.open(&datagram[..datagram.len() - 1]), None);
        assert_eq!(viewer.open(&datagram), Some((b'V', &b"two"[..])), "the real one still opens");
        assert_eq!(viewer.refused(), 6);
        // an empty message is fine
        host.seal(b'r', b"", &mut datagram);
        assert_eq!(viewer.open(&datagram), Some((b'r', &b""[..])));
    }

    #[test]
    fn late_datagrams_open_inside_the_window_and_old_ones_do_not() {
        let key = [1u8; 32];
        let mut host = Sealer::new(&key, Direction::ToViewer);
        let mut viewer = Opener::new(&key, Direction::ToViewer);
        let datagrams: Vec<Vec<u8>> = (0..ReplayWindow::SIZE + 10)
            .map(|i| {
                let mut d = Vec::new();
                host.seal(b'A', &(i as u32).to_be_bytes(), &mut d);
                d
            })
            .collect();
        // 3 overtakes 2: both open once
        assert!(viewer.open(&datagrams[0]).is_some());
        assert!(viewer.open(&datagrams[2]).is_some());
        assert!(viewer.open(&datagrams[1]).is_some());
        assert!(viewer.open(&datagrams[1]).is_none());
        // far ahead: what lies a whole window behind is refused, what lies inside it still opens once
        assert!(viewer.open(&datagrams[ReplayWindow::SIZE + 5]).is_some());
        assert!(viewer.open(&datagrams[4]).is_none(), "a whole window old");
        assert!(viewer.open(&datagrams[ReplayWindow::SIZE]).is_some());
        assert!(viewer.open(&datagrams[ReplayWindow::SIZE]).is_none());
        assert!(viewer.open(&datagrams[ReplayWindow::SIZE + 9]).is_some());
    }
}
