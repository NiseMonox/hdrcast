//! Number formatting that mirrors the .NET custom formats the C# engine used ("0.#", "0.###", ...),
//! so tables, labels and command lines read the same as before.

/// `value.ToString("0.###")` with `decimals` hash marks: at most `decimals` decimals, trailing zeros removed.
pub(crate) fn trimmed(value: f64, decimals: usize) -> String {
    let mut s = format!("{value:.decimals$}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if s == "-0" {
        s = "0".to_owned();
    }
    s
}

/// `str.PadRight(width)`, counted in characters.
pub(crate) fn pad_right(s: &str, width: usize) -> String {
    let len = s.chars().count();
    let mut out = String::with_capacity(s.len() + width.saturating_sub(len));
    out.push_str(s);
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(len)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_like_dotnet_hash_formats() {
        assert_eq!(trimmed(80.0, 3), "80");
        assert_eq!(trimmed(0.5, 3), "0.5");
        assert_eq!(trimmed(59.94005994, 2), "59.94");
        assert_eq!(trimmed(0.0002, 4), "0.0002");
        assert_eq!(trimmed(239.76, 1), "239.8");
        assert_eq!(trimmed(1000.0, 0), "1000");
        assert_eq!(trimmed(-0.0001, 1), "0");
    }

    #[test]
    fn pads_by_characters() {
        assert_eq!(pad_right("ab", 5), "ab   ");
        assert_eq!(pad_right("abcdef", 3), "abcdef");
    }
}
