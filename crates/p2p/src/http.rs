//! HTTP for the signaling API, on top of `ureq`: no proxy, a time limit for the whole call, a size limit for the
//! answer, and failures sorted the way the C# code sorted `WebException`s, because the words end up in logs
//! people compare with older logs. (What goes to routers is written by hand: see router_http.rs.) Also the
//! little URL type both share.

use std::time::Duration;

use ureq::config::{AutoHeaderValue, Config};
use ureq::http::Request;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::Agent;

pub(crate) struct HttpResponse {
    pub status: u16,
    /// "Not Found" for 404, …; empty when the status came without words.
    pub reason: String,
    pub body: String,
}

impl HttpResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HttpFailure {
    /// The address is not something a request can be made to.
    BadUrl,
    /// Nothing came back within the time limit.
    Timeout,
    /// No HTTP answer; the name is .NET's `WebExceptionStatus` for the same failure ("ConnectFailure", …).
    NoAnswer(&'static str),
    /// The connection broke while the answer was being read, or the answer is larger than allowed.
    Broken,
}

pub(crate) struct HttpClient {
    agent: Agent,
}

impl HttpClient {
    /// `keep_alive`: reuse connections (the signaling server) or one request per connection.
    pub fn new(user_agent: &str, accept: Option<&str>, keep_alive: bool) -> HttpClient {
        let mut config = Config::builder()
            // a system proxy (and its discovery) would add seconds or break it
            .proxy(None)
            .http_status_as_error(false)
            .user_agent(user_agent)
            .accept(match accept {
                Some(value) => AutoHeaderValue::from(value),
                None => AutoHeaderValue::None,
            })
            // https through the operating system's TLS and its certificate store, as it was with .NET
            .tls_config(TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build());
        config = if keep_alive {
            // The signaling server closes idle keep-alive connections after 30 s. A long-poll, a heartbeat and a
            // live-list refresh run side by side, so more than one connection per host stays open.
            config.max_idle_age(Duration::from_secs(20)).max_idle_connections(16).max_idle_connections_per_host(16)
        } else {
            config.max_idle_connections(0).max_idle_connections_per_host(0)
        };
        HttpClient { agent: config.build().new_agent() }
    }

    /// One request. `timeout` covers everything from connecting to the last byte of the answer. Blocks; the
    /// only way out early is the timeout.
    pub fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        timeout: Duration,
        max_body_bytes: u64,
    ) -> Result<HttpResponse, HttpFailure> {
        let mut builder = Request::builder().method(method).uri(url);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let result = match body {
            Some(bytes) => {
                let request = builder.body(bytes).map_err(|_| HttpFailure::BadUrl)?;
                self.agent.run(self.agent.configure_request(request).timeout_global(Some(timeout)).build())
            }
            None => {
                let request = builder.body(()).map_err(|_| HttpFailure::BadUrl)?;
                self.agent.run(self.agent.configure_request(request).timeout_global(Some(timeout)).build())
            }
        };
        let mut response = result.map_err(failure_of)?;
        let status = response.status();
        let bytes = response.body_mut().with_config().limit(max_body_bytes).read_to_vec().map_err(|e| match failure_of(e) {
            HttpFailure::Timeout => HttpFailure::Timeout,
            _ => HttpFailure::Broken,
        })?;
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
        Ok(HttpResponse {
            status: status.as_u16(),
            reason: status.canonical_reason().unwrap_or("").to_string(),
            body: String::from_utf8_lossy(bytes).into_owned(),
        })
    }
}

fn failure_of(error: ureq::Error) -> HttpFailure {
    use std::io::ErrorKind;
    match error {
        ureq::Error::BadUri(_) | ureq::Error::Http(_) | ureq::Error::RequireHttpsOnly(_) => HttpFailure::BadUrl,
        ureq::Error::Timeout(_) => HttpFailure::Timeout,
        ureq::Error::HostNotFound => HttpFailure::NoAnswer("NameResolutionFailure"),
        ureq::Error::ConnectionFailed => HttpFailure::NoAnswer("ConnectFailure"),
        ureq::Error::Io(e) => match e.kind() {
            ErrorKind::TimedOut | ErrorKind::WouldBlock => HttpFailure::Timeout,
            ErrorKind::ConnectionRefused | ErrorKind::AddrNotAvailable | ErrorKind::NetworkUnreachable | ErrorKind::HostUnreachable => {
                HttpFailure::NoAnswer("ConnectFailure")
            }
            ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::UnexpectedEof => HttpFailure::NoAnswer("ConnectionClosed"),
            ErrorKind::BrokenPipe | ErrorKind::WriteZero => HttpFailure::NoAnswer("SendFailure"),
            _ => HttpFailure::NoAnswer("ReceiveFailure"),
        },
        ureq::Error::Protocol(_) | ureq::Error::LargeResponseHeader(..) => HttpFailure::NoAnswer("ServerProtocolViolation"),
        ureq::Error::TooManyRedirects | ureq::Error::RedirectFailed => HttpFailure::NoAnswer("ProtocolError"),
        ureq::Error::Tls(_) | ureq::Error::NativeTls(_) | ureq::Error::Der(_) | ureq::Error::Pem(_) | ureq::Error::TlsRequired => {
            HttpFailure::NoAnswer("SecureChannelFailure")
        }
        ureq::Error::BodyExceedsLimit(_) => HttpFailure::Broken,
        _ => HttpFailure::NoAnswer("UnknownError"),
    }
}

