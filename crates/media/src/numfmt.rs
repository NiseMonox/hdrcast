//! Numbers as the C# apps wrote them, so the mpv arguments and the status texts are the same character for
//! character. (hdrcast-capture has a similar helper, but keeps it private.)

/// `value.ToString("0.00##", InvariantCulture)`: at least `min_decimals` and at most `max_decimals` decimals.
///
/// .NET rounds the *decimal* text of the number, half away from zero: 2.5 is "3" with the format "0", and
/// 1.0005 is "1.001" with "0.###". `format!("{:.3}")` rounds the exact binary value half to even instead ("2"
/// and "1.000"), so the text is built from the shortest decimal form that reads back as the same number.
pub(crate) fn fixed(value: f64, min_decimals: usize, max_decimals: usize) -> String {
    debug_assert!(min_decimals <= max_decimals);
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }

    // Display never uses an exponent: "1500", "0.0000001", "2.0005".
    let text = value.abs().to_string();
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let mut digits: Vec<u8> = whole.bytes().collect();
    let whole_len = digits.len();
    digits.extend(fraction.bytes().take(max_decimals));
    let round_up = fraction
        .as_bytes()
        .get(max_decimals)
        .is_some_and(|&next| next >= b'5');

    let mut whole_len = whole_len;
    if round_up {
        let mut carry = true;
        for digit in digits.iter_mut().rev() {
            if *digit == b'9' {
                *digit = b'0';
            } else {
                *digit += 1;
                carry = false;
                break;
            }
        }
        if carry {
            digits.insert(0, b'1');
            whole_len += 1;
        }
    }

    let (whole, fraction) = digits.split_at(whole_len);
    let mut fraction = fraction.to_vec();
    while fraction.len() > min_decimals && fraction.last() == Some(&b'0') {
        fraction.pop();
    }
    fraction.resize(fraction.len().max(min_decimals), b'0');

    let mut out = String::with_capacity(digits.len() + 2);
    // .NET Framework never wrote "-0" for a small negative number that rounds to nothing.
    if value < 0.0 && digits.iter().any(|&d| d != b'0') {
        out.push('-');
    }
    out.push_str(std::str::from_utf8(whole).expect("ASCII digits"));
    if !fraction.is_empty() {
        out.push('.');
        out.push_str(std::str::from_utf8(&fraction).expect("ASCII digits"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::fixed;

    #[test]
    fn optional_decimals_are_dropped_when_they_are_zero() {
        // "0.###"
        assert_eq!(fixed(1.5, 0, 3), "1.5");
        assert_eq!(fixed(3.0, 0, 3), "3");
        assert_eq!(fixed(8.0, 0, 3), "8");
        assert_eq!(fixed(2.4690, 0, 3), "2.469");
        assert_eq!(fixed(0.1 + 0.2, 0, 3), "0.3");
        assert_eq!(fixed(1234.56789, 0, 3), "1234.568");
        assert_eq!(fixed(0.0, 0, 3), "0");
        assert_eq!(fixed(0.0004, 0, 3), "0");
    }

    #[test]
    fn required_decimals_are_always_written() {
        assert_eq!(fixed(39.8765, 1, 1), "39.9"); // "0.0"
        assert_eq!(fixed(40.0, 1, 1), "40.0");
        assert_eq!(fixed(0.0, 2, 2), "0.00"); // "0.00"
        assert_eq!(fixed(1.5, 2, 2), "1.50");
        assert_eq!(fixed(1000.0, 0, 0), "1000"); // "0"
        assert_eq!(fixed(4.25, 1, 2), "4.25");
        assert_eq!(fixed(4.2, 1, 2), "4.2");
    }

    #[test]
    fn halves_round_away_from_zero_like_dotnet() {
        assert_eq!(fixed(0.5, 0, 0), "1");
        assert_eq!(fixed(2.5, 0, 0), "3");
        assert_eq!(fixed(1.0005, 0, 3), "1.001");
        assert_eq!(fixed(1.005, 2, 2), "1.01");
        assert_eq!(fixed(0.125, 2, 2), "0.13");
        assert_eq!(fixed(-2.5, 0, 0), "-3");
        assert_eq!(fixed(9.9996, 0, 3), "10");
        assert_eq!(fixed(99.95, 1, 1), "100.0");
        assert_eq!(fixed(0.9999, 2, 2), "1.00");
    }

    #[test]
    fn nothing_odd_is_written_for_odd_numbers() {
        assert_eq!(fixed(-0.0, 0, 3), "0");
        assert_eq!(fixed(-0.0001, 0, 3), "0");
        assert_eq!(fixed(-0.25, 0, 3), "-0.25");
        assert_eq!(fixed(1e21, 0, 3), "1000000000000000000000");
        assert_eq!(fixed(1e-7, 0, 3), "0");
        assert_eq!(fixed(f64::NAN, 0, 3), "NaN");
        assert_eq!(fixed(f64::INFINITY, 0, 3), "Infinity");
        assert_eq!(fixed(f64::NEG_INFINITY, 1, 1), "-Infinity");
    }
}
