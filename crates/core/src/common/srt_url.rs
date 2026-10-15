//! Builds srt:// URLs for ffmpeg / mpv (libavformat's libsrt.c reads every option from the ?query;
//! all latency-style values are MICROSECONDS, maxbw/inputbw are bytes per second, ffs is packets).

use super::endpoint;
use super::srt_buffer_sizing::{self, SrtBufferSizingError};
use super::{starts_with_ignore_ascii_case, utf16_len};

/// How the SRT socket reaches its peer. In P2P mode both ends are local: each viewer's remux ffmpeg listens on
/// 127.0.0.1 and mpv calls 127.0.0.1; HdrCast's own UDP links carry the datagrams in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SrtTopology {
    /// mode=listener on [`SrtUrlOptions::listen_address`] (default 0.0.0.0).
    Listener = 1,
    /// mode=caller to [`SrtUrlOptions::host`].
    Caller = 2,
}

/// Whether this side sends (a remux child) or receives (mpv) the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SrtRole {
    Publish = 0,
    Read = 1,
}

/// All inputs of [`build`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrtUrlOptions {
    pub topology: SrtTopology,
    pub role: SrtRole,

    /// The peer to call (name, IPv4 or IPv6, brackets optional); ignored for listeners.
    pub host: String,
    pub port: u16,

    /// Listener bind address: `None`/empty = 0.0.0.0 (IPv4 only). An IPv6 listener must name a concrete address:
    /// libsrt refuses the IPv6 wildcard "::" unless SRTO_IPV6ONLY is set, and ffmpeg has no option for it.
    pub listen_address: Option<String>,

    /// AES passphrase, 10-79 characters; `None`/empty = unencrypted.
    pub passphrase: Option<String>,
    /// pbkeylen: 16 / 24 / 32 (AES-128/192/256). Only emitted with a passphrase.
    pub pb_key_len: u32,

    /// SRT latency in milliseconds (the URL carries microseconds).
    pub latency_ms: u32,
    /// Expected stream bitrate in bits per second; drives buffer sizing and inputbw.
    pub bitrate_bps: u64,

    pub pkt_size: u32,
    pub live_trans_type: bool,

    /// Emit `maxbw=0&inputbw=<bitrate/8>&oheadbw=<pct>` (sender-side pacing).
    pub rate_control: bool,
    /// oheadbw percentage (5..100).
    pub overhead_percent: u32,

    /// connect_timeout in milliseconds (callers only; 0 = omit).
    pub connect_timeout_ms: u32,
    /// Emit recv_buffer_size=8000000 for readers.
    pub include_recv_socket_buffer: bool,
    pub recv_socket_buffer_bytes: u32,
}

impl Default for SrtUrlOptions {
    fn default() -> Self {
        SrtUrlOptions {
            topology: SrtTopology::Caller,
            role: SrtRole::Read,
            host: String::new(),
            port: 8890,
            listen_address: None,
            passphrase: None,
            pb_key_len: 32,
            latency_ms: 1500,
            bitrate_bps: 100_000_000,
            pkt_size: srt_buffer_sizing::PAYLOAD_BYTES,
            live_trans_type: true,
            rate_control: false,
            overhead_percent: 25,
            connect_timeout_ms: 5000,
            include_recv_socket_buffer: true,
            recv_socket_buffer_bytes: srt_buffer_sizing::RECV_SOCKET_BUFFER_BYTES,
        }
    }
}

/// Decoded pieces of an srt:// URL (see [`parse`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SrtUrlParts {
    pub host: String,
    /// 0 when the URL names no port.
    pub port: u16,
    /// Query parameters in URL order, values percent-decoded like ffmpeg does.
    pub query: Vec<(String, String)>,
}

impl SrtUrlParts {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
}

/// A host or listen address that cannot go into an SRT URL; the text is a sentence for the user.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SrtHostError(pub String);