/// A very small http(s) URL: enough to keep the description and control URLs of a router, to resolve a
/// relative control URL against its base, and to print them the way .NET's `Uri` did (lower-case scheme and
/// host, no default port, "/" for an empty path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpUrl {
    https: bool,
    /// Lower case; an IPv6 literal keeps its brackets.
    host: String,
    port: Option<u16>,
    /// Starts with '/'; includes the query.
    path_and_query: String,
}

impl HttpUrl {
    /// An absolute http:// or https:// URL; `None` for anything else.
    pub fn parse(text: &str) -> Option<HttpUrl> {
        let text = text.trim();
        let (https, rest) = match strip_prefix_ignore_case(text, "http://") {
            Some(rest) => (false, rest),
            None => (true, strip_prefix_ignore_case(text, "https://")?),
        };
        let rest = rest.split('#').next().unwrap_or("");
        let end = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
            let close = bracketed.find(']')?;
            let after = &bracketed[close + 1..];
            let port = match after.strip_prefix(':') {
                Some(p) => Some(p),
                None if after.is_empty() => None,
                None => return None,
            };
            (&authority[..close + 2], port)
        } else {
            match authority.split_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            }
        };
        if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return None;
        }
        let port = match port {
            None | Some("") => None,
            Some(p) => Some(p.parse::<u16>().ok().filter(|p| *p != 0)?),
        };
        let default_port = if https { 443 } else { 80 };
        let path_and_query = match tail {
            "" => "/".to_string(),
            t if t.starts_with('?') => format!("/{t}"),
            t => t.to_string(),
        };
        if path_and_query.chars().any(|c| c.is_control()) {
            return None;
        }
        Some(HttpUrl { https, host: host.to_ascii_lowercase(), port: port.filter(|p| *p != default_port), path_and_query })
    }

    pub fn is_https(&self) -> bool {
        self.https
    }

    /// The host without the brackets of an IPv6 literal.
    pub fn host(&self) -> &str {
        self.host.trim_start_matches('[').trim_end_matches(']')
    }

    /// The port to connect to (the scheme's own when the URL names none).
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(if self.https { 443 } else { 80 })
    }

    /// What goes on the request line: the path with its query.
    pub fn path_and_query(&self) -> &str {
        &self.path_and_query
    }

    /// What goes into the Host header: "host", or "host:port" for another port than the scheme's own.
    pub fn authority(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}", self.host),
            None => self.host.clone(),
        }
    }

    /// `reference` as seen from this URL (RFC 3986 §5.2, for http): absolute URLs stand for themselves,
    /// "//host/path" keeps the scheme, "/path" keeps the host, anything else is relative to this path.
    pub fn join(&self, reference: &str) -> Option<HttpUrl> {
        let reference = reference.trim();
        if reference.is_empty() {
            return Some(self.clone());
        }
        if let Some(scheme_end) = reference.find(':') {
            let scheme = &reference[..scheme_end];
            let is_scheme = !scheme.is_empty()
                && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
            if is_scheme {
                return HttpUrl::parse(reference); // another scheme than http(s) is not ours to call
            }
        }
        if let Some(rest) = reference.strip_prefix("//") {
            return HttpUrl::parse(&format!("{}://{rest}", if self.https { "https" } else { "http" }));
        }
        let reference = reference.split('#').next().unwrap_or("");
        let path_and_query = if reference.starts_with('/') {
            reference.to_string()
        } else if reference.starts_with('?') {
            format!("{}{reference}", self.path_and_query.split('?').next().unwrap_or("/"))
        } else {
            let base_path = self.path_and_query.split('?').next().unwrap_or("/");
            let directory = &base_path[..=base_path.rfind('/').unwrap_or(0)];
            format!("{directory}{reference}")
        };
        if path_and_query.chars().any(|c| c.is_control()) {
            return None;
        }
        Some(HttpUrl { path_and_query: remove_dot_segments(&path_and_query), ..self.clone() })
    }
}

