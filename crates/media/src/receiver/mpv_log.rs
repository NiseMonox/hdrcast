//! Guesses why mpv exited from its last log lines so the UI can say something useful.

use crate::text::contains_ignore_ascii_case as has;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MpvExitReason {
    #[default]
    Unknown = 0,
    /// The server had nothing to play yet ("no stream is available"; kept for old relay-style servers).
    NoPublisher = 1,
    /// Connection refused / timed out / reset (the path to the host is not up, or the host stopped).
    ConnectionFailed = 2,
    /// Wrong passphrase or credentials.
    AuthFailed = 3,
    /// The mpv build cannot open srt:// (no libsrt).
    ProtocolUnsupported = 4,
    /// The stream ended (the host stopped).
    EndOfStream = 5,
    /// The user closed the mpv window / quit was sent.
    Quit = 6,
}

pub fn classify_line(line: &str) -> MpvExitReason {
    if line.is_empty() {
        return MpvExitReason::Unknown;
    }
    // mpv echoes the whole URL when a connection fails, and our URL always contains the word "passphrase".
    // Classifying on the raw line would report every failure as an auth failure, so the query string is
    // stripped before anything is matched.
    let l = strip_urls(line);
    let l = l.as_str();
    if has(l, "no stream is available") || has(l, "no one is publishing") {
        return MpvExitReason::NoPublisher;
    }
    if has(l, "Protocol not found")
        || has(l, "not on whitelist")
        || has(l, "Unsupported protocol")
        || has(l, "Unrecognized URL")
    {
        return MpvExitReason::ProtocolUnsupported;
    }
    if has(l, "bad password")
        || has(l, "badsecret")
        || has(l, "wrong password")
        || has(l, "invalid passphrase")
        || has(l, "passphrase not provided")
        || has(l, "wrong passphrase")
        || has(l, "authentication failed")
        || has(l, "unauthorized")
        || has(l, "KM_BADSECRET")
    {
        return MpvExitReason::AuthFailed;
    }
    if has(l, "Exiting... (Quit)") {
        return MpvExitReason::Quit;
    }
    if has(l, "Exiting... (End of file)") || has(l, "Exiting... (Errors when loading file)") {
        return MpvExitReason::EndOfStream;
    }
    if has(l, "Connection refused")
        || has(l, "Connection timed out")
        || has(l, "timed out")
        || has(l, "Connection setup failure")
        || has(l, "connection rejected")
        || has(l, "Connection reset")
        || has(l, "Failed to open")
        || has(l, "Failed to recognize file format")
        || has(l, "Input/output error")
        || has(l, "No route to host")
        || has(l, "Name or service not known")
        || has(l, "getaddrinfo")
    {
        return MpvExitReason::ConnectionFailed;
    }
    MpvExitReason::Unknown
}

/// Removes srt:// / rist:// / http:// URLs from a line. mpv prints the URL it failed to open, and that URL
/// carries our own option names ("passphrase", "streamid", "latency"), which would otherwise be matched as if
/// they were error text.
pub fn strip_urls(line: &str) -> String {
    const MARK: &str = "://";
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find(MARK) {
        // back up over the scheme
        let scheme_len: usize = rest[..at]
            .chars()
            .rev()
            .take_while(|&c| is_scheme_char(c))
            .map(char::len_utf8)
            .sum();
        out.push_str(&rest[..at - scheme_len]);
        out.push_str("<url>");
        // the URL runs up to the next blank
        let after = &rest[at + MARK.len()..];
        rest = &after[after.find(char::is_whitespace).unwrap_or(after.len())..];
    }
    out.push_str(rest);
    out
}

fn is_scheme_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '+' | '-' | '.')
}

/// Most specific reason found in the lines (later lines do not override a more specific earlier one).
pub fn classify<I, S>(recent_lines: I) -> MpvExitReason
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut best = MpvExitReason::Unknown;
    for line in recent_lines {
        let reason = classify_line(line.as_ref());
        if reason != MpvExitReason::Unknown
            && (best == MpvExitReason::Unknown || rank(reason) > rank(best))
        {
            best = reason;
        }
    }
    best
}

/// True for lines that mean "nothing to play yet, retry" (nothing published yet, or unreachable).
pub fn is_stream_unavailable(line: &str) -> bool {
    matches!(
        classify_line(line),
        MpvExitReason::NoPublisher | MpvExitReason::ConnectionFailed
    )
}

