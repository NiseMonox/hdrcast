//! Everything derived from the 32-byte group key K (docs/design.md §8.3):
//! sub(label) = HMAC-SHA256(K, "hdrcast/v1/" + label). The signaling server only ever learns the server key, so
//! it can authenticate requests but cannot forge what peers sign with the message key, and it never sees the SRT
//! passphrase.

use std::fmt;

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::common::base64url;

pub const KEY_LENGTH: usize = 32;
pub const ID_BYTES: usize = 16;
pub const ID_LENGTH: usize = 22;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the group key must be 32 bytes")]
pub struct GroupKeyLengthError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a 22-character id")]
pub struct InvalidIdError;

#[derive(Clone, PartialEq, Eq)]
pub struct GroupKeys {
    group_key: [u8; KEY_LENGTH],
    server_key: [u8; 32],
    msg_key: [u8; 32],
    srt_passphrase: String,
}

impl GroupKeys {
    pub fn new(group_key: &[u8]) -> Result<GroupKeys, GroupKeyLengthError> {
        let group_key: [u8; KEY_LENGTH] = group_key.try_into().map_err(|_| GroupKeyLengthError)?;
        let mut srt_passphrase = base64url::encode(&sub(&group_key, "srt"));
        srt_passphrase.truncate(32);
        Ok(GroupKeys {
            group_key,
            server_key: sub(&group_key, "server"),
            msg_key: sub(&group_key, "msg"),
            srt_passphrase,
        })
    }

    pub fn group_key(&self) -> &[u8; KEY_LENGTH] {
        &self.group_key
    }

    /// Authenticates requests to the signaling server (the server stores it).
    pub fn server_key(&self) -> &[u8; 32] {
        &self.server_key
    }

    /// Signs live records, join requests and answers (the server does not have it).
    pub fn msg_key(&self) -> &[u8; 32] {
        &self.msg_key
    }

    /// SRT AES-256 passphrase (32 characters, pbkeylen=32), identical for every member.
    pub fn srt_passphrase(&self) -> &str {
        &self.srt_passphrase
    }

    /// Per-session key for the UDP punch packets.
    pub fn punch_key(&self, sid: &str) -> [u8; 32] {
        self.sub(&format!("punch/{sid}"))
    }

    /// Per-session key for what remote control sends over the side channel (docs/design.md §8.9).
    pub fn control_key(&self, sid: &str) -> [u8; 32] {
        self.sub(&format!("ctl/{sid}"))
    }

    pub fn sub(&self, label: &str) -> [u8; 32] {
        sub(&self.group_key, label)
    }
}

/// Keys never reach a log through `{:?}`.
impl fmt::Debug for GroupKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GroupKeys(***)")
    }
}

fn sub(group_key: &[u8; KEY_LENGTH], label: &str) -> [u8; 32] {
    hmac_sha256(group_key, format!("hdrcast/v1/{label}").as_bytes())
}

// ---- primitives shared by the P2P code ----------------------------------------------------

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// Constant-time comparison; slices of different lengths are simply unequal.
pub fn fixed_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// `count` bytes from the operating system's random generator.
pub fn random_bytes(count: usize) -> Vec<u8> {
    let mut buf = vec![0u8; count];
    fill_random(&mut buf);
    buf
}

pub fn fill_random(buf: &mut [u8]) {
    // No fallback: an id or a group key made from anything weaker would be worse than not starting.
    getrandom::fill(buf).expect("the operating system's random generator is unavailable");
}

/// A new 22-character id (16 random bytes, base64url): group, peer and session ids.
pub fn new_id() -> String {
    let mut raw = [0u8; ID_BYTES];
    fill_random(&mut raw);
    base64url::encode(&raw)
}

/// 22 base64url characters, which is what the signaling server's `^[A-Za-z0-9_-]{22}$` accepts.
pub fn is_valid_id(id: &str) -> bool {
    id.len() == ID_LENGTH && id.chars().all(base64url::is_alphabet)
}

pub fn id_bytes_of(id: &str) -> Result<[u8; ID_BYTES], InvalidIdError> {
    if !is_valid_id(id) {
        return Err(InvalidIdError);
    }
    base64url::decode(id).and_then(|raw| raw.try_into().ok()).ok_or(InvalidIdError)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_answers() {
        // RFC 4231 test case 2 and the SHA-256 of "abc"
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(hex(&mac), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn comparison_needs_equal_length_and_content() {
        assert!(fixed_time_eq(b"abc", b"abc"));
        assert!(!fixed_time_eq(b"abc", b"abd"));
        assert!(!fixed_time_eq(b"abc", b"abcd"));
        assert!(fixed_time_eq(b"", b""));
    }

    #[test]
    fn ids_are_random_and_decode_to_16_bytes() {
        let (a, b) = (new_id(), new_id());
        assert_ne!(a, b);
        assert_eq!(id_bytes_of(&a).unwrap().len(), ID_BYTES);
        assert_eq!(base64url::encode(&id_bytes_of(&a).unwrap()), a);
        assert_eq!(id_bytes_of("short"), Err(InvalidIdError));
        assert_eq!(id_bytes_of("测试组测试组测试组测"), Err(InvalidIdError));
        assert_ne!(random_bytes(32), random_bytes(32));
    }

    #[test]
    fn the_control_key_is_a_labelled_sub_key_of_its_session() {
        let keys = GroupKeys::new(&[7u8; 32]).unwrap();
        let (a, b) = (new_id(), new_id());
        assert_eq!(keys.control_key(&a), keys.sub(&format!("ctl/{a}")));
        assert_eq!(keys.control_key(&a), hmac_sha256(&[7u8; 32], format!("hdrcast/v1/ctl/{a}").as_bytes()));
        assert_ne!(keys.control_key(&a), keys.control_key(&b));
        assert_ne!(keys.control_key(&a), keys.punch_key(&a));
        assert_ne!(keys.control_key(&a), GroupKeys::new(&[8u8; 32]).unwrap().control_key(&a));
    }

    #[test]
    fn a_key_of_the_wrong_length_is_refused_and_keys_do_not_print() {
        assert_eq!(GroupKeys::new(&[0u8; 31]), Err(GroupKeyLengthError));
        assert_eq!(GroupKeys::new(&[0u8; 33]), Err(GroupKeyLengthError));
        let keys = GroupKeys::new(&[7u8; 32]).unwrap();
        assert_eq!(format!("{keys:?}"), "GroupKeys(***)");
        assert_eq!(keys.srt_passphrase().len(), 32);
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
