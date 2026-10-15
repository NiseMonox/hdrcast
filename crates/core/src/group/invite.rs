//! What an invite carries (docs/design.md §8.2): where to meet, which group, and the group key; and the code it
//! travels as:
//!
//! hdrcast:<b64u(JSON)>.<first 8 chars of b64u(SHA-256(JSON))> — pasted whole from a chat message.
//! The extractor forgives what chat apps do to text: full-width characters, inserted line breaks and zero-width
//! characters, surrounding words.

use std::fmt;
use std::net::Ipv6Addr;

use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

use super::keys::{self, KEY_LENGTH};
use crate::common::{base64url, find_ignore_ascii_case, json, starts_with_ignore_ascii_case, utf16_len};

pub const CURRENT_VERSION: i64 = 1;
pub const MAX_NAME_LENGTH: usize = 40;
/// The signaling server new groups use unless told otherwise (metadata only, never video).
pub const DEFAULT_SIGNAL_URL: &str = "http://101.43.23.94:18650";

pub const PREFIX: &str = "hdrcast:";
pub const CHECKSUM_LENGTH: usize = 8;

/// Why the parts do not form a valid invite; `to_string()` is a sentence for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InviteProblem {
    #[error("the signaling address must be an http:// or https:// URL")]
    SignalUrl,
    #[error("the group id is not valid")]
    GroupId,
    #[error("the group key must be 32 bytes")]
    GroupKey,
    #[error("the group name must be 1-40 characters")]
    GroupName,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Invite {
    signal_url: String,
    group_id: String,
    group_key: [u8; KEY_LENGTH],
    group_name: String,
}

impl Invite {
    pub fn new(signal_url: &str, group_id: &str, group_key: &[u8], group_name: &str) -> Result<Invite, InviteProblem> {
        if let Some(problem) = Invite::problem(signal_url, group_id, group_key, group_name) {
            return Err(problem);
        }
        Ok(Invite {
            signal_url: normalize_url(signal_url).to_string(),
            group_id: group_id.to_string(),
            group_key: group_key.try_into().expect("length checked by problem()"),
            group_name: group_name.trim().to_string(),
        })
    }

    pub fn signal_url(&self) -> &str {
        &self.signal_url
    }

    pub fn group_id(&self) -> &str {
        &self.group_id
    }

    pub fn group_key(&self) -> &[u8; KEY_LENGTH] {
        &self.group_key
    }

    pub fn group_name(&self) -> &str {
        &self.group_name
    }

    /// Stable short tag for "the user already saw / dismissed this invite" (not secret-bearing).
    pub fn fingerprint(&self) -> String {
        let tagged = format!("{}\n{}", self.group_id, base64url::encode(&self.group_key));
        let mut tag = base64url::encode(&keys::sha256(tagged.as_bytes()));
        tag.truncate(12);
        tag
    }

    /// A brand-new group: random id and key, on [`DEFAULT_SIGNAL_URL`] unless another server is named.
    pub fn create(group_name: &str, signal_url: Option<&str>) -> Result<Invite, InviteProblem> {
        let url = signal_url.filter(|u| !u.trim().is_empty()).unwrap_or(DEFAULT_SIGNAL_URL);
        Invite::new(url, &keys::new_id(), &keys::random_bytes(KEY_LENGTH), group_name)
    }

    /// `None` when the parts form a valid invite, otherwise what is wrong with them.
    pub fn problem(signal_url: &str, group_id: &str, group_key: &[u8], group_name: &str) -> Option<InviteProblem> {
        if !is_http_url(normalize_url(signal_url)) {
            return Some(InviteProblem::SignalUrl);
        }
        if !keys::is_valid_id(group_id) {
            return Some(InviteProblem::GroupId);
        }
        if group_key.len() != KEY_LENGTH {
            return Some(InviteProblem::GroupKey);
        }
        let name_length = utf16_len(group_name.trim());
        if name_length == 0 || name_length > MAX_NAME_LENGTH {
            return Some(InviteProblem::GroupName);
        }
        None
    }
}

/// An invite carries the group key: `{:?}` shows everything but that.
impl fmt::Debug for Invite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Invite")
            .field("signal_url", &self.signal_url)
            .field("group_id", &self.group_id)
            .field("group_key", &"***")
            .field("group_name", &self.group_name)
            .finish()
    }
}

fn normalize_url(url: &str) -> &str {
    url.trim().trim_end_matches('/')
}

