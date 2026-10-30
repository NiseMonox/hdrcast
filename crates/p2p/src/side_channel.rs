//! The side channel (docs/design.md §8.9): small datagrams of another part of the app (remote control) that
//! travel next to the SRT packets on the punched path.
//!
//! "HDRCSIDE" (8 bytes, ASCII) | sid (16 raw bytes, as in control packets) | payload (1..=1200 bytes)
//!
//! The link does not look inside the payload and does not authenticate it: whoever owns the channel seals it
//! (remote control uses `GroupKeys::control_key(sid)`). What the link does guarantee is where it came from, in
//! the same sense as for SRT data: a live session's id, from an address that session confirmed by punching.
//!
//! A peer older than this version knows nothing of the format and would hand such a datagram to SRT, so one is
//! only ever sent to a peer that announced it understands them ("rc" in the live record / the join).

use hdrcast_core::base64url;
use hdrcast_core::group::keys::ID_BYTES;

pub const MAGIC: &[u8; 8] = b"HDRCSIDE";
pub const HEADER_LENGTH: usize = 8 + ID_BYTES;
pub const MIN_PAYLOAD: usize = 1;
/// With the 24-byte header this stays below the 1316-byte SRT payload the path is known to carry.
pub const MAX_PAYLOAD: usize = 1200;

/// True for a payload size the channel carries.
pub fn is_valid_payload(payload: &[u8]) -> bool {
    (MIN_PAYLOAD..=MAX_PAYLOAD).contains(&payload.len())
}

/// The datagram for `payload`, or `None` when the payload is empty or longer than [`MAX_PAYLOAD`].
pub fn build(sid: &[u8; ID_BYTES], payload: &[u8]) -> Option<Vec<u8>> {
    if !is_valid_payload(payload) {
        return None;
    }
    let mut datagram = Vec::with_capacity(HEADER_LENGTH + payload.len());
    datagram.extend_from_slice(MAGIC);
    datagram.extend_from_slice(sid);
    datagram.extend_from_slice(payload);
    Some(datagram)
}

/// [`build`] into `datagram` (replacing what was there), so a busy sender reuses one buffer. False, with
/// `datagram` untouched, for a payload out of range.
pub fn build_into(sid: &[u8; ID_BYTES], payload: &[u8], datagram: &mut Vec<u8>) -> bool {
    if !is_valid_payload(payload) {
        return false;
    }
    datagram.clear();
    datagram.extend_from_slice(MAGIC);
    datagram.extend_from_slice(sid);
    datagram.extend_from_slice(payload);
    true
}

/// The datagram starts with the side channel's magic. Such a datagram is never SRT, whatever else is wrong
/// with it: it is either delivered to the side handler or dropped.
pub fn has_magic(datagram: &[u8]) -> bool {
    datagram.len() >= MAGIC.len() && datagram[..MAGIC.len()] == MAGIC[..]
}

/// The session id (22 characters, as everywhere else) and the payload of a well-formed side datagram.
pub fn parse(datagram: &[u8]) -> Option<(String, &[u8])> {
    if !has_magic(datagram) || datagram.len() < HEADER_LENGTH {
        return None;
    }
    let payload = &datagram[HEADER_LENGTH..];
    if !is_valid_payload(payload) {
        return None;
    }
    Some((base64url::encode(&datagram[MAGIC.len()..HEADER_LENGTH]), payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{control_packet, echo_protocol};

    #[test]
    fn the_datagram_is_magic_sid_payload() {
        let sid: [u8; 16] = *b"0123456789abcdef";
        let datagram = build(&sid, b"hi").unwrap();
        assert_eq!(datagram, b"HDRCSIDE0123456789abcdefhi");
        let (id, payload) = parse(&datagram).unwrap();
        assert_eq!(id, base64url::encode(&sid));
        assert_eq!(id.len(), 22);
        assert_eq!(payload, b"hi");
        let mut reused = b"old".to_vec();
        assert!(build_into(&sid, b"hi", &mut reused));
        assert_eq!(reused, datagram);
        assert!(!build_into(&sid, b"", &mut reused));
        assert_eq!(reused, datagram, "untouched");
    }

    #[test]
    fn payloads_are_1_to_1200_bytes() {
        let sid = [1u8; 16];
        assert_eq!(build(&sid, b""), None);
        assert_eq!(build(&sid, &[0u8; MAX_PAYLOAD + 1]), None);
        assert_eq!(build(&sid, &[0u8; 1]).unwrap().len(), HEADER_LENGTH + 1);
        let largest = build(&sid, &[7u8; MAX_PAYLOAD]).unwrap();
        assert_eq!(largest.len(), 1224);
        assert_eq!(parse(&largest).unwrap().1.len(), MAX_PAYLOAD);

        // malformed, but still the side channel's: recognised by the magic and never taken for SRT
        let mut too_long = largest.clone();
        too_long.push(0);
        for bad in [&largest[..HEADER_LENGTH], &largest[..12], &largest[..8], &too_long[..]] {
            assert!(has_magic(bad));
            assert_eq!(parse(bad), None, "{} bytes", bad.len());
        }
        assert!(!has_magic(b"HDRCSID"));
        assert!(!has_magic(&[0u8; 1316]));
        assert_eq!(parse(&[0u8; 1316]), None);
    }

    #[test]
    fn the_three_formats_on_the_socket_cannot_be_confused() {
        let side = build(&[2u8; 16], &[0u8; 34]).unwrap(); // 58 bytes, a control packet's length
        assert_eq!(side.len(), control_packet::LENGTH);
        assert!(!control_packet::looks_like_control(&side));
        assert!(!echo_protocol::looks_like_reply(&build(&[2u8; 16], &[0u8; 7]).unwrap())); // 31 bytes, an echo reply's
        let control = control_packet::build(control_packet::ControlType::Punch, control_packet::PeerRole::Host, &[2u8; 16], 1, 0, &[3u8; 32]);
        assert!(!has_magic(&control));
        assert!(!has_magic(&echo_protocol::build_reply(&[4u8; 16], "203.0.113.9:1".parse().unwrap())));
    }
}
