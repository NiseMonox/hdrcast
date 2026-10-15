//! Port of `shared/Common`: nothing here knows about groups or the protocol.

pub mod app_paths;
pub mod arg_quote;
pub mod base64url;
pub mod endpoint;
pub mod firewall;
pub mod json;
pub mod line_process;
pub mod process_job;
pub mod secret_redactor;
pub mod srt_buffer_sizing;
pub mod srt_url;
pub mod tool_locator;

/// `string.Length` as the C# counted it (UTF-16 code units). Every length limit of protocol v1 was written
/// against that, so the limits are checked the same way here: a name the C# apps accept is accepted, and one
/// they refuse is refused.
pub(crate) fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// The longest prefix of `s` that is at most `max` UTF-16 code units, never cutting a character in half
/// (the C# `Substring(0, max)` could leave half a surrogate pair; that half would not survive UTF-8 anyway).
pub(crate) fn truncate_utf16(s: &str, max: usize) -> &str {
    let mut units = 0;
    for (at, c) in s.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &s[..at];
        }
    }
    s
}

/// First position at or after `from` where the ASCII `needle` occurs, ignoring ASCII case. `from` must be a
/// character boundary; so is the result, because an ASCII needle cannot start inside a UTF-8 sequence.
pub(crate) fn find_ignore_ascii_case(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    debug_assert!(needle.is_ascii());
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() || from > h.len() - n.len() {
        return None;
    }
    (from..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// `text.StartsWith(prefix, OrdinalIgnoreCase)` for an ASCII prefix.
pub(crate) fn starts_with_ignore_ascii_case(text: &str, prefix: &str) -> bool {
    debug_assert!(prefix.is_ascii());
    text.len() >= prefix.len() && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_lengths_count_surrogate_pairs_twice() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len("测试组"), 3);
        assert_eq!(utf16_len("a😀"), 3);
        assert_eq!(truncate_utf16("abcdef", 4), "abcd");
        assert_eq!(truncate_utf16("ab😀", 3), "ab");
        assert_eq!(truncate_utf16("ab😀", 4), "ab😀");
        assert_eq!(truncate_utf16("ab", 24), "ab");
    }

    #[test]
    fn ascii_search_ignores_case_and_respects_the_start() {
        assert_eq!(find_ignore_ascii_case("see HDRCAST: x hdrcast:y", "hdrcast:", 0), Some(4));
        assert_eq!(find_ignore_ascii_case("see HDRCAST: x hdrcast:y", "hdrcast:", 5), Some(15));
        assert_eq!(find_ignore_ascii_case("短", "hdrcast:", 0), None);
        assert_eq!(find_ignore_ascii_case("abc", "c", 3), None);
        assert!(starts_with_ignore_ascii_case("SRT://x", "srt://"));
        assert!(!starts_with_ignore_ascii_case("srt:", "srt://"));
    }
}
