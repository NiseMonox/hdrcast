//! The HTTP that goes to routers (the device description and the SOAP actions of UPnP), written out by hand.
//!
//! A general HTTP library sends header names in lower case, which is fine by the standard and not fine by every
//! router: their HTTP servers are small, old and sometimes compare names byte for byte ("SOAPAction" is the
//! usual victim). What reached the routers this was developed against is what .NET sent, so that is what goes on
//! the wire here: the same headers, spelled and ordered the same way, one request per connection.
//!
//! Plain http only (no router speaks UPnP over TLS); no redirects (no router sends them for these).

use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use crate::http::{HttpFailure, HttpResponse, HttpUrl};

/// A response head larger than this is not a router's.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// One request on a connection of its own: User-Agent, then `headers` as given, then Host, Content-Length (with
/// a body) and "Connection: Close". `timeout` covers everything from connecting to the last byte of the answer.
pub(crate) fn request(
    method: &str,
    url: &HttpUrl,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    user_agent: &str,
    timeout: Duration,
    max_body_bytes: u64,
) -> Result<HttpResponse, HttpFailure> {
    let deadline = Instant::now() + timeout;
    let left = || match deadline.saturating_duration_since(Instant::now()) {
        d if d.is_zero() => Err(HttpFailure::Timeout),
        d => Ok(d),
    };

    let addresses: Vec<SocketAddr> = match url.host().parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, url.port())],
        Err(_) => (url.host(), url.port()).to_socket_addrs().map_err(|_| HttpFailure::NoAnswer("NameResolutionFailure"))?.collect(),
    };
    let mut failure = HttpFailure::NoAnswer(if addresses.is_empty() { "NameResolutionFailure" } else { "ConnectFailure" });
    let mut connected = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, left()?) {
            Ok(stream) => {
                connected = Some(stream);
                break;
            }
            Err(e) if is_timeout(&e) => failure = HttpFailure::Timeout,
            Err(_) => failure = HttpFailure::NoAnswer("ConnectFailure"),
        }
    }
    let Some(mut stream) = connected else { return Err(failure) };
    let _ = stream.set_nodelay(true);

    let mut message = format!("{method} {} HTTP/1.1\r\nUser-Agent: {user_agent}\r\n", url.path_and_query()).into_bytes();
    for (name, value) in headers {
        message.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    message.extend_from_slice(format!("Host: {}\r\n", url.authority()).as_bytes());
    if let Some(body) = body {
        message.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    }
    message.extend_from_slice(b"Connection: Close\r\n\r\n");
    // head and body in one write: some routers read once and expect all of it
    message.extend_from_slice(body.unwrap_or_default());
    let _ = stream.set_write_timeout(Some(left()?));
    stream.write_all(&message).and_then(|()| stream.flush()).map_err(|e| if is_timeout(&e) { HttpFailure::Timeout } else { HttpFailure::NoAnswer("SendFailure") })?;

    let mut received: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(response) = parse(&received, false, max_body_bytes)? {
            return Ok(response);
        }
        let _ = stream.set_read_timeout(Some(left()?));
        match stream.read(&mut chunk) {
            Ok(0) => {
                // the server closed: whatever is there is all there will be
                return parse(&received, true, max_body_bytes)?.ok_or(HttpFailure::NoAnswer("ConnectionClosed"));
            }
            Ok(n) => received.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if is_timeout(&e) => return Err(HttpFailure::Timeout),
            Err(_) => return Err(if find(&received, b"\r\n\r\n").is_some() { HttpFailure::Broken } else { HttpFailure::NoAnswer("ReceiveFailure") }),
        }
    }
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// The response in `received`, when it is complete (`at_end`: the connection is closed, nothing more comes).
/// `Ok(None)` = not yet.
fn parse(received: &[u8], at_end: bool, max_body_bytes: u64) -> Result<Option<HttpResponse>, HttpFailure> {
    let mut rest = received;
    loop {
        let Some(head_end) = find(rest, b"\r\n\r\n") else {
            if rest.len() > MAX_HEAD_BYTES {
                return Err(HttpFailure::NoAnswer("ServerProtocolViolation"));
            }
            return Ok(None);
        };
        let head = String::from_utf8_lossy(&rest[..head_end]).into_owned();
        let after = &rest[head_end + 4..];
        let mut lines = head.split("\r\n");
        let status_line = lines.next().unwrap_or("");
        let mut words = status_line.splitn(3, ' ');
        let (version, code) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
        let reason = words.next().unwrap_or("").trim().to_string();
        let status: u16 = code.parse().ok().filter(|_| version.starts_with("HTTP/1.") && code.len() == 3).ok_or(HttpFailure::NoAnswer("ServerProtocolViolation"))?;
        if (100..200).contains(&status) {
            rest = after; // "100 Continue" and the like: the real answer follows
            continue;
        }

        let header = |name: &str| {
            head.split("\r\n").skip(1).find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.trim().eq_ignore_ascii_case(name).then(|| value.trim().to_string())
            })
        };
        let too_large = |length: usize| length as u64 > max_body_bytes;
        let body: Vec<u8> = if status == 204 || status == 304 {
            Vec::new()
        } else if header("Transfer-Encoding").is_some_and(|encoding| encoding.to_ascii_lowercase().contains("chunked")) {
            match dechunk(after, max_body_bytes)? {
                Some(body) => body,
                None if at_end => return Err(HttpFailure::Broken),
                None => return Ok(None),
            }
        } else if let Some(length) = header("Content-Length") {
            let length: usize = length.parse().map_err(|_| HttpFailure::NoAnswer("ServerProtocolViolation"))?;
            if too_large(length) {
                return Err(HttpFailure::Broken);
            }
            if after.len() < length {
                return if at_end { Err(HttpFailure::Broken) } else { Ok(None) };
            }
            after[..length].to_vec()
        } else {
            // no length: the body is everything until the connection closes
            if too_large(after.len()) {
                return Err(HttpFailure::Broken);
            }
            if !at_end {
                return Ok(None);
            }
            after.to_vec()
        };
        let text = body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&body);
        return Ok(Some(HttpResponse { status, reason, body: String::from_utf8_lossy(text).into_owned() }));
    }
}