/// An absolute http:// or https:// URL whose host and port the C# apps' check (`Uri.TryCreate` on .NET Framework
/// 4.8) also takes: a host made of dot-separated labels of ASCII letters, digits, '-', '_' or non-ASCII
/// characters (an IDN), or a bracketed IPv6 address; optional `user@` in front; a port that is empty or a number
/// up to 65535. The rules were checked against the real thing case by case.
///
/// One thing is refused on purpose that .NET let through: a control character (tab, line break) anywhere, also
/// in the path. The URL ends up on an HTTP request line, where a line break is not text any more.
fn is_http_url(url: &str) -> bool {
    let rest = if starts_with_ignore_ascii_case(url, "http://") {
        &url[7..]
    } else if starts_with_ignore_ascii_case(url, "https://") {
        &url[8..]
    } else {
        return false;
    };
    if url.contains(' ') || url.chars().any(char::is_control) {
        return false;
    }
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let host_port = match authority.split_once('@') {
        Some((_user, host_port)) if !host_port.contains('@') => host_port,
        Some(_) => return false,
        None => authority,
    };

    let (host, port) = if let Some(inner) = host_port.strip_prefix('[') {
        let Some((v6, after)) = inner.split_once(']') else { return false };
        // a zone id ("%25eth0") is part of the literal
        let address = v6.split_once('%').map_or(v6, |(address, _zone)| address);
        if address.parse::<Ipv6Addr>().is_err() {
            return false;
        }
        match after.strip_prefix(':') {
            Some(port) => (None, port),
            None if after.is_empty() => (None, ""),
            None => return false,
        }
    } else {
        match host_port.rsplit_once(':') {
            Some((host, port)) => (Some(host), port),
            None => (Some(host_port), ""),
        }
    };

    // "http://host:/" is a valid way to spell the default port
    let port_ok = port.is_empty()
        || (port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u32>().is_ok_and(|p| p <= 65535));
    port_ok && host.is_none_or(is_url_host_name)
}

fn is_url_host_name(host: &str) -> bool {
    let name = host.strip_suffix('.').unwrap_or(host);
    !name.is_empty()
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || !c.is_ascii())
        })
}

/// Why no invite came out of a piece of text; `to_string()` is the sentence the user is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InviteError {
    /// No "hdrcast:" in the text.
    #[error("No HdrCast invite found (it starts with \"hdrcast:\").")]
    NotFound,
    /// Cut off: no checksum or a short one.
    #[error("The invite is cut off. Copy the whole message again.")]
    Incomplete,
    /// Checksum mismatch or undecodable.
    #[error("The invite is damaged (checksum mismatch). Ask for it again, or copy the whole message.")]
    Damaged,
    /// v > 1: made by a newer HdrCast.
    #[error("This invite comes from a newer HdrCast. Update the app.")]
    NewerVersion,
    /// Decodes, but a field is wrong.
    #[error("This invite is not valid.")]
    Invalid,
}

impl InviteError {
    pub const ALL: [InviteError; 5] = [
        InviteError::NotFound,
        InviteError::Incomplete,
        InviteError::Damaged,
        InviteError::NewerVersion,
        InviteError::Invalid,
    ];

    /// A sentence for the user (the same text as `to_string()`).
    pub fn describe(self) -> String {
        self.to_string()
    }
}

/// The code for `invite`: `hdrcast:<payload>.<checksum>`.
pub fn encode(invite: &Invite) -> String {
    // A struct, not a map: the field order is the wire order (v, s, g, k, n), whatever serde_json is built with.
    #[derive(Serialize)]
    struct Wire<'a> {
        v: i64,
        s: &'a str,
        g: &'a str,
        k: String,
        n: &'a str,
    }
    let wire = Wire {
        v: CURRENT_VERSION,
        s: &invite.signal_url,
        g: &invite.group_id,
        k: base64url::encode(&invite.group_key),
        n: &invite.group_name,
    };
    let json = serde_json::to_string(&wire).expect("strings and an integer always serialize");
    format!("{PREFIX}{}.{}", base64url::encode(json.as_bytes()), checksum(json.as_bytes()))
}