/// Why [`build`] refused its options.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SrtUrlError {
    #[error("port must be 1..65535")]
    Port,
    #[error("latency must be positive")]
    Latency,
    #[error("bitrate must be positive")]
    Bitrate,
    #[error("pbkeylen must be 0, 16, 24 or 32")]
    PbKeyLen,
    #[error("oheadbw must be 5..100")]
    Overhead,
    #[error("passphrase must be 10-79 characters")]
    Passphrase,
    #[error(transparent)]
    Host(#[from] SrtHostError),
    #[error(transparent)]
    Buffer(#[from] SrtBufferSizingError),
}

/// Why [`parse`] could not split a URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SrtUrlParseError {
    #[error("not an srt:// URL")]
    NotSrt,
    #[error("unterminated IPv6 literal")]
    UnterminatedIpv6,
    #[error("an IPv6 address in a URL must be in [brackets]")]
    Ipv6NeedsBrackets,
    #[error("invalid port")]
    InvalidPort,
}

pub const LISTENER_HOST: &str = "0.0.0.0";
pub const MIN_PASSPHRASE_LENGTH: usize = 10;
pub const MAX_PASSPHRASE_LENGTH: usize = 79;

pub fn build(o: &SrtUrlOptions) -> Result<String, SrtUrlError> {
    if o.port == 0 {
        return Err(SrtUrlError::Port);
    }
    if o.latency_ms == 0 {
        return Err(SrtUrlError::Latency);
    }
    if o.bitrate_bps == 0 {
        return Err(SrtUrlError::Bitrate);
    }
    if !matches!(o.pb_key_len, 0 | 16 | 24 | 32) {
        return Err(SrtUrlError::PbKeyLen);
    }
    if !(5..=100).contains(&o.overhead_percent) {
        return Err(SrtUrlError::Overhead);
    }

    let listener = o.topology == SrtTopology::Listener;
    let host =
        if listener { listen_host_for(o.listen_address.as_deref())? } else { caller_host_for(&o.host, o.port)? };

    let mut q: Vec<(&str, String)> = Vec::new();
    q.push(("mode", if listener { "listener" } else { "caller" }.to_string()));

    if let Some(passphrase) = o.passphrase.as_deref().filter(|p| !p.is_empty()) {
        validate_passphrase(passphrase)?;
        q.push(("passphrase", encode(passphrase)));
        if o.pb_key_len != 0 {
            q.push(("pbkeylen", o.pb_key_len.to_string()));
        }
    }

    if o.pkt_size > 0 {
        q.push(("pkt_size", o.pkt_size.to_string()));
    }
    if o.live_trans_type {
        q.push(("transtype", "live".to_string()));
    }
    q.push(("latency", (u64::from(o.latency_ms) * 1000).to_string()));

    if o.rate_control {
        q.push(("maxbw", "0".to_string()));
        q.push(("inputbw", (o.bitrate_bps / 8).to_string()));
        q.push(("oheadbw", o.overhead_percent.to_string()));
    }

    let plan = srt_buffer_sizing::compute(o.latency_ms, o.bitrate_bps)?;
    q.push((if o.role == SrtRole::Publish { "sndbuf" } else { "rcvbuf" }, plan.buffer_bytes.to_string()));
    q.push(("ffs", plan.flow_window.to_string()));
    if o.role == SrtRole::Read && o.include_recv_socket_buffer && o.recv_socket_buffer_bytes > 0 {
        q.push(("recv_buffer_size", o.recv_socket_buffer_bytes.to_string()));
    }

    if listener {
        q.push(("listen_timeout", "-1".to_string()));
    } else if o.connect_timeout_ms > 0 {
        q.push(("connect_timeout", o.connect_timeout_ms.to_string()));
    }

    let mut url = format!("srt://{host}:{}?", o.port);
    for (i, (k, v)) in q.iter().enumerate() {
        if i > 0 {
            url.push('&');
        }
        url.push_str(k);
        url.push('=');
        url.push_str(v);
    }
    Ok(url)
}

