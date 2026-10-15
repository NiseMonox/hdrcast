//! Small text helpers shared by the parsers.

/// The words of a space-separated argument list, each as its own argument.
pub(crate) fn words(arguments: &str) -> Vec<String> {
    arguments.split(' ').map(str::to_string).collect()
}

/// `haystack.IndexOf(needle, StringComparison.OrdinalIgnoreCase) >= 0` for an ASCII needle.
pub(crate) fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    debug_assert!(needle.is_ascii());
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    n.is_empty()
        || h.windows(n.len())
            .any(|window| window.eq_ignore_ascii_case(n))
}

/// Position of the ASCII `needle` in `haystack`, ignoring ASCII case. An ASCII needle cannot start inside a
/// UTF-8 sequence, so the result is a character boundary.
pub(crate) fn find_ignore_ascii_case(haystack: &str, needle: &str) -> Option<usize> {
    debug_assert!(needle.is_ascii());
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() {
        return Some(0);
    }
    h.windows(n.len())
        .position(|window| window.eq_ignore_ascii_case(n))
}

/// `text.StartsWith(prefix, OrdinalIgnoreCase)` for an ASCII prefix.
pub(crate) fn starts_with_ignore_ascii_case(text: &str, prefix: &str) -> bool {
    debug_assert!(prefix.is_ascii());
    text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// `text.EndsWith(suffix, OrdinalIgnoreCase)` for an ASCII suffix.
pub(crate) fn ends_with_ignore_ascii_case(text: &str, suffix: &str) -> bool {
    debug_assert!(suffix.is_ascii());
    text.len() >= suffix.len()
        && text.as_bytes()[text.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
}

/// `double.TryParse(text, NumberStyles.Float, InvariantCulture)` for what ffmpeg prints: digits with an optional
/// sign, decimal point and exponent, surrounding blanks allowed. C's "inf" and "nan" are not numbers (the .NET
/// Framework apps read them as "no value" too), so the result is always finite.
pub(crate) fn parse_float(text: &str) -> Option<f64> {
    let t = text.trim();
    if t.is_empty()
        || !t
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        return None;
    }
    t.parse::<f64>().ok().filter(|d| d.is_finite())
}

/// `long.TryParse(text, NumberStyles.Integer, InvariantCulture)`.
pub(crate) fn parse_integer(text: &str) -> Option<i64> {
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_search_ignores_case_and_copes_with_other_scripts() {
        assert!(contains_ignore_ascii_case(
            "[ffmpeg] srt: CONNECTION REFUSED",
            "Connection refused"
        ));
        assert!(contains_ignore_ascii_case("连接 timed OUT 了", "timed out"));
        assert!(!contains_ignore_ascii_case("短", "timed out"));
        assert!(contains_ignore_ascii_case("anything", ""));
        assert_eq!(find_ignore_ascii_case("12.5KBits/s", "kbits/s"), Some(4));
        assert_eq!(find_ignore_ascii_case("N/A", "kbits/s"), None);
        assert!(starts_with_ignore_ascii_case("Frame=12", "frame="));
        assert!(!starts_with_ignore_ascii_case("fr", "frame="));
        assert!(ends_with_ignore_ascii_case("1.01X", "x"));
        assert!(!ends_with_ignore_ascii_case("", "x"));
    }

    #[test]
    fn numbers_are_read_the_way_dotnet_read_them() {
        assert_eq!(parse_float(" 59.94 "), Some(59.94));
        assert_eq!(parse_float("1e+03"), Some(1000.0));
        assert_eq!(parse_float("-.5"), Some(-0.5));
        for not_a_number in ["N/A", "", "inf", "nan", "Infinity", "1.0x", "0x10", "1e999"] {
            assert_eq!(parse_float(not_a_number), None, "{not_a_number}");
        }
        assert_eq!(parse_integer(" -42 "), Some(-42));
        assert_eq!(parse_integer("+7"), Some(7));
        assert_eq!(parse_integer("4.0"), None);
        assert_eq!(parse_integer("N/A"), None);
    }
}