/// Finds and decodes the first valid invite in `text`. When there is none, the error describes the last
/// "hdrcast:" that was tried ([`InviteError::NotFound`] when there was no such prefix at all).
pub fn try_extract(text: &str) -> Result<Invite, InviteError> {
    let mut error = InviteError::NotFound;
    // Zero-width characters go everywhere first: some chat apps put one inside "hdrcast:" to stop auto-linking.
    let s: String = text.chars().filter(|&c| !is_zero_width(c)).nfkc().collect();

    let mut from = 0;
    while let Some(at) = find_ignore_ascii_case(&s, PREFIX, from) {
        from = at + PREFIX.len();
        match decode_at(&s[from..]) {
            Ok(invite) => return Ok(invite),
            Err(e) => error = e,
        }
    }
    Err(error)
}

fn decode_at(rest: &str) -> Result<Invite, InviteError> {
    // Collect base64url characters and the '.', skipping whitespace / zero-width characters that chat apps
    // insert; stop at anything else. The payload never contains '.', so the first '.' ends it and the next
    // 8 characters are the checksum (anything after that belongs to the surrounding message).
    let mut payload = String::new();
    let mut check = String::new();
    let mut in_checksum = false;
    for c in rest.chars() {
        if is_skippable(c) {
            continue;
        }
        if c == '.' {
            if in_checksum {
                break;
            }
            in_checksum = true;
            continue;
        }
        if !base64url::is_alphabet(c) {
            break;
        }
        if in_checksum {
            check.push(c);
            if check.len() == CHECKSUM_LENGTH {
                break;
            }
        } else {
            payload.push(c);
        }
    }

    if payload.is_empty() || !in_checksum || check.len() < CHECKSUM_LENGTH {
        return Err(InviteError::Incomplete);
    }
    let raw = base64url::decode(&payload).ok_or(InviteError::Damaged)?;
    if checksum(&raw) != check {
        return Err(InviteError::Damaged);
    }

    let root = json::parse(&String::from_utf8_lossy(&raw)).map_err(|_| InviteError::Invalid)?;
    if !root.is_object() {
        return Err(InviteError::Invalid);
    }

    let v = json::get(&root, "v").and_then(json::as_i64).ok_or(InviteError::Invalid)?;
    if v > CURRENT_VERSION {
        return Err(InviteError::NewerVersion);
    }
    if v != CURRENT_VERSION {
        return Err(InviteError::Invalid);
    }

    let field = |key: &str| json::get_str(&root, key);
    let key = field("k").and_then(base64url::decode).ok_or(InviteError::Invalid)?;
    Invite::new(field("s").unwrap_or(""), field("g").unwrap_or(""), &key, field("n").unwrap_or(""))
        .map_err(|_| InviteError::Invalid)
}

fn checksum(json: &[u8]) -> String {
    let mut sum = base64url::encode(&keys::sha256(json));
    sum.truncate(CHECKSUM_LENGTH);
    sum
}

fn is_skippable(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n' | '\u{00A0}') || is_zero_width(c)
}

