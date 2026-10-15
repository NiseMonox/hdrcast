//! The group this PC belongs to, in `group.json` of the settings folder (%APPDATA%\HdrCast on Windows). The C#
//! Sender and Receiver (0.2.4) share the very same file, so its field names and types stay as they are: pasting
//! an invite into any of the apps configures all of them. Written only when an invite is imported or a group is
//! created; holds the group key, so treat the file like a password.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use super::invite::Invite;
use super::keys::{self, GroupKeys};
use crate::common::{app_paths, base64url, json, truncate_utf16};

pub const FILE_NAME: &str = "group.json";
pub const MAX_DISPLAY_NAME_LENGTH: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("no group is configured")]
pub struct NotConfiguredError;

/// The fields are serialized in this order under these names; the C# apps read and rewrite the same file.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct GroupConfig {
    #[serde(rename = "v")]
    pub version: i32,
    #[serde(rename = "signal")]
    pub signal_url: String,
    #[serde(rename = "gid")]
    pub group_id: String,
    /// base64url of the 32-byte group key.
    #[serde(rename = "key")]
    pub group_key: String,
    #[serde(rename = "name")]
    pub group_name: String,
    /// How friends see this person in the live list and the viewer list.
    #[serde(rename = "me")]
    pub display_name: String,
    /// This install's id: the host id while streaming, the viewer id while watching.
    #[serde(rename = "peer")]
    pub peer_id: String,
    #[serde(rename = "importedAt")]
    pub imported_at_ms: i64,
    /// Offer to import an invite found on the clipboard when a window is activated.
    #[serde(rename = "detectClipboard")]
    pub detect_clipboard_invites: bool,
    /// Fingerprint of the last clipboard invite the user said no to (asked once, not every time).
    #[serde(rename = "dismissed")]
    pub dismissed_fingerprint: String,
}

impl Default for GroupConfig {
    fn default() -> Self {
        GroupConfig {
            version: 0,
            signal_url: String::new(),
            group_id: String::new(),
            group_key: String::new(),
            group_name: String::new(),
            display_name: String::new(),
            peer_id: String::new(),
            imported_at_ms: 0,
            detect_clipboard_invites: true,
            dismissed_fingerprint: String::new(),
        }
    }
}

/// `{:?}` shows everything but the group key.
impl fmt::Debug for GroupConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GroupConfig")
            .field("version", &self.version)
            .field("signal_url", &self.signal_url)
            .field("group_id", &self.group_id)
            .field("group_key", &if self.group_key.is_empty() { "" } else { "***" })
            .field("group_name", &self.group_name)
            .field("display_name", &self.display_name)
            .field("peer_id", &self.peer_id)
            .field("imported_at_ms", &self.imported_at_ms)
            .field("detect_clipboard_invites", &self.detect_clipboard_invites)
            .field("dismissed_fingerprint", &self.dismissed_fingerprint)
            .finish()
    }
}

impl GroupConfig {
    /// True when this describes a usable group.
    pub fn is_configured(&self) -> bool {
        self.try_get_invite().is_some()
    }

    pub fn try_get_invite(&self) -> Option<Invite> {
        let key = base64url::decode(&self.group_key)?;
        Invite::new(&self.signal_url, &self.group_id, &key, &self.group_name).ok()
    }

    pub fn to_invite(&self) -> Result<Invite, NotConfiguredError> {
        self.try_get_invite().ok_or(NotConfiguredError)
    }

    pub fn keys(&self) -> Result<GroupKeys, NotConfiguredError> {
        let invite = self.to_invite()?;
        GroupKeys::new(invite.group_key()).map_err(|_| NotConfiguredError)
    }

    /// Joins `invite`'s group, keeping this install's peer id and the user's preferences.
    pub fn adopt(&self, invite: &Invite, display_name: &str) -> GroupConfig {
        GroupConfig {
            version: 1,
            signal_url: invite.signal_url().to_string(),
            group_id: invite.group_id().to_string(),
            group_key: base64url::encode(invite.group_key()),
            group_name: invite.group_name().to_string(),
            display_name: clean_display_name(display_name),
            peer_id: if keys::is_valid_id(&self.peer_id) { self.peer_id.clone() } else { keys::new_id() },
            imported_at_ms: super::now_ms(),
            detect_clipboard_invites: self.detect_clipboard_invites,
            dismissed_fingerprint: self.dismissed_fingerprint.clone(),
        }
    }

