//! Parses and formats network addresses the way people actually type them: "host", "host:port", a bare IPv6
//! address, `[v6]` or `[v6]:port`, even pasted as a whole srt:// URL or typed with a CJK input method
//! (full-width "：８８９０"). Link-local and zone-scoped addresses are rejected: they only work on one network
//! segment and ffmpeg's SRT URL parser cannot carry a zone id.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};

use unicode_normalization::UnicodeNormalization;

use super::starts_with_ignore_ascii_case;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EndpointKind {
    Hostname = 0,
    IPv4 = 1,
    IPv6 = 2,
}

/// Why an address was refused. `to_string()` is a sentence for the user (it completes "Host: … ." and
/// "Listen address: … .").
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EndpointError {
    #[error("the address is empty")]
    Empty,
    #[error("the IPv6 address has no closing ']'")]
    NoClosingBracket,
    #[error("expected ':port' after ']'")]
    ExpectedPortAfterBracket,
    #[error("only IPv6 addresses go in [brackets]")]
    BracketsNotIpv6,
    #[error("the port must be a number from 1 to 65535")]
    BadPort,
    #[error("the host is empty")]
    EmptyHost,
    #[error("zone ids (%...) are not supported; use the global address instead of fe80::")]
    ZoneId,
    #[error("'{0}' is not a valid IPv6 address")]
    InvalidIpv6(String),
    #[error("fe80:: link-local addresses only work inside one network; use the global IPv6 address")]
    LinkLocal,
    #[error("that is a multicast address")]
    Multicast,
    #[error(":: is not an address you can connect to")]
    UnspecifiedIpv6,
    #[error("'{0}' is not a valid IPv4 address")]
    InvalidIpv4(String),
    #[error("0.0.0.0 is not an address you can connect to")]
    UnspecifiedIpv4,
    #[error("'{0}' is not a valid host name")]
    InvalidHostName(String),
}

/// A host with an optional port, as typed by a person or carried in an invite / candidate list.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostPort {
    host: String,
    port: Option<u16>,
    kind: EndpointKind,
}

impl HostPort {
    pub fn new(host: impl Into<String>, port: Option<u16>, kind: EndpointKind) -> HostPort {
        HostPort { host: host.into(), port, kind }
    }

    /// Bare host: a name, a dotted IPv4 address or a compressed IPv6 address (never bracketed).
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> Option<u16> {
        self.port
    }

    pub fn kind(&self) -> EndpointKind {
        self.kind
    }

    pub fn is_literal(&self) -> bool {
        self.kind != EndpointKind::Hostname
    }

    /// "host:port" with IPv6 in brackets, using `default_port` when none was given.
    pub fn to_authority(&self, default_port: u16) -> String {
        format!("{}:{}", format_host(&self.host), self.port.unwrap_or(default_port))
    }

    /// The address itself (literals only; names need DNS first).
    pub fn ip(&self) -> Option<IpAddr> {
        if self.is_literal() {
            self.host.parse().ok()
        } else {
            None
        }
    }

    /// The address to send to (literals only; `None` for a host name, which has no address before DNS resolution).
    pub fn to_socket_addr(&self, default_port: u16) -> Option<SocketAddr> {
        self.ip().map(|ip| SocketAddr::new(ip, self.port.unwrap_or(default_port)))
    }
}

impl fmt::Display for HostPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.port {
            Some(port) => f.write_str(&self.to_authority(port)),
            None => f.write_str(&format_host(&self.host)),
        }
    }
}

/// Parses `input`; the error is a sentence for the user. "::" and "0.0.0.0" are refused (see [`parse_with`]).
pub fn parse(input: &str) -> Result<HostPort, EndpointError> {
    parse_with(input, false)
}