fn is_zero_width(c: char) -> bool {
    matches!(c, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signaling_urls_are_http_or_https_with_a_real_host() {
        for ok in [
            "http://101.43.23.94:18650",
            "HTTPS://Signal.Example.net/",
            "https://signal.example.net/hdrcast/",
            "http://[2001:db8::1]:18650",
            "http://localhost:8080",
            "http://my_host:1",
            "http://例え.jp",
            "http://user@host:80/path?x=1",
            "http://user:pw@[::1]:80",
            "http://host:/",
            "http://host:0",
            "  http://host  ",
            "http://[fe80::1%25eth0]:80",
            "http://-a.b_c-",
            "http://1.2.3.4.5",
            "http://ho\u{3000}st", // .NET takes any non-ASCII character as part of an international name
        ] {
            assert!(is_http_url(normalize_url(ok)), "{ok}");
        }
        for bad in [
            "",
            "ftp://host",
            "host:80",
            "http://",
            "http:///path",
            "http://:80",
            "http://ho st",
            "http://ho\tst",
            "http://host/a\nb",
            "http://host:99999",
            "http://host:abc",
            "http://[::1",
            "http://[::1]x",
            "http://[nope]",
            "http://a..b",
            "http://.a",
            "http://ho|st",
            "http://ho%20st",
            "http://a@b@c",
            "http://host\\path",
            "http://host:+1",
            "http://x:1:2",
        ] {
            assert!(!is_http_url(normalize_url(bad)), "{bad:?}");
        }
    }

    #[test]
    fn the_parts_are_checked_in_order_and_normalized() {
        let gid = keys::new_id();
        let key = [1u8; 32];
        assert_eq!(Invite::problem("nope", &gid, &key, "g"), Some(InviteProblem::SignalUrl));
        assert_eq!(Invite::problem("http://h", "short", &key, "g"), Some(InviteProblem::GroupId));
        assert_eq!(Invite::problem("http://h", &gid, &key[..31], "g"), Some(InviteProblem::GroupKey));
        assert_eq!(Invite::problem("http://h", &gid, &key, "   "), Some(InviteProblem::GroupName));
        assert_eq!(Invite::problem("http://h", &gid, &key, &"名".repeat(41)), Some(InviteProblem::GroupName));
        assert_eq!(Invite::problem("http://h", &gid, &key, &"名".repeat(40)), None);
        // 21 emoji are 42 UTF-16 units: the C# apps would refuse this name, so it must not be sent to them
        assert_eq!(Invite::problem("http://h", &gid, &key, &"😀".repeat(21)), Some(InviteProblem::GroupName));

        let invite = Invite::new(" http://h:1/ ", &gid, &key, "  Friends ").unwrap();
        assert_eq!((invite.signal_url(), invite.group_name()), ("http://h:1", "Friends"));
        assert_eq!(invite.fingerprint().len(), 12);
        assert!(!format!("{invite:?}").contains(&base64url::encode(&key)), "the key must not print");
    }

    #[test]
    fn version_and_field_types_are_read_like_the_csharp_reader() {
        let gid = keys::new_id();
        let key = base64url::encode(&[9u8; 32]);
        let craft = |json: String| {
            format!("{PREFIX}{}.{}", base64url::encode(json.as_bytes()), checksum(json.as_bytes()))
        };
        let with_v = |v: &str| craft(format!(r#"{{"v":{v},"s":"http://h:1","g":"{gid}","k":"{key}","n":"x","future":[1,2]}}"#));
        assert!(try_extract(&with_v("1")).is_ok(), "unknown fields are ignored");
        assert!(try_extract(&with_v("1.0")).is_ok());
        assert!(try_extract(&with_v("\"1\"")).is_ok());
        assert_eq!(try_extract(&with_v("2")), Err(InviteError::NewerVersion));
        assert_eq!(try_extract(&with_v("1.5")), Err(InviteError::NewerVersion)); // rounds half to even: 2
        assert_eq!(try_extract(&with_v("0")), Err(InviteError::Invalid));
        assert_eq!(try_extract(&with_v("-3")), Err(InviteError::Invalid));
        assert_eq!(try_extract(&with_v("null")), Err(InviteError::Invalid));
        assert_eq!(try_extract(&with_v("true")), Err(InviteError::Invalid));

        assert_eq!(try_extract(&craft("[1]".to_string())), Err(InviteError::Invalid));
        assert_eq!(try_extract(&craft("not json".to_string())), Err(InviteError::Invalid));
        assert_eq!(try_extract(&craft(format!(r#"{{"v":1,"s":"http://h:1","g":"{gid}","k":"{key}"}}"#))), Err(InviteError::Invalid));
        assert_eq!(try_extract(&craft(format!(r#"{{"v":1,"s":"http://h:1","g":"{gid}","k":5,"n":"x"}}"#))), Err(InviteError::Invalid));
        assert_eq!(try_extract(&craft(format!(r#"{{"v":1,"s":"ftp://h","g":"{gid}","k":"{key}","n":"x"}}"#))), Err(InviteError::Invalid));
        // a repeated key keeps its last value
        let dup = craft(format!(r#"{{"v":1,"s":"http://h:1","g":"{gid}","k":"{key}","n":"first","n":"second"}}"#));
        assert_eq!(try_extract(&dup).unwrap().group_name(), "second");
    }

    #[test]
    fn the_error_is_the_last_prefix_tried() {
        assert_eq!(try_extract(""), Err(InviteError::NotFound));
        assert_eq!(try_extract("hdrcast:"), Err(InviteError::Incomplete));
        assert_eq!(try_extract("hdrcast:abc"), Err(InviteError::Incomplete));
        assert_eq!(try_extract("hdrcast:.abcdefgh"), Err(InviteError::Incomplete));
        assert_eq!(try_extract("hdrcast:abcd.abcdefgh"), Err(InviteError::Damaged));
        assert_eq!(try_extract("hdrcast:a.abcdefgh"), Err(InviteError::Damaged)); // 1 character is not base64
        assert_eq!(try_extract("hdrcast:abcd.abcdefgh then hdrcast:x"), Err(InviteError::Incomplete));
    }
}
