//! The small part of UPnP IGD (v1 and v2) needed to forward one UDP port (docs/design.md §8.7): an SSDP search,
//! the device description, and the SOAP actions GetExternalIPAddress / AddPortMapping / DeletePortMapping; and
//! [`RouterPortMapping`], which keeps this PC's IPv4 P2P port forwarded while it lives.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hdrcast_core::group::keys;
use socket2::{Domain, Protocol, Socket, Type};

use crate::cancel::CancelToken;
use crate::http::{HttpClient, HttpFailure, HttpResponse, HttpUrl};
use crate::local_addresses;
use crate::util::{lock, Callbacks, LogFn, Worker};
use crate::{router_http, xml};

/// A failed UPnP call: the router's UPnP error code (718 = port taken, 606 = not allowed, …) or 0.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct UpnpError {
    pub code: i32,
    pub message: String,
}

impl UpnpError {
    fn new(code: i32, message: impl Into<String>) -> UpnpError {
        UpnpError { code, message: message.into() }
    }
}

/// One WANIPConnection / WANPPPConnection service of an Internet Gateway Device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpnpGateway {
    /// Absolute http(s) URL the SOAP actions are posted to.
    pub control_url: String,
    pub service_type: String,
    /// The router's model or friendly name, for logs.
    pub name: String,
}

pub const SSDP_MULTICAST: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);

pub const ERROR_ONLY_PERMANENT_LEASES: i32 = 725;
pub const ERROR_CONFLICT: i32 = 718;

const MAX_DESCRIPTION_BYTES: u64 = 256 * 1024;

const SEARCH_TARGETS: [&str; 5] = [
    "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
    "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
    "urn:schemas-upnp-org:service:WANIPConnection:1",
    "urn:schemas-upnp-org:service:WANIPConnection:2",
    "urn:schemas-upnp-org:service:WANPPPConnection:1",
];

/// LOCATION URLs of the gateways that answer an SSDP search sent from `bind` (this PC's LAN address, so the
/// multicast leaves on the LAN and not on a VPN) to `target`.
pub fn search(bind: Ipv4Addr, target: SocketAddrV4, timeout_ms: u64, ct: &CancelToken) -> io::Result<Vec<String>> {
    let mut found: Vec<String> = Vec::new();
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.bind(&SocketAddr::V4(SocketAddrV4::new(bind, 0)).into())?;
    if target.ip().is_multicast() {
        let _ = socket.set_multicast_if_v4(&bind);
        let _ = socket.set_multicast_ttl_v4(2);
    }
    let socket: UdpSocket = socket.into();
    for _round in 0..2 {
        for st in SEARCH_TARGETS {
            let message = format!("M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {st}\r\n\r\n");
            let _ = socket.send_to(message.as_bytes(), target);
        }
    }

    // Polled rather than read with a timeout: on Windows a datagram that arrives as a receive timeout expires
    // is lost (see udp_channel.rs), and a router that answers once must not be missed for that.
    socket.set_nonblocking(true)?;
    let mut buffer = [0u8; 4096];
    let mut deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while !ct.is_cancelled() {
        if Instant::now() >= deadline {
            break;
        }
        let Ok((n, _from)) = socket.recv_from(&mut buffer) else {
            // nothing there yet, or an ICMP error for the search
            if ct.wait(Duration::from_millis(5)) {
                break;
            }
            continue;
        };
        let text: String = buffer[..n].iter().map(|&b| if b.is_ascii() { b as char } else { '?' }).collect();
        let Some(location) = parse_search_response(&text) else { continue };
        if found.contains(&location) {
            continue;
        }
        found.push(location);
        // the router answers within a few ms; give a second device a moment, then stop waiting
        let soon = Instant::now() + Duration::from_millis(400);
        deadline = deadline.min(soon);
    }
    Ok(found)
}