/// The bracketed host for a listener URL. Empty = 0.0.0.0; otherwise an IP literal of this PC; never "::"
/// (see [`SrtUrlOptions::listen_address`]) and never a name.
pub fn listen_host_for(listen_address: Option<&str>) -> Result<String, SrtHostError> {
    let Some(address) = listen_address.filter(|a| !a.trim().is_empty()) else {
        return Ok(LISTENER_HOST.to_string());
    };
    let text = endpoint::normalize(address);
    if text == "::" || text == "[::]" {
        return Err(SrtHostError(
            "An SRT listener cannot use '::' (libsrt refuses the IPv6 wildcard and ffmpeg cannot change that). \
             Enter this PC's IPv6 address, or leave it empty for IPv4."
                .to_string(),
        ));
    }
    let hp = endpoint::parse_with(&text, true).map_err(|why| SrtHostError(format!("Listen address: {why}.")))?;
    if !hp.is_literal() {
        return Err(SrtHostError("Listen address must be an IP address of this PC, not a host name.".to_string()));
    }
    if hp.port().is_some() {
        return Err(SrtHostError("Listen address must not contain a port; use the Port box.".to_string()));
    }
    Ok(endpoint::format_host(hp.host()))
}

/// The bracketed host for a caller URL: name / IPv4 / IPv6 (brackets optional); a port typed into the host must
/// agree with `port`.
pub fn caller_host_for(host: &str, port: u16) -> Result<String, SrtHostError> {
    if host.trim().is_empty() {
        return Err(SrtHostError("Host is required.".to_string()));
    }
    let hp = endpoint::parse(host).map_err(|why| SrtHostError(format!("Host: {why}.")))?;
    if let Some(typed) = hp.port().filter(|&typed| typed != port) {
        return Err(SrtHostError(format!("Host contains port {typed} but the Port box says {port}.")));
    }
    Ok(endpoint::format_host(hp.host()))
}

pub fn validate_passphrase(passphrase: &str) -> Result<(), SrtUrlError> {
    if is_valid_passphrase(passphrase) {
        Ok(())
    } else {
        Err(SrtUrlError::Passphrase)
    }
}

/// True when the passphrase is empty (unencrypted) or 10-79 characters.
pub fn is_valid_passphrase(passphrase: &str) -> bool {
    passphrase.is_empty() || (MIN_PASSPHRASE_LENGTH..=MAX_PASSPHRASE_LENGTH).contains(&utf16_len(passphrase))
}

/// RFC 3986 percent-encoding of a query value: unreserved characters (ALPHA / DIGIT / "-" / "." / "_" / "~")
/// pass through, everything else (including '&', '#', '%', '+', ':', '=', spaces and non-ASCII as UTF-8)
/// becomes %XX. '+' must be encoded because av_find_info_tag turns a raw '+' into a space.
pub fn encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len() + 8);
    for &b in value.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0xf) as usize] as char);
        }
    }
    out
}

/// Decodes like ffmpeg: av_find_info_tag maps '+' to ' ', ff_urldecode expands %XX (UTF-8).
pub fn decode(value: &str) -> String {
    let raw = value.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'+' => bytes.push(b' '),
            b'%' if i + 2 < raw.len() && raw[i + 1].is_ascii_hexdigit() && raw[i + 2].is_ascii_hexdigit() => {
                bytes.push((hex_val(raw[i + 1]) << 4) | hex_val(raw[i + 2]));
                i += 2;
            }
            b => bytes.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Splits srt://host:port?k=v&k=v into parts, decoding values the way ffmpeg does.
pub fn parse(url: &str) -> Result<SrtUrlParts, SrtUrlParseError> {
    const SCHEME: &str = "srt://";
    if !starts_with_ignore_ascii_case(url, SCHEME) {
        return Err(SrtUrlParseError::NotSrt);
    }
    let rest = &url[SCHEME.len()..];
    let (mut authority, query) = rest.split_once('?').unwrap_or((rest, ""));
    if let Some(slash) = authority.find('/') {
        authority = &authority[..slash];
    }

    let (host_part, port_part) = if let Some(inner) = authority.strip_prefix('[') {
        let close = inner.find(']').ok_or(SrtUrlParseError::UnterminatedIpv6)?;
        (&inner[..close], inner[close + 1..].strip_prefix(':').unwrap_or(""))
    } else {
        match authority.rfind(':') {
            Some(colon) if authority.find(':') != Some(colon) => return Err(SrtUrlParseError::Ipv6NeedsBrackets),
            Some(colon) => (&authority[..colon], &authority[colon + 1..]),
            None => (authority, ""),
        }
    };

    let mut parts = SrtUrlParts { host: host_part.to_string(), ..SrtUrlParts::default() };
    if !port_part.is_empty() {
        if !port_part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(SrtUrlParseError::InvalidPort);
        }
        parts.port = port_part.parse().map_err(|_| SrtUrlParseError::InvalidPort)?;
    }

    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        parts.query.push((k.to_string(), decode(v)));
    }
    Ok(parts)
}