impl std::fmt::Display for HttpUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://{}", if self.https { "https" } else { "http" }, self.host)?;
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        f.write_str(&self.path_and_query)
    }
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &text[prefix.len()..])
}

/// "/a/b/../c/./d" → "/a/c/d" (the query, if any, is left alone).
fn remove_dot_segments(path_and_query: &str) -> String {
    let (path, query) = match path_and_query.find('?') {
        Some(at) => path_and_query.split_at(at),
        None => (path_and_query, ""),
    };
    let mut kept: Vec<&str> = Vec::new();
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    let last = segments.len().saturating_sub(1);
    for (i, segment) in segments.iter().enumerate() {
        match *segment {
            "." => {
                if i == last {
                    kept.push("");
                }
            }
            ".." => {
                kept.pop();
                if i == last {
                    kept.push("");
                }
            }
            other => kept.push(other),
        }
    }
    format!("/{}{query}", kept.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> String {
        HttpUrl::parse(text).unwrap_or_else(|| panic!("{text} should parse")).to_string()
    }

    #[test]
    fn urls_print_the_way_dotnet_printed_them() {
        assert_eq!(url("http://192.168.1.1:2869/upnp/790a830.xml"), "http://192.168.1.1:2869/upnp/790a830.xml");
        assert_eq!(url("HTTP://Router.Local:80/x?y=1#frag"), "http://router.local/x?y=1");
        assert_eq!(url("http://10.0.0.1"), "http://10.0.0.1/");
        assert_eq!(url("http://10.0.0.1?x=1"), "http://10.0.0.1/?x=1");
        assert_eq!(url("https://example.net:443/a"), "https://example.net/a");
        assert_eq!(url("https://example.net:8443/a"), "https://example.net:8443/a");
        assert_eq!(url("  http://[fd00::1]:5000/ctl  "), "http://[fd00::1]:5000/ctl");
        assert_eq!(HttpUrl::parse("http://[fd00::1]:5000/ctl").unwrap().host(), "fd00::1");
        assert_eq!(url("http://user:pw@10.0.0.1:81/a"), "http://10.0.0.1:81/a");
        for bad in ["", "file:///etc/passwd", "ftp://10.0.0.1/x", "http://", "http:///x", "http://host:0/", "http://host:99999/", "http://ho st/", "/relative", "http://[::1/x", "http://[::1]x/"] {
            assert_eq!(HttpUrl::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn references_resolve_against_their_base() {
        let base = HttpUrl::parse("http://192.168.1.1:2869/upnp/desc/790a830.xml?x=1").unwrap();
        let join = |reference: &str| base.join(reference).map(|u| u.to_string());
        assert_eq!(join("/upnp/control/0a1b2c3d").as_deref(), Some("http://192.168.1.1:2869/upnp/control/0a1b2c3d"));
        assert_eq!(join("http://192.168.1.1:5000/ip").as_deref(), Some("http://192.168.1.1:5000/ip"));
        assert_eq!(join("ctl/wan").as_deref(), Some("http://192.168.1.1:2869/upnp/desc/ctl/wan"));
        assert_eq!(join("../ctl/./wan?a=b").as_deref(), Some("http://192.168.1.1:2869/upnp/ctl/wan?a=b"));
        assert_eq!(join("../../../../wan").as_deref(), Some("http://192.168.1.1:2869/wan"));
        assert_eq!(join("//192.168.1.254:80/ip").as_deref(), Some("http://192.168.1.254/ip"));
        assert_eq!(join("?only=query").as_deref(), Some("http://192.168.1.1:2869/upnp/desc/790a830.xml?only=query"));
        assert_eq!(join("").as_deref(), Some("http://192.168.1.1:2869/upnp/desc/790a830.xml?x=1"));
        assert_eq!(join("ftp://192.168.1.1/x"), None);
        assert_eq!(join("file:///c:/windows/win.ini"), None);
        // a colon later in a relative path is not a scheme
        assert_eq!(join("ctl/a:b").as_deref(), Some("http://192.168.1.1:2869/upnp/desc/ctl/a:b"));
        let root = HttpUrl::parse("http://10.0.0.1").unwrap();
        assert_eq!(root.join("ctl").map(|u| u.to_string()).as_deref(), Some("http://10.0.0.1/ctl"));
    }
}