/// The LOCATION of an SSDP "HTTP/1.1 200 OK" answer (http only); `None` for anything else.
pub fn parse_search_response(text: &str) -> Option<String> {
    let starts_with = |prefix: &str| text.get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(prefix));
    if !starts_with("HTTP/1.1 200") && !starts_with("HTTP/1.0 200") {
        return None;
    }
    for raw in text.split('\n') {
        let line = raw.trim();
        let Some((name, value)) = line.split_once(':') else { continue };
        if name.is_empty() || !name.trim().eq_ignore_ascii_case("LOCATION") {
            continue;
        }
        if let Some(url) = HttpUrl::parse(value.trim()).filter(|url| !url.is_https()) {
            return Some(url.to_string());
        }
    }
    None
}

/// The WAN connection services the device description at `location` lists.
pub fn read_description(location: &str, timeout_ms: u64) -> Result<Vec<UpnpGateway>, UpnpError> {
    match router_send("GET", location, &[], None, timeout_ms) {
        Ok(response) if response.is_success() => parse_description(&response.body, location),
        Ok(_) => Err(UpnpError::new(0, "the router's description could not be read (ProtocolError)")),
        Err(HttpFailure::Broken) => Err(UpnpError::new(0, "the router's description could not be read")),
        Err(failure) => Err(UpnpError::new(0, format!("the router's description could not be read ({})", status_name(failure)))),
    }
}

/// Pure: WANIPConnection / WANPPPConnection services with absolute control URLs, in document order.
pub fn parse_description(xml_text: &str, location: &str) -> Result<Vec<UpnpGateway>, UpnpError> {
    let root = xml::parse(xml_text).map_err(|_| UpnpError::new(0, "the router's description is not valid XML"))?;
    let location = HttpUrl::parse(location).ok_or_else(|| UpnpError::new(0, "the router's description has no usable address"))?;
    let base = root.first_text("URLBase").and_then(|text| HttpUrl::parse(&text)).unwrap_or_else(|| location.clone());
    let name = root.first_text("modelName").or_else(|| root.first_text("friendlyName")).unwrap_or_else(|| location.host().to_string());

    let mut list = Vec::new();
    for service in root.descendants("service") {
        let service_type = service.child_text("serviceType").unwrap_or_default().trim().to_string();
        if !service_type.starts_with("urn:schemas-upnp-org:service:WANIPConnection:") && !service_type.starts_with("urn:schemas-upnp-org:service:WANPPPConnection:") {
            continue;
        }
        let control = service.child_text("controlURL").unwrap_or_default();
        let control = control.trim();
        if control.is_empty() {
            continue;
        }
        let Some(url) = base.join(control) else { continue };
        list.push(UpnpGateway { control_url: url.to_string(), service_type, name: name.trim().to_string() });
    }
    Ok(list)
}

/// The router's internet address as it reports it; `None` when it does not say (or says something else).
pub fn get_external_address(gateway: &UpnpGateway, timeout_ms: u64) -> Result<Option<Ipv4Addr>, UpnpError> {
    let answer = call(gateway, "GetExternalIPAddress", &[], timeout_ms)?;
    Ok(answer.get("NewExternalIPAddress").and_then(|text| text.trim().parse::<Ipv4Addr>().ok()))
}

pub fn add_port_mapping(
    gateway: &UpnpGateway,
    external_port: u16,
    internal_client: Ipv4Addr,
    internal_port: u16,
    description: &str,
    lease_seconds: u32,
    timeout_ms: u64,
) -> Result<(), UpnpError> {
    // Argument order as in the IGD spec: some routers insist on it.
    let args = [
        ("NewRemoteHost", String::new()),
        ("NewExternalPort", external_port.to_string()),
        ("NewProtocol", "UDP".to_string()),
        ("NewInternalPort", internal_port.to_string()),
        ("NewInternalClient", internal_client.to_string()),
        ("NewEnabled", "1".to_string()),
        ("NewPortMappingDescription", description.to_string()),
        ("NewLeaseDuration", lease_seconds.to_string()),
    ];
    call(gateway, "AddPortMapping", &args, timeout_ms).map(drop)
}