pub fn describe(reason: MpvExitReason) -> &'static str {
    match reason {
        MpvExitReason::NoPublisher => "nothing to play yet",
        MpvExitReason::ConnectionFailed => "could not connect (the path to the host is not up yet, or the host stopped)",
        MpvExitReason::AuthFailed => "rejected: check passphrase / user / password",
        MpvExitReason::ProtocolUnsupported => {
            "this mpv build cannot open srt:// (use the ffmpeg pipe frontend or a shinchiro/zhongfly build)"
        }
        MpvExitReason::EndOfStream => "stream ended",
        MpvExitReason::Quit => "mpv was closed",
        MpvExitReason::Unknown => "mpv exited",
    }
}

fn rank(reason: MpvExitReason) -> u8 {
    match reason {
        MpvExitReason::ProtocolUnsupported => 6,
        MpvExitReason::AuthFailed => 5,
        MpvExitReason::NoPublisher => 4,
        MpvExitReason::ConnectionFailed => 3,
        MpvExitReason::EndOfStream => 2,
        MpvExitReason::Quit => 1,
        MpvExitReason::Unknown => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_stripping_handles_several_urls_and_odd_positions() {
        assert_eq!(strip_urls("no url here"), "no url here");
        assert_eq!(strip_urls("srt://h:1?passphrase=x"), "<url>");
        assert_eq!(
            strip_urls("a srt://h:1?p=1 b http://x/y c"),
            "a <url> b <url> c"
        );
        assert_eq!(
            strip_urls("open(rist+udp://h:1) failed"),
            "open(<url> failed"
        );
        assert_eq!(strip_urls("://"), "<url>");
        assert_eq!(strip_urls("x :// y"), "x <url> y");
        assert_eq!(
            strip_urls("连接 srt://主机:1?passphrase=口令\t失败"),
            "连接 <url>\t失败"
        );
        // a scheme only reaches back to where the previous URL ended
        assert_eq!(strip_urls("a://b://c d"), "<url> d");
    }

    #[test]
    fn the_most_specific_reason_wins_whatever_the_order() {
        let lines = [
            "Exiting... (Quit)",
            "[ffmpeg] srt: Connection refused",
            "[ffmpeg] srt: invalid passphrase",
            "Exiting... (End of file)",
        ];
        assert_eq!(classify(lines), MpvExitReason::AuthFailed);
        let mut reversed = lines;
        reversed.reverse();
        assert_eq!(classify(reversed), MpvExitReason::AuthFailed);
        assert_eq!(
            classify(["Protocol not found", "unauthorized"]),
            MpvExitReason::ProtocolUnsupported
        );
        assert_eq!(classify(Vec::<String>::new()), MpvExitReason::Unknown);
        assert_eq!(classify(["", "nothing special"]), MpvExitReason::Unknown);
    }

    #[test]
    fn every_reason_has_its_own_sentence() {
        let all = [
            MpvExitReason::Unknown,
            MpvExitReason::NoPublisher,
            MpvExitReason::ConnectionFailed,
            MpvExitReason::AuthFailed,
            MpvExitReason::ProtocolUnsupported,
            MpvExitReason::EndOfStream,
            MpvExitReason::Quit,
        ];
        let sentences: std::collections::HashSet<&str> = all.iter().map(|&r| describe(r)).collect();
        assert_eq!(sentences.len(), all.len());
        assert_eq!(all.map(|r| r as i32), [0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(describe(MpvExitReason::Unknown), "mpv exited");
    }

    #[test]
    fn matching_ignores_case_like_the_csharp_classifier() {
        assert_eq!(
            classify_line("[ffmpeg] SRT: CONNECTION TIMED OUT"),
            MpvExitReason::ConnectionFailed
        );
        assert_eq!(
            classify_line("srt: km_badsecret"),
            MpvExitReason::AuthFailed
        );
        assert_eq!(classify_line("exiting... (quit)"), MpvExitReason::Quit);
        assert_eq!(
            classify_line("Exiting... (Errors when loading file)"),
            MpvExitReason::EndOfStream
        );
        assert_eq!(classify_line(""), MpvExitReason::Unknown);
    }
}