    /// Makes sure a peer id and a display name exist (older files, or a file written by hand).
    pub fn normalize(mut self) -> GroupConfig {
        self.display_name = clean_display_name(&self.display_name);
        if !keys::is_valid_id(&self.peer_id) {
            self.peer_id = keys::new_id();
        }
        self
    }

    pub fn default_path() -> PathBuf {
        app_paths::settings_file(FILE_NAME)
    }

    /// The file's group, normalized; a missing, unreadable or broken file reads as "no group yet".
    pub fn load(path: &Path) -> GroupConfig {
        json::load_file_text(path).and_then(|text| GroupConfig::from_json(&text)).unwrap_or_default().normalize()
    }

    /// Reads `group.json` text (not normalized); `None` when it is not a JSON object.
    ///
    /// This file gets edited by hand ("detectClipboard" never had a switch anywhere else), and the C# apps'
    /// serializer forgave the usual slip, a comma left before the closing brace, as well as stray text after the
    /// object. A file that worked there must not read as "no group" (with a new peer id) here, so both are
    /// forgiven too.
    pub fn from_json(text: &str) -> Option<GroupConfig> {
        let value = json::parse(text).or_else(|_| json::parse(&without_trailing_commas(text))).ok()?;
        GroupConfig::from_value(&value)
    }

    /// Reads the fields of a `group.json` object (not normalized); `None` when `value` is not an object.
    /// A missing, null or unusable field takes its default, so a file from an older version still loads. Like
    /// the C# serializer, a number or true/false where a string belongs is taken as its text ("me": 123), and
    /// the flag also reads from 0 / 1 or from a string.
    pub fn from_value(value: &Value) -> Option<GroupConfig> {
        value.as_object()?;
        let text = |key: &str| match json::get(value, key) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::Bool(b)) => b.to_string(),
            _ => String::new(),
        };
        let number = |key: &str| json::get(value, key).and_then(json::as_i64).unwrap_or(0);
        let flag = |key: &str| match json::get(value, key)? {
            Value::Bool(b) => Some(*b),
            Value::Number(n) => match n.as_i64() {
                Some(0) => Some(false),
                Some(1) => Some(true),
                _ => None,
            },
            Value::String(s) => match s.trim() {
                "true" | "1" | "yes" => Some(true),
                "false" | "0" | "no" => Some(false),
                _ => None,
            },
            _ => None,
        };
        Some(GroupConfig {
            version: number("v") as i32,
            signal_url: text("signal"),
            group_id: text("gid"),
            group_key: text("key"),
            group_name: text("name"),
            display_name: text("me"),
            peer_id: text("peer"),
            imported_at_ms: number("importedAt"),
            detect_clipboard_invites: flag("detectClipboard").unwrap_or(true),
            dismissed_fingerprint: text("dismissed"),
        })
    }

    /// Writes the file atomically (see [`json::save_file`]).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        json::save_file(path, self)
    }
}

/// `text` without the commas that directly precede a closing brace or bracket (outside strings).
fn without_trailing_commas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let (mut in_string, mut escaped) = (false, false);
    for (at, c) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
        } else if c == ',' && text[at + 1..].trim_start().starts_with(['}', ']']) {
            continue;
        }
        out.push(c);
    }
    out
}

/// Trimmed, control characters removed, at most 24 characters; falls back to the operating system's user name.
pub fn clean_display_name(name: &str) -> String {
    let cleaned: String = name.trim().chars().filter(|c| !c.is_control()).collect();
    let mut s = cleaned.trim().to_string();
    if s.is_empty() {
        s = os_user_name();
    }
    truncate_utf16(&s, MAX_DISPLAY_NAME_LENGTH).to_string()
}

/// The name the user is logged in as ("friend" when the system will not say).
pub fn os_user_name() -> String {
    imp::user_name().filter(|n| !n.is_empty()).unwrap_or_else(|| "friend".to_string())
}

#[cfg(windows)]
mod imp {
    use windows::core::PWSTR;
    use windows::Win32::System::WindowsProgramming::GetUserNameW;