pub fn delete_port_mapping(gateway: &UpnpGateway, external_port: u16, timeout_ms: u64) -> Result<(), UpnpError> {
    let args = [("NewRemoteHost", String::new()), ("NewExternalPort", external_port.to_string()), ("NewProtocol", "UDP".to_string())];
    call(gateway, "DeletePortMapping", &args, timeout_ms).map(drop)
}

/// One SOAP action; returns the response's out-arguments, or the router's fault as an error.
pub fn call(gateway: &UpnpGateway, action: &str, args: &[(&str, String)], timeout_ms: u64) -> Result<HashMap<String, String>, UpnpError> {
    let body = build_envelope(&gateway.service_type, action, args);
    let soap_action = format!("\"{}#{action}\"", gateway.service_type);
    let headers = [("Content-Type", "text/xml; charset=\"utf-8\""), ("SOAPAction", soap_action.as_str())];
    match router_send("POST", &gateway.control_url, &headers, Some(body.as_bytes()), timeout_ms) {
        Ok(response) if response.is_success() => parse_response(&response.body, action),
        Ok(response) => Err(parse_fault(&response.body, response.status, action)),
        Err(HttpFailure::Broken) => Err(UpnpError::new(0, format!("{action}: the connection to the router failed"))),
        Err(failure) => Err(UpnpError::new(0, format!("{action}: the router did not answer ({})", status_name(failure)))),
    }
}

/// Pure: the SOAP request body.
pub fn build_envelope(service_type: &str, action: &str, args: &[(&str, String)]) -> String {
    let mut body = String::with_capacity(512);
    body.push_str("<?xml version=\"1.0\"?>");
    body.push_str("<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">");
    body.push_str(&format!("<s:Body><u:{action} xmlns:u=\"{}\">", escape(service_type)));
    for (name, value) in args {
        body.push_str(&format!("<{name}>{}</{name}>", escape(value)));
    }
    body.push_str(&format!("</u:{action}></s:Body></s:Envelope>"));
    body
}

/// Pure: the child elements of `<u:ActionResponse>` by local name.
pub fn parse_response(xml_text: &str, action: &str) -> Result<HashMap<String, String>, UpnpError> {
    let root = xml::parse(xml_text).map_err(|_| UpnpError::new(0, format!("{action}: the router sent something that is not SOAP")))?;
    let response = root
        .first(&format!("{action}Response"))
        .ok_or_else(|| UpnpError::new(0, format!("{action}: the router's answer has no {action}Response")))?;
    Ok(response.child_elements().map(|child| (child.local_name().to_string(), child.inner_text())).collect())
}

/// Pure: a SOAP fault (UPnPError errorCode / errorDescription) as an error.
pub fn parse_fault(xml_text: &str, http_status: u16, action: &str) -> UpnpError {
    let (mut code, mut description) = (0, String::new());
    if let Ok(root) = xml::parse(xml_text) {
        code = root.first_text("errorCode").and_then(|text| text.trim().parse::<i32>().ok()).unwrap_or(0);
        description = root.first_text("errorDescription").unwrap_or_default().trim().to_string();
    }
    let what = if code != 0 {
        if description.is_empty() {
            format!("error {code}")
        } else {
            format!("error {code} {description}")
        }
    } else {
        format!("HTTP {http_status}")
    };
    UpnpError::new(code, format!("{action} refused by the router ({what})"))
}

/// This PC's address on the way to `gateway`: the mapping's internal client.
pub fn local_address_towards(gateway: Ipv4Addr) -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((gateway, 1900)).ok()?; // UDP connect: picks a route, sends nothing
    match socket.local_addr().ok()? {
        SocketAddr::V4(local) => Some(*local.ip()),
        SocketAddr::V6(_) => None,
    }
}

const USER_AGENT: &str = "HdrCast/1 UPnP/1.1";