/// A chunked body, when its last chunk has arrived.
fn dechunk(mut rest: &[u8], max_body_bytes: u64) -> Result<Option<Vec<u8>>, HttpFailure> {
    let malformed = HttpFailure::NoAnswer("ServerProtocolViolation");
    let mut body = Vec::new();
    loop {
        let Some(line_end) = find(rest, b"\r\n") else { return Ok(None) };
        let size_text = String::from_utf8_lossy(&rest[..line_end]);
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16).map_err(|_| malformed)?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(Some(body)); // trailers, if any, are of no interest
        }
        if (body.len() + size) as u64 > max_body_bytes {
            return Err(HttpFailure::Broken);
        }
        if rest.len() < size + 2 {
            return Ok(None);
        }
        body.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::mpsc;
    use std::thread;

    /// A server for one connection: reads the request, then sends `pieces` one by one (a moment apart) and
    /// closes. Returns where it listens and, once the request is in, the request.
    fn serve(pieces: Vec<Vec<u8>>, linger: Duration) -> (HttpUrl, mpsc::Receiver<String>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let url = HttpUrl::parse(&format!("http://127.0.0.1:{}/ctl/wan?x=1", listener.local_addr().unwrap().port())).unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            // the head, then as much body as its Content-Length says
            while find(&request, b"\r\n\r\n").is_none() && stream.read(&mut byte).unwrap_or(0) == 1 {
                request.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&request).into_owned();
            let length: usize = head.lines().find_map(|l| l.strip_prefix("Content-Length: ")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
            let mut body = vec![0u8; length];
            let _ = stream.read_exact(&mut body);
            let _ = tx.send(format!("{head}{}", String::from_utf8_lossy(&body)));
            for piece in pieces {
                let _ = stream.write_all(&piece);
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(30));
            }
            thread::sleep(linger);
        });
        (url, rx)
    }

    fn one(response: &str) -> Vec<Vec<u8>> {
        vec![response.as_bytes().to_vec()]
    }

    const NONE: Duration = Duration::ZERO;
    const LIMIT: u64 = 256 * 1024;

    #[test]
    fn the_request_is_spelled_the_way_dotnet_spelled_it() {
        let (url, seen) = serve(one("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"), NONE);
        let headers = [("Content-Type", "text/xml; charset=\"utf-8\""), ("SOAPAction", "\"urn:x#AddPortMapping\"")];
        let response = request("POST", &url, &headers, Some(b"<body/>"), "HdrCast/1 UPnP/1.1", Duration::from_secs(5), LIMIT).unwrap();
        assert_eq!((response.status, response.reason.as_str(), response.body.as_str()), (200, "OK", "ok"));
        assert_eq!(
            seen.recv().unwrap(),
            format!(
                "POST /ctl/wan?x=1 HTTP/1.1\r\nUser-Agent: HdrCast/1 UPnP/1.1\r\nContent-Type: text/xml; charset=\"utf-8\"\r\n\
                 SOAPAction: \"urn:x#AddPortMapping\"\r\nHost: {}\r\nContent-Length: 7\r\nConnection: Close\r\n\r\n<body/>",
                url.authority()
            )
        );

        let (url, seen) = serve(one("HTTP/1.0 200 OK\r\n\r\n<root/>"), NONE);
        let response = request("GET", &url, &[], None, "HdrCast/1 UPnP/1.1", Duration::from_secs(5), LIMIT).unwrap();
        assert_eq!(response.body, "<root/>", "without a length the body ends where the connection does");
        assert_eq!(seen.recv().unwrap(), format!("GET /ctl/wan?x=1 HTTP/1.1\r\nUser-Agent: HdrCast/1 UPnP/1.1\r\nHost: {}\r\nConnection: Close\r\n\r\n", url.authority()));
    }

    #[test]
    fn answers_arrive_however_the_router_frames_them() {
        let get = |pieces: Vec<Vec<u8>>, linger: Duration| {
            let (url, _seen) = serve(pieces, linger);
            request("GET", &url, &[], None, "t", Duration::from_secs(5), LIMIT)
        };
        // in pieces, with the connection kept open afterwards: the length says when it is complete
        let pieces = vec![b"HTTP/1.1 200 OK\r\nCONTENT-LENGTH: 10\r\n".to_vec(), b"Server: tiny\r\n\r\n01234".to_vec(), b"56789".to_vec()];
        let started = Instant::now();
        assert_eq!(get(pieces, Duration::from_secs(3)).unwrap().body, "0123456789");
        assert!(started.elapsed() < Duration::from_secs(2), "waited for the connection to close: {:?}", started.elapsed());

        let chunked = vec![b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n".to_vec(), b"5;ext=1\r\npedia\r\n0\r\n\r\n".to_vec()];
        assert_eq!(get(chunked, Duration::from_secs(3)).unwrap().body, "Wikipedia");

        // a fault is an answer too: the status, its words and the body
        let fault = get(one("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\n\r\nfault"), NONE).unwrap();
        assert_eq!((fault.status, fault.reason.as_str(), fault.body.as_str()), (500, "Internal Server Error", "fault"));
        assert!(!fault.is_success());
        let interim = get(one("HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nx"), NONE).unwrap();
        assert_eq!((interim.status, interim.body.as_str()), (200, "x"));
        assert_eq!(get(one("HTTP/1.1 204 No Content\r\n\r\n"), Duration::from_secs(3)).unwrap().body, "");
        assert_eq!(get(one("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n\u{feff}ab"), NONE).unwrap().body, "ab", "a byte order mark is not text");
    }

    #[test]
    fn failures_are_told_apart() {
        let get = |pieces: Vec<Vec<u8>>, linger: Duration, timeout_ms: u64, limit: u64| {
            let (url, _seen) = serve(pieces, linger);
            request("GET", &url, &[], None, "t", Duration::from_millis(timeout_ms), limit).err()
        };
        // silent: the time limit is the whole call's
        let started = Instant::now();
        assert_eq!(get(Vec::new(), Duration::from_secs(3), 300, LIMIT), Some(HttpFailure::Timeout));
        assert!(started.elapsed() < Duration::from_millis(1500), "{:?}", started.elapsed());
        assert_eq!(get(one("HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n0123"), Duration::from_secs(3), 300, LIMIT), Some(HttpFailure::Timeout));
        // closed without a word, closed mid-answer, not HTTP, too large
        assert_eq!(get(Vec::new(), NONE, 3000, LIMIT), Some(HttpFailure::NoAnswer("ConnectionClosed")));
        assert_eq!(get(one("HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n0123"), NONE, 3000, LIMIT), Some(HttpFailure::Broken));
        assert_eq!(get(one("SSH-2.0-dropbear\r\n\r\n"), NONE, 3000, LIMIT), Some(HttpFailure::NoAnswer("ServerProtocolViolation")));
        assert_eq!(get(one("HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n01234567890"), NONE, 3000, 10), Some(HttpFailure::Broken));
        assert_eq!(get(one("HTTP/1.1 200 OK\r\n\r\n01234567890"), NONE, 3000, 10), Some(HttpFailure::Broken));
        assert_eq!(get(one("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nb\r\n01234567890\r\n0\r\n\r\n"), NONE, 3000, 10), Some(HttpFailure::Broken));

        // nobody listens
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        let nowhere = HttpUrl::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let refused = request("GET", &nowhere, &[], None, "t", Duration::from_secs(5), LIMIT).err();
        assert!(matches!(refused, Some(HttpFailure::NoAnswer("ConnectFailure")) | Some(HttpFailure::Timeout)), "{refused:?}");
    }
}