fn hex_val(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => c - b'A' + 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_ascii_and_reserved_characters_survive_encode_and_decode() {
        for text in ["测试 pass+phrase", "a%b&c=d#e~f", "100%", "%4", "", "a+b"] {
            assert_eq!(decode(&encode(text)), text, "{text:?}");
        }
        assert_eq!(encode("a b+c"), "a%20b%2Bc");
        assert_eq!(encode("é"), "%C3%A9");
        assert_eq!(decode("a+b%20c"), "a b c");
        assert_eq!(decode("100%"), "100%"); // an unfinished escape is kept as typed
        assert_eq!(decode("%zz"), "%zz");
    }

    #[test]
    fn parse_keeps_query_order_and_tolerates_a_path_and_no_port() {
        let p = parse("SRT://relay.example.net/live?b=2&&a=1&flag").unwrap();
        assert_eq!((p.host.as_str(), p.port), ("relay.example.net", 0));
        assert_eq!(p.query, vec![("b".into(), "2".into()), ("a".into(), "1".into()), ("flag".into(), "".into())]);
        assert_eq!(parse("http://x"), Err(SrtUrlParseError::NotSrt));
        assert_eq!(parse("srt://[::1:5"), Err(SrtUrlParseError::UnterminatedIpv6));
        assert_eq!(parse("srt://h:port"), Err(SrtUrlParseError::InvalidPort));
        assert_eq!(parse("srt://h:70000"), Err(SrtUrlParseError::InvalidPort));
    }

    #[test]
    fn option_errors_name_the_option() {
        let ok = SrtUrlOptions { host: "relay".into(), ..SrtUrlOptions::default() };
        assert!(build(&ok).is_ok());
        assert_eq!(build(&SrtUrlOptions { port: 0, ..ok.clone() }), Err(SrtUrlError::Port));
        assert_eq!(build(&SrtUrlOptions { latency_ms: 0, ..ok.clone() }), Err(SrtUrlError::Latency));
        assert_eq!(build(&SrtUrlOptions { bitrate_bps: 0, ..ok.clone() }), Err(SrtUrlError::Bitrate));
        assert_eq!(build(&SrtUrlOptions { pb_key_len: 20, ..ok.clone() }), Err(SrtUrlError::PbKeyLen));
        assert_eq!(build(&SrtUrlOptions { overhead_percent: 4, ..ok.clone() }), Err(SrtUrlError::Overhead));
        assert_eq!(build(&SrtUrlOptions { host: " ".into(), ..ok.clone() }).unwrap_err().to_string(), "Host is required.");
        assert_eq!(
            build(&SrtUrlOptions { host: "fe80::1".into(), ..ok.clone() }).unwrap_err().to_string(),
            "Host: fe80:: link-local addresses only work inside one network; use the global IPv6 address."
        );
        assert_eq!(build(&SrtUrlOptions { passphrase: Some("x".repeat(80)), ..ok }), Err(SrtUrlError::Passphrase));
    }

    #[test]
    fn the_whole_url_is_stable() {
        // The exact text 0.2.4 builds for a viewer's remux child; a changed order or name would still work, but a
        // diff in the log between versions should mean something.
        let child = SrtUrlOptions {
            topology: SrtTopology::Listener,
            role: SrtRole::Publish,
            listen_address: Some("127.0.0.1".into()),
            port: 40000,
            passphrase: Some("a77MWmBi3_603E3kH2abfOI5O-dIU5dn".into()),
            latency_ms: 3500,
            bitrate_bps: 40_000_000,
            rate_control: true,
            overhead_percent: 50,
            ..SrtUrlOptions::default()
        };
        assert_eq!(
            build(&child).unwrap(),
            "srt://127.0.0.1:40000?mode=listener&passphrase=a77MWmBi3_603E3kH2abfOI5O-dIU5dn&pbkeylen=32&pkt_size=1316\
             &transtype=live&latency=3500000&maxbw=0&inputbw=5000000&oheadbw=50&sndbuf=37683200&ffs=25600&listen_timeout=-1"
        );
    }
}
