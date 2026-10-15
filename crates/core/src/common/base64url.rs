//! RFC 4648 §5 base64url without padding (decoding also accepts '=' padding).

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
        }
    }
    out
}

/// `None` when `text` is not base64url.
///
/// Bits left over in the last character are ignored, not checked, exactly like the C# (which handed the text to
/// `Convert.FromBase64String`) and like the signaling server's `^[A-Za-z0-9_-]{22}$`: every string of 22
/// alphabet characters is a valid id. A stricter decoder would refuse ids the other implementations accept.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let s = text.trim_end_matches('=').as_bytes();
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 2);
    for chunk in s.chunks(4) {
        let mut v = [0u8; 4];
        for (i, &c) in chunk.iter().enumerate() {
            v[i] = value_of(c)?;
        }
        out.push((v[0] << 2) | (v[1] >> 4));
        if chunk.len() > 2 {
            out.push((v[1] << 4) | (v[2] >> 2));
        }
        if chunk.len() > 3 {
            out.push((v[2] << 6) | v[3]);
        }
    }
    Some(out)
}

pub fn is_alphabet(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

fn value_of(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_length_round_trips() {
        let data: Vec<u8> = (0..=255u8).collect();
        for len in 0..data.len() {
            let text = encode(&data[..len]);
            assert!(!text.contains('='));
            assert_eq!(decode(&text).as_deref(), Some(&data[..len]), "length {len}");
        }
        assert_eq!(encode(b""), "");
        assert_eq!(decode(""), Some(Vec::new()));
        assert_eq!(decode("===="), Some(Vec::new()));
    }

    #[test]
    fn leftover_bits_are_ignored_like_the_other_implementations() {
        // "QQ" and "QR" both decode to 'A'; the C# and Python sides never looked at the spare bits.
        assert_eq!(decode("QQ"), Some(vec![0x41]));
        assert_eq!(decode("QR"), Some(vec![0x41]));
        assert_eq!(decode("QR=="), Some(vec![0x41]));
    }

    #[test]
    fn anything_outside_the_alphabet_is_refused() {
        for bad in ["a", "ab+c", "ab/c", "ab c", "ab\nc", "a=b", "ｈｄｒｃ", "abcde"] {
            assert_eq!(decode(bad), None, "{bad:?}");
        }
    }
}