/// Like [`parse`]; `allow_unspecified` lets "::" and "0.0.0.0" through (a listener may bind them, nobody can
/// call them).
pub fn parse_with(input: &str, allow_unspecified: bool) -> Result<HostPort, EndpointError> {
    let normalized = normalize(input);
    let mut text = normalized.as_str();
    if text.is_empty() {
        return Err(EndpointError::Empty);
    }

    if starts_with_ignore_ascii_case(text, "srt://") {
        text = &text[6..];
    }
    if let Some(cut) = text.find(['?', '/']) {
        text = &text[..cut];
    }
    if let Some(at) = text.rfind('@') {
        text = &text[at + 1..]; // user@host from a pasted URL
    }

    let host: &str;
    let mut port_text: Option<&str> = None;
    if let Some(inner) = text.strip_prefix('[') {
        let close = inner.find(']').ok_or(EndpointError::NoClosingBracket)?;
        host = &inner[..close];
        let rest = &inner[close + 1..];
        if !rest.is_empty() {
            if !rest.starts_with(':') || rest.len() == 1 {
                return Err(EndpointError::ExpectedPortAfterBracket);
            }
            port_text = Some(&rest[1..]);
        }
        if !host.contains(':') {
            return Err(EndpointError::BracketsNotIpv6);
        }
    } else if text.bytes().filter(|&b| b == b':').count() == 1 {
        let (h, p) = text.split_once(':').expect("exactly one colon");
        host = h;
        port_text = Some(p);
    } else {
        // 0 colons: a name or IPv4; 2+: a bare IPv6 address (a port needs brackets: [v6]:port)
        host = text;
    }

    let port = match port_text {
        Some(p) => Some(parse_port(p).ok_or(EndpointError::BadPort)?),
        None => None,
    };

    let (canonical, kind) = classify_host(host, allow_unspecified)?;
    Ok(HostPort { host: canonical, port, kind })
}

/// Puts IPv6 literals in brackets for a URL authority; everything else is returned unchanged.
pub fn format_host(host: &str) -> String {
    if host.is_empty() || host.starts_with('[') || !host.contains(':') {
        host.to_string()
    } else {
        format!("[{host}]")
    }
}

/// `[v6]:port` or `v4:port` for a socket address (IPv4-mapped IPv6 is shown as IPv4).
pub fn format(addr: &SocketAddr) -> String {
    let host = match (unmap(addr.ip()), addr) {
        // .NET printed the scope of a link-local source address ("fe80::1%12"); keep it, it tells interfaces apart.
        (IpAddr::V6(ip), SocketAddr::V6(v6)) if v6.scope_id() != 0 => format!("{ip}%{}", v6.scope_id()),
        (ip, _) => ip.to_string(),
    };
    format!("{}:{}", format_host(&host), addr.port())
}

/// ::ffff:a.b.c.d → a.b.c.d (dual-stack sockets report IPv4 peers this way).
pub fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => ip,
        },
        IpAddr::V4(_) => ip,
    }
}

/// NFKC (full-width letters, digits and '：' become ASCII), ideographic full stops become '.', surrounding
/// quotes / corner brackets and whitespace are removed.
pub fn normalize(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let folded = input.nfkc().collect::<String>().replace('\u{3002}', ".");
    let mut s = folded.trim();
    const PAIRS: [(char, char); 7] =
        [('"', '"'), ('\'', '\''), ('「', '」'), ('『', '』'), ('<', '>'), ('（', '）'), ('(', ')')];
    let mut changed = true;
    while changed {
        changed = false;
        let mut chars = s.chars();
        let (Some(first), Some(last)) = (chars.next(), chars.next_back()) else { break };
        for (open, close) in PAIRS {
            if first == open && last == close {
                s = s[open.len_utf8()..s.len() - close.len_utf8()].trim();
                changed = true;
                break;
            }
        }
    }
    s.to_string()
}

/// Digits only (no sign, no spaces), 1..=65535.
fn parse_port(text: &str) -> Option<u16> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match text.parse::<u32>() {
        Ok(p) if (1..=65535).contains(&p) => Some(p as u16),
        _ => None,
    }
}