/// One request per connection, no proxy: the router is on the LAN and its HTTP server is tiny.
fn router_send(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&[u8]>, timeout_ms: u64) -> Result<HttpResponse, HttpFailure> {
    let parsed = HttpUrl::parse(url).ok_or(HttpFailure::BadUrl)?;
    let timeout = Duration::from_millis(timeout_ms);
    if parsed.is_https() {
        // No router is known to do this; should one, it gets the general client (and lower-case header names).
        let mut all = headers.to_vec();
        all.push(("Connection", "close"));
        return HttpClient::new(USER_AGENT, None, false).send(method, url, &all, body, timeout, MAX_DESCRIPTION_BYTES);
    }
    router_http::request(method, &parsed, headers, body, USER_AGENT, timeout, MAX_DESCRIPTION_BYTES)
}

fn status_name(failure: HttpFailure) -> &'static str {
    match failure {
        HttpFailure::Timeout => "Timeout",
        HttpFailure::NoAnswer(name) => name,
        HttpFailure::BadUrl => "ConnectFailure",
        HttpFailure::Broken => "ReceiveFailure",
    }
}

/// What .NET's `SecurityElement.Escape` escaped.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '&' => out.push_str("&amp;"),
            other => out.push(other),
        }
    }
    out
}

/// The name .NET's `SocketError` had for the same failure (it is part of a log line).
fn socket_error_name(error: &io::Error) -> String {
    use io::ErrorKind;
    match error.kind() {
        ErrorKind::AddrNotAvailable => "AddressNotAvailable".to_string(),
        ErrorKind::AddrInUse => "AddressAlreadyInUse".to_string(),
        ErrorKind::PermissionDenied => "AccessDenied".to_string(),
        ErrorKind::NetworkUnreachable => "NetworkUnreachable".to_string(),
        ErrorKind::HostUnreachable => "HostUnreachable".to_string(),
        ErrorKind::NetworkDown => "NetworkDown".to_string(),
        ErrorKind::TimedOut => "TimedOut".to_string(),
        ErrorKind::InvalidInput => "InvalidArgument".to_string(),
        other => format!("{other:?}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PortMappingState {
    Off = 0,
    /// Looking for the router / asking it.
    Working = 1,
    /// The router forwards the port to this PC.
    Mapped = 2,
    /// This PC has a public IPv4 address itself: there is no router to ask.
    Direct = 3,
    /// No UPnP router answered.
    NoRouter = 4,
    /// A router answered but would not forward the port.
    Refused = 5,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortMappingOptions {
    /// Where the SSDP search goes (tests point it at a stand-in router).
    pub ssdp_target: SocketAddrV4,
    /// The address to search from; `None` = this PC's LAN IPv4.
    pub bind: Option<Ipv4Addr>,
    pub search_timeout_ms: u64,
    pub call_timeout_ms: u64,
    /// Lease asked for; renewed well before it runs out, and gone by itself an hour after a crash.
    pub lease_seconds: u32,
    pub renew_seconds: u64,
    /// First retry after "no router" / a refusal; doubles up to 15 minutes.
    pub retry_seconds: u64,
    pub description: String,
}

impl Default for PortMappingOptions {
    fn default() -> Self {
        PortMappingOptions {
            ssdp_target: SSDP_MULTICAST,
            bind: None,
            search_timeout_ms: 2500,
            call_timeout_ms: 3000,
            lease_seconds: 3600,
            renew_seconds: 1200,
            retry_seconds: 60,
            description: "HdrCast".to_string(),
        }
    }
}

/// Keeps this PC's IPv4 P2P port forwarded on the home router while it lives (docs/design.md §8.7), so peers whose
/// routers pick a new port for every destination (symmetric NAT) can still reach it: their packets arrive from a
/// port this PC never sent to, which a router without the forward drops. Works on a background thread, renews
/// the lease, and deletes the forward on [`dispose`](Self::dispose) (also when dropped).
pub struct RouterPortMapping {
    inner: Arc<Inner>,
    thread: Mutex<Option<Worker>>,
}

struct Inner {
    port: u16,
    o: PortMappingOptions,
    log: LogFn,
    stop: CancelToken,
    state: Mutex<State>,
    changed: Callbacks<dyn Fn() + Send + Sync>,
}

struct State {
    gateway: Option<UpnpGateway>,
    client: Option<Ipv4Addr>,
    external_port: u16,
    lease: u32,
    last_logged: String,
    state: PortMappingState,
    external: Option<SocketAddrV4>,
    behind_another_nat: bool,
    router_name: String,
    problem: String,
}

impl RouterPortMapping {
    /// `port`: the IPv4 UDP port to forward (never 0). `options`: `None` = the real router.
    pub fn new(port: u16, options: Option<PortMappingOptions>, log: LogFn) -> RouterPortMapping {
        assert!(port != 0, "the port to forward must be a real port");
        RouterPortMapping {
            inner: Arc::new(Inner {
                port,
                o: options.unwrap_or_default(),
                log,
                stop: CancelToken::new(),
                state: Mutex::new(State {
                    gateway: None,
                    client: None,
                    external_port: 0,
                    lease: 0,
                    last_logged: String::new(),
                    state: PortMappingState::Off,
                    external: None,
                    behind_another_nat: false,
                    router_name: String::new(),
                    problem: String::new(),
                }),
                changed: Callbacks::new(),
            }),
            thread: Mutex::new(None),
        }
    }

    /// Called (on the mapping thread) whenever [`state`](Self::state) or [`external`](Self::external) changes.
    pub fn on_changed(&self, handler: impl Fn() + Send + Sync + 'static) {
        self.inner.changed.add(Arc::new(handler));
    }

    pub fn port(&self) -> u16 {
        self.inner.port
    }

    pub fn state(&self) -> PortMappingState {
        lock(&self.inner.state).state
    }

    /// The router's internet address and the forwarded port (or this PC's own public address).
    pub fn external(&self) -> Option<SocketAddrV4> {
        lock(&self.inner.state).external
    }

    /// The router's own internet address is private or CGNAT: the forward exists but cannot help.
    pub fn behind_another_nat(&self) -> bool {
        lock(&self.inner.state).behind_another_nat
    }

    pub fn router_name(&self) -> String {
        lock(&self.inner.state).router_name.clone()
    }

    /// Why there is no forward (NoRouter / Refused), in words.
    pub fn problem(&self) -> String {
        lock(&self.inner.state).problem.clone()
    }

    /// Peers can reach the port from anywhere, as far as the router is concerned.
    pub fn open(&self) -> bool {
        self.inner.open()
    }

    pub fn start(&self) {
        let mut thread = lock(&self.thread);
        if thread.is_some() || self.inner.stop.is_cancelled() {
            return;
        }
        let inner = self.inner.clone();
        *thread = Worker::spawn("p2p-upnp", move || inner.run()).ok();
    }

    /// Stops renewing and deletes the forward (up to a few seconds: it waits for the router).
    pub fn dispose(&self) {
        if self.inner.stop.is_cancelled() {
            return;
        }
        self.inner.stop.cancel();
        if let Some(thread) = lock(&self.thread).take() {
            thread.join(Duration::from_millis(self.inner.o.search_timeout_ms + self.inner.o.call_timeout_ms + 1000));
        }
        self.inner.remove();
        lock(&self.inner.state).state = PortMappingState::Off;
    }
}

impl Drop for RouterPortMapping {
    fn drop(&mut self) {
        self.dispose();
    }
}

impl std::fmt::Debug for RouterPortMapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouterPortMapping").field("port", &self.port()).field("state", &self.state()).field("external", &self.external()).finish()
    }
}

impl Inner {
    fn open(&self) -> bool {
        let state = lock(&self.state);
        (state.state == PortMappingState::Mapped && !state.behind_another_nat) || state.state == PortMappingState::Direct
    }

    fn run(&self) {
        let mut failures: u32 = 0;
        let ct = &self.stop;
        while !ct.is_cancelled() {
            let mapped = lock(&self.state).gateway.is_some();
            let ok = match catch_unwind(AssertUnwindSafe(|| if mapped { self.renew() } else { self.map(ct) })) {
                Ok(ok) => ok,
                Err(panic) => {
                    let what = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_default();
                    self.fail(PortMappingState::Refused, &format!("unexpected error: {what}"))
                }
            };
            if ct.is_cancelled() {
                break;
            }
            if lock(&self.state).state == PortMappingState::Direct {
                break; // nothing to keep alive
            }
            failures = if ok { 0 } else { failures + 1 };
            let wait = if ok { self.o.renew_seconds } else { (self.o.retry_seconds << (failures - 1).min(4)).min(900) };
            if ct.wait(Duration::from_secs(wait.max(1))) {
                break;
            }
        }
    }

    fn map(&self, ct: &CancelToken) -> bool {
        self.set(PortMappingState::Working, None, false, "", "");
        let Some(bind) = self.o.bind.or_else(local_addresses::lan_ipv4) else {
            return self.fail(PortMappingState::NoRouter, "this PC has no IPv4 network");
        };
        if local_addresses::is_public_ipv4(&IpAddr::V4(bind)) {
            self.set(PortMappingState::Direct, Some(SocketAddrV4::new(bind, self.port)), false, "", "");
            self.report(&format!("[p2p] router: this PC has the public IPv4 address {bind} itself, no router to open a port on"));
            return true;
        }

        let locations = match search(bind, self.o.ssdp_target, self.o.search_timeout_ms, ct) {
            Ok(locations) => locations,
            Err(e) => return self.fail(PortMappingState::NoRouter, &format!("the UPnP search failed ({})", socket_error_name(&e))),
        };
        if ct.is_cancelled() {
            return false;
        }
        if locations.is_empty() {
            return self.fail(PortMappingState::NoRouter, "no UPnP router answered (UPnP may be off in the router's settings)");
        }

        let mut refusal = String::new();
        for location in &locations {
            let gateways = match read_description(location, self.o.call_timeout_ms) {
                Ok(gateways) => gateways,
                Err(e) => {
                    refusal = e.message;
                    continue;
                }
            };
            for gateway in gateways {
                if ct.is_cancelled() {
                    return false;
                }
                // some routers do not say; the forward can still work
                let external = get_external_address(&gateway, self.o.call_timeout_ms).ok().flatten();
                let gateway_address = HttpUrl::parse(&gateway.control_url).and_then(|url| url.host().parse::<Ipv4Addr>().ok());
                let client = gateway_address.and_then(local_address_towards).unwrap_or(bind);
                match self.try_add(&gateway, client, ct) {
                    Ok(external_port) => {
                        if ct.is_cancelled() {
                            self.remove();
                            return false;
                        }
                        let behind = external.is_some_and(|e| !local_addresses::is_public_ipv4(&IpAddr::V4(e)));
                        self.set(PortMappingState::Mapped, external.map(|e| SocketAddrV4::new(e, external_port)), behind, &gateway.name, "");
                        let port = self.port;
                        let name = &gateway.name;
                        match external {
                            Some(external) if behind => self.report(&format!(
                                "[p2p] router: UDP {port} is forwarded on {name}, but that router's own internet address {external} is not public \
                                 (the provider or another router adds a second NAT), so it cannot make this PC reachable"
                            )),
                            _ => {
                                let place = match external {
                                    Some(external) => format!("{name}, internet address {external}:{external_port}"),
                                    None => name.clone(),
                                };
                                self.report(&format!("[p2p] router: UDP {port} forwarded to this PC ({place}); renewed while live, removed when HdrCast stops"));
                            }
                        }
                        return true;
                    }
                    Err(why) => refusal = why,
                }
            }
        }
        self.fail(PortMappingState::Refused, if refusal.is_empty() { "the router offers no WAN connection service" } else { &refusal })
    }

    /// Adds the forward on `gateway`; the outside port on success, else why not.
    fn try_add(&self, gateway: &UpnpGateway, client: Ipv4Addr, ct: &CancelToken) -> Result<u16, String> {
        let mut lease = self.o.lease_seconds;
        let mut ports = vec![self.port];
        ports.extend((0..3).map(|_| random_port()));
        let mut why = String::new();
        let mut i = 0;
        while i < ports.len() && !ct.is_cancelled() {
            match add_port_mapping(gateway, ports[i], client, self.port, &self.o.description, lease, self.o.call_timeout_ms) {
                Ok(()) => {
                    let mut state = lock(&self.state);
                    state.gateway = Some(gateway.clone());
                    state.client = Some(client);
                    state.external_port = ports[i];
                    state.lease = lease;
                    return Ok(ports[i]);
                }
                Err(e) if e.code == ERROR_ONLY_PERMANENT_LEASES && lease != 0 => {
                    lease = 0; // an old IGD v1 router: ask for a permanent forward and delete it on stop
                    continue;
                }
                Err(e) if e.code == ERROR_CONFLICT => {
                    why = format!("UDP {} is already forwarded to another device", ports[i]); // try another outside port
                }
                Err(e) => return Err(e.message), // includes 724 "same port values required" after a conflict
            }
            i += 1;
        }
        Err(why)
    }

    fn renew(&self) -> bool {
        let (gateway, client, port, lease) = {
            let state = lock(&self.state);
            (state.gateway.clone(), state.client, state.external_port, state.lease)
        };
        let (Some(gateway), Some(client)) = (gateway, client) else { return false };
        match add_port_mapping(&gateway, port, client, self.port, &self.o.description, lease, self.o.call_timeout_ms) {
            Ok(()) => true,
            Err(e) => {
                lock(&self.state).gateway = None; // start over: the router may have restarted or changed
                self.fail(PortMappingState::Refused, &format!("renewing the forward failed: {}", e.message))
            }
        }
    }

    fn fail(&self, state: PortMappingState, problem: &str) -> bool {
        let name = lock(&self.state).router_name.clone();
        self.set(state, None, false, &name, problem);
        self.report(&format!(
            "[p2p] router: could not forward UDP {} to this PC - {problem}. Friends whose routers change ports cannot reach this PC over IPv4 (docs/network.md §1.1)",
            self.port
        ));
        false
    }

    fn set(&self, new_state: PortMappingState, external: Option<SocketAddrV4>, behind: bool, name: &str, problem: &str) {
        let changed = {
            let mut state = lock(&self.state);
            let changed = state.state != new_state || state.external != external || state.behind_another_nat != behind;
            state.state = new_state;
            state.external = external;
            state.behind_another_nat = behind;
            if !name.is_empty() {
                state.router_name = name.to_string();
            }
            state.problem = problem.to_string();
            changed
        };
        if changed {
            self.changed.raise();
        }
    }

    /// Logs a line once: retries that end the same way stay quiet.
    fn report(&self, line: &str) {
        {
            let mut state = lock(&self.state);
            if state.last_logged == line {
                return;
            }
            state.last_logged = line.to_string();
        }
        (self.log)(line);
    }

    fn remove(&self) {
        let (gateway, port) = {
            let mut state = lock(&self.state);
            (state.gateway.take(), state.external_port)
        };
        let Some(gateway) = gateway else { return };
        match delete_port_mapping(&gateway, port, self.o.call_timeout_ms.min(2000)) {
            Ok(()) => (self.log)(&format!("[p2p] router: forward of UDP {port} removed")),
            Err(e) => (self.log)(&format!("[p2p] router: could not remove the forward of UDP {port} ({}); it expires by itself", e.message)),
        }
    }
}

/// An outside port to try when ours is taken: 20000..60000.
fn random_port() -> u16 {
    let bytes = keys::random_bytes(4);
    let value = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    20000 + (value % 40000) as u16
}
