//! The signaling server's UDP echo (docs/design.md §8.7): request "HDRCECHO"+nonce(16); reply
//! "HDRCECHO"+nonce+family(4|6)+port(u16 BE)+address(4|16). Unauthenticated and no larger than the request
//! plus 19 bytes, so it cannot be used for amplification.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use hdrcast_core::endpoint;
use hdrcast_core::group::keys;

pub const NONCE_LENGTH: usize = 16;
pub const DEFAULT_PORT: u16 = 18651;
const MAGIC: &[u8; 8] = b"HDRCECHO";
const HEAD: usize = 8 + NONCE_LENGTH + 1 + 2;

pub fn build_request(nonce: &[u8; NONCE_LENGTH]) -> [u8; 8 + NONCE_LENGTH] {
    let mut b = [0u8; 8 + NONCE_LENGTH];
    b[..8].copy_from_slice(MAGIC);
    b[8..].copy_from_slice(nonce);
    b
}

/// Magic and a plausible length (4- or 16-byte address).
pub fn looks_like_reply(datagram: &[u8]) -> bool {
    (datagram.len() == HEAD + 4 || datagram.len() == HEAD + 16) && datagram[..8] == MAGIC[..]
}

/// The address the server saw, or `None` when the nonce or format does not match.
pub fn parse_reply(datagram: &[u8], nonce: &[u8; NONCE_LENGTH]) -> Option<SocketAddr> {
    if !looks_like_reply(datagram) || !keys::fixed_time_eq(&datagram[8..8 + NONCE_LENGTH], nonce) {
        return None;
    }
    let port = u16::from_be_bytes([datagram[25], datagram[26]]);
    let address = &datagram[HEAD..];
    let ip = match (datagram[24], address.len()) {
        (4, 4) => IpAddr::V4(Ipv4Addr::new(address[0], address[1], address[2], address[3])),
        (6, 16) => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(address).ok()?)),
        _ => return None,
    };
    if port == 0 {
        return None;
    }
    Some(SocketAddr::new(ip, port))
}

/// For tests and the server stand-in: the reply for `seen`.
pub fn build_reply(nonce: &[u8; NONCE_LENGTH], seen: SocketAddr) -> Vec<u8> {
    let mut b = Vec::with_capacity(HEAD + 16);
    b.extend_from_slice(MAGIC);
    b.extend_from_slice(nonce);
    match endpoint::unmap(seen.ip()) {
        IpAddr::V4(v4) => {
            b.push(4);
            b.extend_from_slice(&seen.port().to_be_bytes());
            b.extend_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            b.push(6);
            b.extend_from_slice(&seen.port().to_be_bytes());
            b.extend_from_slice(&v6.octets());
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_round_trip_and_mismatches_are_refused() {
        let nonce = [5u8; NONCE_LENGTH];
        for seen in ["203.0.113.9:40001", "[2001:db8::9]:40002"] {
            let seen: SocketAddr = seen.parse().unwrap();
            let reply = build_reply(&nonce, seen);
            assert!(looks_like_reply(&reply));
            assert_eq!(parse_reply(&reply, &nonce), Some(seen));
            assert_eq!(parse_reply(&reply, &[6u8; NONCE_LENGTH]), None, "another request's nonce");
            assert_eq!(parse_reply(&reply[..reply.len() - 1], &nonce), None);

            let mut wrong_family = reply.clone();
            wrong_family[24] = if seen.is_ipv4() { 6 } else { 4 };
            assert_eq!(parse_reply(&wrong_family, &nonce), None, "family and length disagree");
            let mut odd_family = reply.clone();
            odd_family[24] = 7;
            assert_eq!(parse_reply(&odd_family, &nonce), None);
            let mut no_port = reply.clone();
            no_port[25] = 0;
            no_port[26] = 0;
            assert_eq!(parse_reply(&no_port, &nonce), None);
            let mut other_magic = reply;
            other_magic[0] = b'X';
            assert!(!looks_like_reply(&other_magic));
        }
        // a dual-stack server reports IPv4 clients as ::ffff:a.b.c.d; the reply says IPv4
        let mapped: SocketAddr = "[::ffff:203.0.113.9]:40001".parse().unwrap();
        assert_eq!(parse_reply(&build_reply(&nonce, mapped), &nonce), Some("203.0.113.9:40001".parse().unwrap()));
        assert!(!looks_like_reply(&build_request(&nonce)));
    }
}
