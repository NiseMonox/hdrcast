//! Quotes command-line arguments the way Windows CreateProcess / the MSVCRT argv parser expects.
//!
//! [`LineProcess`](super::line_process::LineProcess) hands `std::process::Command` an argv, which does its own
//! quoting, so nothing here is needed to *start* a process any more. It is what a command line looks like in a
//! log, and what a user can paste into cmd.exe to run the same thing by hand.
//!
//! Rules: an argument without whitespace or quotes is passed verbatim; otherwise it is wrapped in double quotes,
//! embedded quotes become \" and backslashes immediately before a quote (or at the end of the argument) are
//! doubled. Backslashes elsewhere are left untouched.

pub fn quote(arg: &str) -> String {
    if !arg.is_empty() && !needs_quoting(arg) {
        return arg.to_string();
    }

    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut chars = arg.chars().peekable();
    loop {
        let mut backslashes = 0;
        while chars.peek() == Some(&'\\') {
            backslashes += 1;
            chars.next();
        }
        match chars.next() {
            None => {
                // Trailing backslashes must be doubled so they do not escape the closing quote.
                push_backslashes(&mut out, backslashes * 2);
                break;
            }
            Some('"') => {
                push_backslashes(&mut out, backslashes * 2 + 1);
                out.push('"');
            }
            Some(c) => {
                push_backslashes(&mut out, backslashes);
                out.push(c);
            }
        }
    }
    out.push('"');
    out
}

pub fn join<I, S>(args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = String::new();
    for a in args {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&quote(a.as_ref()));
    }
    out
}

/// Full command line: quoted executable followed by the quoted arguments.
pub fn command_line<I, S>(exe: &str, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let joined = join(args);
    if joined.is_empty() {
        quote(exe)
    } else {
        quote(exe) + " " + &joined
    }
}

fn push_backslashes(out: &mut String, count: usize) {
    for _ in 0..count {
        out.push('\\');
    }
}

fn needs_quoting(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '"')
            // Not needed by CreateProcess, but these characters break a command the user copies into
            // cmd.exe — and an SRT URL is full of '&'. Quoting them is harmless for CreateProcess.
            || matches!(c, '&' | '|' | '<' | '>' | '^')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backslashes_are_doubled_only_before_a_quote_or_the_end() {
        assert_eq!(quote(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(quote(r"a\b c"), r#""a\b c""#);
        assert_eq!(quote(r"a b\\"), r#""a b\\\\""#);
        assert_eq!(quote("\"\""), r#""\"\"""#);
    }

    #[test]
    fn a_command_line_quotes_the_executable_too() {
        assert_eq!(command_line(r"C:\Program Files\mpv\mpv.exe", ["--no-config", "a b"]), r#""C:\Program Files\mpv\mpv.exe" --no-config "a b""#);
        assert_eq!(command_line("mpv", Vec::<String>::new()), "mpv");
    }
}