    pub fn user_name() -> Option<String> {
        // UNLEN (256) characters plus the terminator; the wide API, so a Chinese or Japanese name survives
        // whatever the ANSI code page is.
        let mut buffer = [0u16; 257];
        let mut size = buffer.len() as u32;
        // SAFETY: `buffer` is writable for `size` UTF-16 units and outlives the call.
        let ok = unsafe { GetUserNameW(Some(PWSTR(buffer.as_mut_ptr())), &mut size) }.is_ok();
        if ok && size > 0 {
            // `size` counts the terminating zero
            Some(String::from_utf16_lossy(&buffer[..size as usize - 1]))
        } else {
            std::env::var("USERNAME").ok()
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn user_name() -> Option<String> {
        std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A group.json exactly as the .NET Framework DataContractJsonSerializer behind 0.2.4 writes it (captured
    /// from the real serializer): CRLF, two spaces, "/" escaped, and a character outside the BMP written as a
    /// pair of \u escapes.
    const C_SHARP_FILE: &str = concat!(
        "{\r\n",
        r#"  "v": 1,"#,
        "\r\n",
        r#"  "signal": "http:\/\/101.43.23.94:18650","#,
        "\r\n",
        r#"  "gid": "oKGio6SlpqeoqaqrrK2urw","#,
        "\r\n",
        r#"  "key": "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8","#,
        "\r\n",
        r#"  "name": "测试组 \"quoted\" \\ \/slash","#,
        "\r\n",
        // the emoji U+1F600 as its two UTF-16 halves, backslash-u-escaped (written in pieces so that no tool
        // on the way to this file "helpfully" decodes them)
        r#"  "me": "小红 "#,
        "\\",
        "ud83d",
        "\\",
        "ude00",
        r#"","#,
        "\r\n",
        r#"  "peer": "wMHCw8TFxsfIycrLzM3Ozw","#,
        "\r\n",
        r#"  "importedAt": 1758500000000,"#,
        "\r\n",
        r#"  "detectClipboard": false,"#,
        "\r\n",
        r#"  "dismissed": "abcDEF123456""#,
        "\r\n}"
    );

    #[test]
    fn a_file_written_by_the_csharp_apps_is_read_field_for_field() {
        assert_eq!(C_SHARP_FILE.len(), 359, "the fixture is the captured file, byte for byte");
        let cfg = GroupConfig::from_value(&serde_json::from_str(C_SHARP_FILE).unwrap()).unwrap();
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.signal_url, "http://101.43.23.94:18650");
        assert_eq!(cfg.group_id, "oKGio6SlpqeoqaqrrK2urw");
        assert_eq!(cfg.group_name, "测试组 \"quoted\" \\ /slash");
        assert_eq!(cfg.display_name, "小红 😀");
        assert_eq!(cfg.peer_id, "wMHCw8TFxsfIycrLzM3Ozw");
        assert_eq!(cfg.imported_at_ms, 1758500000000);
        assert!(!cfg.detect_clipboard_invites);
        assert_eq!(cfg.dismissed_fingerprint, "abcDEF123456");
        assert!(cfg.is_configured());
        assert_eq!(cfg.clone().normalize(), cfg, "a complete file needs no repair");
    }

    #[test]
    fn the_file_is_written_with_the_csharp_names_in_the_csharp_order() {
        let cfg = GroupConfig::from_value(&serde_json::from_str(C_SHARP_FILE).unwrap()).unwrap();
        let text = serde_json::to_string(&cfg).unwrap();
        assert_eq!(
            text,
            r#"{"v":1,"signal":"http://101.43.23.94:18650","gid":"oKGio6SlpqeoqaqrrK2urw","key":"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8","name":"测试组 \"quoted\" \\ /slash","me":"小红 😀","peer":"wMHCw8TFxsfIycrLzM3Ozw","importedAt":1758500000000,"detectClipboard":false,"dismissed":"abcDEF123456"}"#
        );
    }

    #[test]
    fn older_partial_and_hand_edited_files_load_with_defaults() {
        let partial = GroupConfig::from_value(&serde_json::json!({"gid": "x", "me": null, "detectClipboard": "false", "v": "1", "importedAt": 5.0, "unknown": [1]})).unwrap();
        assert_eq!((partial.version, partial.group_id.as_str(), partial.imported_at_ms), (1, "x", 5));
        assert!(!partial.detect_clipboard_invites);
        assert_eq!(partial.display_name, "");
        assert!(!partial.is_configured());

        let empty = GroupConfig::from_value(&serde_json::json!({})).unwrap();
        assert_eq!(empty, GroupConfig::default());
        assert!(empty.detect_clipboard_invites, "clipboard detection is on unless switched off");
        assert_eq!(GroupConfig::from_value(&serde_json::json!([1, 2])), None);

        let repaired = empty.normalize();
        assert!(keys::is_valid_id(&repaired.peer_id));
        assert!(!repaired.display_name.is_empty());
    }

    #[test]
    fn hand_edits_the_csharp_apps_forgave_are_forgiven() {
        // each of these loaded in 0.2.4 (checked against its serializer on .NET Framework 4.8)
        let peer = "wMHCw8TFxsfIycrLzM3Ozw";
        let off = GroupConfig::from_json(&format!("{{\r\n  \"peer\": \"{peer}\",\r\n  \"detectClipboard\": false,\r\n}}")).unwrap();
        assert_eq!((off.peer_id.as_str(), off.detect_clipboard_invites), (peer, false));
        for (flag, expected) in [("0", false), ("1", true), ("\"0\"", false), ("\"1\"", true), ("\" false \"", false), ("\"true\"", true)] {
            let cfg = GroupConfig::from_json(&format!("{{\"peer\":\"{peer}\",\"detectClipboard\":{flag}}}")).unwrap();
            assert_eq!(cfg.detect_clipboard_invites, expected, "{flag}");
        }
        assert_eq!(GroupConfig::from_json(r#"{"me":123,"name":false,"gid":null}"#).unwrap().display_name, "123");
        assert_eq!(GroupConfig::from_json(r#"{"me":"a",},"#).unwrap().display_name, "a");
        assert_eq!(GroupConfig::from_json(r#"{"me":"a"}}"#).unwrap().display_name, "a");
        assert_eq!(GroupConfig::from_json("\r\n\t{\"me\":\"a\"}\n\n").unwrap().display_name, "a");
        // a comma inside a string is text, and only commas before a closing bracket are dropped
        let kept = GroupConfig::from_json(r#"{"me":"a,}","name":"b\",]",}"#).unwrap();
        assert_eq!((kept.display_name.as_str(), kept.group_name.as_str()), ("a,}", "b\",]"));
        assert_eq!(without_trailing_commas(r#"{"a":[1,2, ],"b":"x,]" ,}"#), r#"{"a":[1,2 ],"b":"x,]" }"#);

        // still not a config: no object, or JSON that no slip explains
        for broken in ["", "[1,2]", "null", "{oops", r#"{"me":"a",,}"#, r#"{'me':'a'}"#, r#"{"me":"a" "name":"b"}"#] {
            assert_eq!(GroupConfig::from_json(broken), None, "{broken}");
        }
    }

    #[test]
    fn nothing_usable_without_all_four_parts() {
        let good = GroupConfig::from_value(&serde_json::from_str(C_SHARP_FILE).unwrap()).unwrap();
        assert!(good.keys().is_ok());
        for broken in [
            GroupConfig { group_key: "AAAA".into(), ..good.clone() },
            GroupConfig { group_key: "not base64!".into(), ..good.clone() },
            GroupConfig { group_id: "short".into(), ..good.clone() },
            GroupConfig { signal_url: "".into(), ..good.clone() },
            GroupConfig { group_name: " ".into(), ..good.clone() },
        ] {
            assert!(!broken.is_configured());
            assert_eq!(broken.to_invite(), Err(NotConfiguredError));
            assert_eq!(broken.keys().err(), Some(NotConfiguredError));
        }
        assert!(!format!("{good:?}").contains("AAECAwQF"), "the key must not print");
    }

    #[test]
    fn adopting_keeps_preferences_and_replaces_a_bad_peer_id() {
        let good = GroupConfig::from_value(&serde_json::from_str(C_SHARP_FILE).unwrap()).unwrap();
        let other = Invite::create("Another", None).unwrap();
        let next = good.adopt(&other, " me\u{7}\n ");
        assert_eq!(next.version, 1);
        assert_eq!(next.group_id, other.group_id());
        assert_eq!(next.display_name, "me");
        assert_eq!(next.peer_id, good.peer_id);
        assert!(!next.detect_clipboard_invites);
        assert_eq!(next.dismissed_fingerprint, "abcDEF123456");
        assert!(next.imported_at_ms > good.imported_at_ms);

        let fresh = GroupConfig { peer_id: "bad".into(), ..GroupConfig::default() }.adopt(&other, "x");
        assert!(keys::is_valid_id(&fresh.peer_id));
    }

    #[test]
    fn names_are_cut_at_24_utf16_units_without_splitting_a_character() {
        assert_eq!(clean_display_name(&"名".repeat(30)), "名".repeat(24));
        assert_eq!(clean_display_name(&"😀".repeat(30)), "😀".repeat(12));
        assert_eq!(clean_display_name(&format!("{}😀", "x".repeat(23))), "x".repeat(23));
        assert_eq!(clean_display_name("\u{1}a\u{7f}b\u{9b}"), "ab");
        assert_eq!(clean_display_name(""), clean_display_name("  \t "));
        assert!(!os_user_name().is_empty());
    }
}
