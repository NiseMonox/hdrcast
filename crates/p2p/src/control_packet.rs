//! The 58-byte UDP control packet (docs/design.md §8.6):
//! magic "HDRCP2P1" | type | role | sid (16) | ts µs (u64 BE) | echo (u64 BE) | HMAC-SHA256(punchKey, 0..41)[0..16].
//! Everything else on the same socket is SRT (or the side channel) and is not touched here.

use hdrcast_core::base64url;
use hdrcast_core::group::keys::{self, ID_BYTES};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ControlType {
    Punch = 1,
    PunchAck = 2,
    Keepalive = 3,
    Bye = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PeerRole {
    Host = 1,
    Viewer = 2,
}

pub const LENGTH: usize = 58;
pub const MAC_OFFSET: usize = 42;
pub const MAC_LENGTH: usize = 16;
const MAGIC: &[u8; 8] = b"HDRCP2P1";
const SID_OFFSET: usize = 10;
const TS_OFFSET: usize = 26;
const ECHO_OFFSET: usize = 34;

pub fn build(kind: ControlType, role: PeerRole, sid: &[u8; ID_BYTES], ts: u64, echo: u64, punch_key: &[u8]) -> [u8; LENGTH] {
    let mut b = [0u8; LENGTH];
    b[..8].copy_from_slice(MAGIC);
    b[8] = kind as u8;
    b[9] = role as u8;
    b[SID_OFFSET..SID_OFFSET + ID_BYTES].copy_from_slice(sid);
    b[TS_OFFSET..TS_OFFSET + 8].copy_from_slice(&ts.to_be_bytes());
    b[ECHO_OFFSET..ECHO_OFFSET + 8].copy_from_slice(&echo.to_be_bytes());
    let mac = keys::hmac_sha256(punch_key, &b[..MAC_OFFSET]);
    b[MAC_OFFSET..].copy_from_slice(&mac[..MAC_LENGTH]);
    b
}

/// Right length, magic, known type and role. Says nothing about authenticity.
pub fn looks_like_control(datagram: &[u8]) -> bool {
    datagram.len() == LENGTH && datagram[..8] == MAGIC[..] && (1..=4).contains(&datagram[8]) && matches!(datagram[9], 1 | 2)
}

/// `None` unless [`looks_like_control`].
pub fn type_of(datagram: &[u8]) -> Option<ControlType> {
    if !looks_like_control(datagram) {
        return None;
    }
    Some(match datagram[8] {
        1 => ControlType::Punch,
        2 => ControlType::PunchAck,
        3 => ControlType::Keepalive,
        _ => ControlType::Bye,
    })
}

/// `None` unless [`looks_like_control`].
pub fn role_of(datagram: &[u8]) -> Option<PeerRole> {
    if !looks_like_control(datagram) {
        return None;
    }
    Some(if datagram[9] == 1 { PeerRole::Host } else { PeerRole::Viewer })
}

/// The sender's clock when it sent the packet (0 for something that is not a control packet).
pub fn ts_of(datagram: &[u8]) -> u64 {
    read_u64(datagram, TS_OFFSET)
}

/// PUNCH_ACK: the `ts` of the PUNCH it answers; otherwise 0.
pub fn echo_of(datagram: &[u8]) -> u64 {
    read_u64(datagram, ECHO_OFFSET)
}

/// The session id as the 22-character id used everywhere else ("" for something that is not a control packet).
pub fn sid_of(datagram: &[u8]) -> String {
    if datagram.len() != LENGTH {
        return String::new();
    }
    base64url::encode(&datagram[SID_OFFSET..SID_OFFSET + ID_BYTES])
}

pub fn verify_mac(datagram: &[u8], punch_key: &[u8]) -> bool {
    if datagram.len() != LENGTH {
        return false;
    }
    let mac = keys::hmac_sha256(punch_key, &datagram[..MAC_OFFSET]);
    keys::fixed_time_eq(&mac[..MAC_LENGTH], &datagram[MAC_OFFSET..])
}

fn read_u64(datagram: &[u8], offset: usize) -> u64 {
    if datagram.len() != LENGTH {
        return 0;
    }
    u64::from_be_bytes(datagram[offset..offset + 8].try_into().expect("8 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anything_but_a_whole_packet_reads_as_nothing() {
        let key = [9u8; 32];
        let packet = build(ControlType::Keepalive, PeerRole::Host, &[7u8; 16], 5, 0, &key);
        assert_eq!(type_of(&packet), Some(ControlType::Keepalive));
        assert_eq!(role_of(&packet), Some(PeerRole::Host));
        assert_eq!(ts_of(&packet), 5);
        assert!(verify_mac(&packet, &key));

        for short in [&packet[..57], &packet[..0], &[0u8; 59][..]] {
            assert!(!looks_like_control(short));
            assert_eq!(type_of(short), None);
            assert_eq!(role_of(short), None);
            assert_eq!(ts_of(short), 0);
            assert_eq!(echo_of(short), 0);
            assert_eq!(sid_of(short), "");
            assert!(!verify_mac(short, &key));
        }
        for (at, value) in [(8usize, 0u8), (8, 5), (9, 0), (9, 3), (0, b'X')] {
            let mut bad = packet;
            bad[at] = value;
            assert!(!looks_like_control(&bad), "byte {at} = {value}");
            assert_eq!(type_of(&bad), None);
        }
    }
}
