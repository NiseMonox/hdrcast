//! Control mode (docs/design.md §11): the stream for remote control, sent the way Moonlight and Sunshine /
//! Vibepollo send theirs instead of through SRT and a player's buffer. Every picture leaves the moment the encoder
//! has it, in datagrams on the punched path (the side channel, §8.9), with Reed-Solomon parity so that a few lost
//! packets cost nothing ([`video`]). Nothing is ever sent again: a picture that cannot be put together is given up
//! at once, the viewer keeps the last good picture on screen and asks for a keyframe ([`messages`]). The sound
//! goes alongside as 16-bit PCM in 5 ms packets ([`audio`]).
//!
//! Every datagram is sealed ([`seal`]) with ChaCha20-Poly1305 under a key of the session ([`session_key`]), as SRT
//! encrypts the watching stream:
//!
//! ```text
//!  0  1  kind     'V' a piece of a picture, 'A' sound, 'r' an answer (host → viewer); 'R' a request or a report
//!                 (viewer → host)
//!  1  8  counter  per direction, from 1, never the same twice (big-endian): the nonce; old ones are refused
//!  9  ..  the message, sealed, then its 16-byte tag
//! ```
//!
//! Remote control's datagrams on the same channel start with its direction byte (1 or 2) and keyframe requests
//! with 'K' / 'k', so none of them mix. Both ends send these only to a peer that said "ll" (§8.13).

pub mod audio;
pub mod messages;
pub mod seal;
pub mod video;

use hdrcast_core::group::keys;

/// A piece (shard) of a picture, host → viewer.
pub const VIDEO: u8 = b'V';
/// 5 ms of sound, host → viewer.
pub const AUDIO: u8 = b'A';
/// A keyframe request or a report, viewer → host.
pub const REQUEST: u8 = b'R';
/// The host's answer to a request or a report, host → viewer.
pub const ANSWER: u8 = b'r';

/// The most a side datagram carries (`hdrcast_p2p::side_channel::MAX_PAYLOAD`): kept small enough for the
/// smallest path MTU IPv6 allows, with the side channel's and the IP / UDP headers.
pub const MAX_DATAGRAM: usize = 1200;

/// The datagram belongs to control mode, whatever else is wrong with it.
pub fn is_low_latency_datagram(payload: &[u8]) -> bool {
    matches!(payload.first(), Some(&VIDEO) | Some(&AUDIO) | Some(&REQUEST) | Some(&ANSWER))
}

/// The key a control-mode session seals its datagrams with: derived from the session's remote-control key (the
/// one agreed with the viewer's device when there is one, §8.10), so nobody else in the group can read the picture.
pub fn session_key(control_key: &[u8; 32]) -> [u8; 32] {
    keys::hmac_sha256(control_key, b"hdrcast/v1/low-latency")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn its_datagrams_are_told_apart_from_the_other_users_of_the_side_channel() {
        for first in [VIDEO, AUDIO, REQUEST, ANSWER] {
            assert!(is_low_latency_datagram(&[first, 0]));
        }
        // remote control (direction 1 / 2) and keyframe requests ('K' / 'k')
        for first in [1u8, 2, b'K', b'k'] {
            assert!(!is_low_latency_datagram(&[first, 0]));
            assert!(!crate::keyframe::is_keyframe_datagram(&[VIDEO]) && !crate::keyframe::is_keyframe_datagram(&[AUDIO]));
        }
        assert!(!is_low_latency_datagram(&[]));
        assert_ne!(session_key(&[1; 32]), session_key(&[2; 32]));
        assert_ne!(session_key(&[1; 32]), [1; 32]);
    }
}