fn classify_host(host: &str, allow_unspecified: bool) -> Result<(String, EndpointKind), EndpointError> {
    if host.is_empty() {
        return Err(EndpointError::EmptyHost);
    }
    if host.contains('%') {
        return Err(EndpointError::ZoneId);
    }

    if host.contains(':') {
        let v6: Ipv6Addr = host.parse().map_err(|_| EndpointError::InvalidIpv6(host.to_string()))?;
        if let Some(v4) = v6.to_ipv4_mapped() {
            return classify_host(&v4.to_string(), allow_unspecified);
        }
        if v6.segments()[0] & 0xffc0 == 0xfe80 {
            return Err(EndpointError::LinkLocal);
        }
        if v6.is_multicast() {
            return Err(EndpointError::Multicast);
        }
        if v6.is_unspecified() && !allow_unspecified {
            return Err(EndpointError::UnspecifiedIpv6);
        }
        return Ok((v6.to_string(), EndpointKind::IPv6));
    }

    if looks_numeric(host) {
        if !is_strict_ipv4(host) {
            return Err(EndpointError::InvalidIpv4(host.to_string()));
        }
        if host == "0.0.0.0" && !allow_unspecified {
            return Err(EndpointError::UnspecifiedIpv4);
        }
        return Ok((host.to_string(), EndpointKind::IPv4));
    }

    if !is_host_name(host) {
        return Err(EndpointError::InvalidHostName(host.to_string()));
    }
    Ok((host.trim_end_matches('.').to_string(), EndpointKind::Hostname))
}

fn looks_numeric(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_digit() || b == b'.')
}

/// Exactly four decimal octets 0-255 (lenient parsers also take "1" or octal forms; people don't mean those).
fn is_strict_ipv4(s: &str) -> bool {
    let mut parts = 0;
    for p in s.split('.') {
        parts += 1;
        if p.is_empty() || p.len() > 3 {
            return false;
        }
        if p.len() > 1 && p.starts_with('0') {
            return false;
        }
        match p.parse::<u32>() {
            Ok(v) if v <= 255 => {}
            _ => return false,
        }
    }
    parts == 4
}

fn is_host_name(s: &str) -> bool {
    if s.len() > 253 {
        return false;
    }
    let trimmed = s.strip_suffix('.').unwrap_or(s);
    if trimmed.is_empty() {
        return false;
    }
    trimmed.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_shows_the_port_only_when_one_was_given() {
        assert_eq!(parse("[2001:db8::1]:8890").unwrap().to_string(), "[2001:db8::1]:8890");
        assert_eq!(parse("2001:db8::1").unwrap().to_string(), "[2001:db8::1]");
        assert_eq!(parse("relay.example.net").unwrap().to_string(), "relay.example.net");
    }

    #[test]
    fn socket_addresses_come_from_literals_only() {
        let hp = parse("[2001:db8::1]:4000").unwrap();
        assert_eq!(hp.to_socket_addr(0), Some("[2001:db8::1]:4000".parse().unwrap()));
        assert_eq!(parse("10.0.0.2").unwrap().to_socket_addr(5000), Some("10.0.0.2:5000".parse().unwrap()));
        assert_eq!(parse("example.com:4000").unwrap().to_socket_addr(0), None);
    }

    #[test]
    fn nested_quotes_and_brackets_are_peeled_off() {
        assert_eq!(normalize("  「 \"1.2.3.4\" 」 "), "1.2.3.4");
        assert_eq!(normalize("\""), "\"");
        assert_eq!(normalize("（relay：９０００）"), "relay:9000");
        assert_eq!(normalize("192｡168｡1｡1"), "192.168.1.1");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn a_mapped_unspecified_address_is_the_ipv4_wildcard() {
        assert_eq!(parse("::ffff:0.0.0.0"), Err(EndpointError::UnspecifiedIpv4));
        assert_eq!(parse_with("::ffff:0.0.0.0", true).unwrap().host(), "0.0.0.0");
    }

    #[test]
    fn user_info_and_paths_from_a_pasted_url_are_dropped() {
        let hp = parse("srt://viewer:secret@relay.example.net:8890/live?x=1").unwrap();
        assert_eq!((hp.host(), hp.port()), ("relay.example.net", Some(8890)));
        assert_eq!(parse("srt://"), Err(EndpointError::EmptyHost));
        assert_eq!(parse("relay:08890").unwrap().port(), Some(8890));
        assert_eq!(parse("relay:+80"), Err(EndpointError::BadPort));
        assert_eq!(parse("relay:"), Err(EndpointError::BadPort));
    }

    #[test]
    fn a_scoped_source_address_keeps_its_scope_when_formatted() {
        let scoped = SocketAddr::V6(std::net::SocketAddrV6::new("fe80::1".parse().unwrap(), 9, 0, 12));
        assert_eq!(format(&scoped), "[fe80::1%12]:9");
    }
}
