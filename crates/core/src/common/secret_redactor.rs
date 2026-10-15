//! Removes SRT passphrases, stream-id credentials (should one ever appear) and HdrCast invite codes from anything
//! that goes into a log file. The command line has to carry them, but the log does not: the app invites the user
//! to open the log folder and send it when something breaks, and mpv echoes the whole URL on a failed connection.
//! An invite code carries the group key, so it is as secret as a password.

use super::find_ignore_ascii_case;

pub const MASK: &str = "***";

/// Redacts every secret in a line of text (a command line, a log line, an mpv message).
pub fn redact(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let result = redact_query_value(text, "passphrase");
    let result = redact_stream_id_credentials(&result);
    redact_invite_codes(&result)
}

/// hdrcast:eyJ2Ijox….AbCdEfGh → hdrcast:*** (the prefix stays so the log still says an invite was there).
pub fn redact_invite_codes(text: &str) -> String {
    replace_values(text, "hdrcast:", |c| !is_invite_char(c), |value| if value.is_empty() { "" } else { MASK }.to_string())
}

/// Replaces the value of `key`=… up to the next & or whitespace or quote. `key` must be ASCII.
pub fn redact_query_value(text: &str, key: &str) -> String {
    let needle = format!("{key}=");
    replace_values(text, &needle, is_delimiter, |value| if value.is_empty() { "" } else { MASK }.to_string())
}

/// streamid=publish:screen:user:password → streamid=publish:screen:user:***
/// (the path and the user name are kept: they are useful in a log and are not secrets).
pub fn redact_stream_id_credentials(text: &str) -> String {
    replace_values(text, "streamid=", is_delimiter, mask_stream_id)
}

/// Copies `text`, passing whatever follows each `needle` (matched ignoring ASCII case), up to the first character
/// for which `ends_value` is true, through `replace`.
fn replace_values(
    text: &str,
    needle: &str,
    ends_value: impl Fn(char) -> bool,
    replace: impl Fn(&str) -> String,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let Some(at) = find_ignore_ascii_case(text, needle, i) else {
            out.push_str(&text[i..]);
            break;
        };
        let value_start = at + needle.len();
        let value_end = text[value_start..].find(&ends_value).map_or(text.len(), |n| value_start + n);
        out.push_str(&text[i..value_start]);
        out.push_str(&replace(&text[value_start..value_end]));
        i = value_end;
    }
    out
}

fn is_invite_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
}

fn mask_stream_id(value: &str) -> String {
    // Custom form: publish:path[:user:pass[:query]] — mask everything from the 4th component on.
    let parts: Vec<&str> = value.split(':').collect();
    if parts.len() >= 4 {
        return parts
            .iter()
            .enumerate()
            .map(|(i, part)| if i >= 3 { MASK } else { part })
            .collect::<Vec<_>>()
            .join(":");
    }
    // Haivision standard form: #!::m=publish,r=path,u=user,s=secret
    if find_ignore_ascii_case(value, "s=", 0).is_some() {
        return redact_comma_separated(value, "s");
    }
    value.to_string()
}

fn redact_comma_separated(value: &str, key: &str) -> String {
    let prefix = format!("{key}=");
    value
        .split(',')
        .map(|part| if super::starts_with_ignore_ascii_case(part, &prefix) { format!("{key}={MASK}") } else { part.to_string() })
        .collect::<Vec<_>>()
        .join(",")
}

fn is_delimiter(c: char) -> bool {
    c == '&' || c == '"' || c == '\'' || c.is_whitespace()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_occurrence_is_masked_whatever_its_case() {
        assert_eq!(redact("PASSPHRASE=abc passphrase=def"), "PASSPHRASE=*** passphrase=***");
        assert_eq!(redact("passphrase=&x=1 passphrase="), "passphrase=&x=1 passphrase=");
        assert_eq!(redact("密码 passphrase=口令口令 结束"), "密码 passphrase=*** 结束");
        assert_eq!(redact("'srt://h?passphrase=abc'"), "'srt://h?passphrase=***'");
    }

    #[test]
    fn the_haivision_form_keeps_everything_but_the_secret() {
        assert_eq!(redact("streamid=#!::m=publish,r=screen,u=sender,S=hunter2"), "streamid=#!::m=publish,r=screen,u=sender,s=***");
        // four or more colon-separated parts are the custom form: user:pass and whatever follows are masked
        assert_eq!(redact("streamid=read:screen:viewer:pw:extra&x"), "streamid=read:screen:viewer:***:***&x");
        assert_eq!(redact("STREAMID=read:screen"), "STREAMID=read:screen");
    }

    #[test]
    fn two_invites_on_one_line_are_both_masked() {
        assert_eq!(redact("a hdrcast:abc.def b HdrCast:x_y-z, c"), "a hdrcast:*** b HdrCast:***, c");
        assert_eq!(redact("hdrcast:"), "hdrcast:");
    }
}
