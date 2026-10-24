//! What the test files share: the golden vectors, a stateful firewall and a UPnP router stand-in on loopback,
//! the real signaling server as a child process, and the lookup of ffmpeg / ffprobe.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hdrcast_core::group::keys::{self, GroupKeys};
use hdrcast_core::json::{self, Value};
use hdrcast_core::{base64url, tool_locator};
use hdrcast_p2p::upnp::PortMappingOptions;
use hdrcast_p2p::{CancelToken, LinkAddresses, PacketFilter, SignalingClient, UdpChannel};

// ---- golden vectors -------------------------------------------------------------------------------

pub mod vectors {
    use super::*;

    const TEXT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/vectors/group-v1.json"));

    pub fn root() -> Value {
        serde_json::from_str(TEXT).expect("group-v1.json is JSON")
    }

    /// A top-level string.
    pub fn s(key: &str) -> String {
        json::get_str(&root(), key).unwrap_or_else(|| panic!("no {key} in the vectors")).to_string()
    }

    pub fn get(path: &[&str]) -> Value {
        let mut at = root();
        for key in path {
            at = json::get(&at, key).unwrap_or_else(|| panic!("no {path:?} in the vectors")).clone();
        }
        at
    }

    pub fn keys() -> GroupKeys {
        GroupKeys::new(&base64url::decode(&s("groupKey")).unwrap()).unwrap()
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
}

pub fn random_keys() -> GroupKeys {
    GroupKeys::new(&keys::random_bytes(32)).unwrap()
}

// ---- waiting --------------------------------------------------------------------------------------

/// Polls `condition` every 10 ms for up to `ms`.
pub fn wait_for(condition: impl FnMut() -> bool, ms: u64) -> bool {
    wait_for_every(10, condition, ms)
}

/// Polls `condition` every `interval_ms` for up to `ms`. For conditions that ask the signaling server: it
/// allows one address ten requests a second, and a test that asks a hundred times a second gets everybody on
/// loopback (the host and the viewers under test) refused with 429.
pub fn wait_for_every(interval_ms: u64, mut condition: impl FnMut() -> bool, ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(interval_ms));
    }
    condition()
}

/// A child process (an encoder) that is killed when the test lets go of it, also when the test fails.
pub struct KillOnDrop(pub Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ---- loopback links -------------------------------------------------------------------------------

/// Stateful firewall stand-in: inbound only from endpoints this socket has sent to.
#[derive(Default)]
pub struct StatefulFirewall {
    opened: Mutex<HashSet<SocketAddr>>,
    pub block_everything: AtomicBool,
    pub dropped: AtomicUsize,
}

impl StatefulFirewall {
    pub fn new() -> Arc<StatefulFirewall> {
        Arc::new(StatefulFirewall::default())
    }

    pub fn blocking() -> Arc<StatefulFirewall> {
        let firewall = StatefulFirewall::new();
        firewall.block_everything.store(true, Ordering::SeqCst);
        firewall
    }

    pub fn dropped(&self) -> usize {
        self.dropped.load(Ordering::SeqCst)
    }
}

impl PacketFilter for StatefulFirewall {
    fn on_outbound(&self, to: SocketAddr) {
        self.opened.lock().unwrap().insert(to);
    }

    fn allow_inbound(&self, from: SocketAddr) -> bool {
        if !self.block_everything.load(Ordering::SeqCst) && self.opened.lock().unwrap().contains(&from) {
            return true;
        }
        self.dropped.fetch_add(1, Ordering::SeqCst);
        false
    }
}

pub fn has_ipv6_loopback() -> bool {
    UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).is_ok()
}

/// Both families on loopback (IPv6 when this machine has it), a firewall in front of each socket.
pub fn loopback(v6: &Arc<StatefulFirewall>, v4: &Arc<StatefulFirewall>) -> LinkAddresses {
    LinkAddresses {
        ipv6: has_ipv6_loopback().then_some(Ipv6Addr::LOCALHOST),
        ipv4_bind: Some(Ipv4Addr::LOCALHOST),
        ipv4_advertise: None,
        filter_v6: Some(v6.clone()),
        filter_v4: Some(v4.clone()),
    }
}

/// Both families on loopback behind fresh firewalls nobody looks at.
pub fn loopback_firewalled() -> LinkAddresses {
    loopback(&StatefulFirewall::new(), &StatefulFirewall::new())
}

/// IPv4 loopback only, no firewall.
pub fn loopback_v4() -> LinkAddresses {
    LinkAddresses { ipv6: None, ipv4_bind: Some(Ipv4Addr::LOCALHOST), ..LinkAddresses::default() }
}

/// Stands in for the remux child's SRT listener: echoes every datagram, prefixed with "RE:".
pub struct EchoChild {
    channel: UdpChannel,
    received: Arc<AtomicUsize>,
    last: Arc<Mutex<Vec<u8>>>,
}

impl EchoChild {
    pub fn new() -> EchoChild {
        let received = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(Mutex::new(Vec::new()));
        let (count, seen) = (received.clone(), last.clone());
        let channel = UdpChannel::bind(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            "echo-child",
            Box::new(move |io, data, from| {
                count.fetch_add(1, Ordering::SeqCst);
                *seen.lock().unwrap() = data.to_vec();
                let mut reply = b"RE:".to_vec();
                reply.extend_from_slice(data);
                io.send(&reply, from);
            }),
        )
        .unwrap();
        EchoChild { channel, received, last }
    }

    pub fn port(&self) -> u16 {
        self.channel.local_addr().port()
    }

    pub fn received(&self) -> usize {
        self.received.load(Ordering::SeqCst)
    }

    /// The datagram that arrived last.
    pub fn last(&self) -> Vec<u8> {
        self.last.lock().unwrap().clone()
    }
}

/// What "mpv" gets back for `text` sent to the viewer's local port ("" when nothing comes back in 3 s).
pub fn round_trip(viewer_local_port: u16, text: &str) -> String {
    let mpv = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    mpv.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    let mut buffer = [0u8; 2048];
    for _ in 0..10 {
        mpv.send_to(text.as_bytes(), (Ipv4Addr::LOCALHOST, viewer_local_port)).unwrap();
        if let Ok((n, _)) = mpv.recv_from(&mut buffer) {
            return String::from_utf8_lossy(&buffer[..n]).into_owned();
        }
    }
    String::new()
}

// ---- a UPnP router on loopback ----------------------------------------------------------------------

pub const FAKE_SERVICE_TYPE: &str = "urn:schemas-upnp-org:service:WANPPPConnection:1";

type Refusal = Box<dyn Fn(&HashMap<String, String>) -> Option<(i32, String)> + Send + Sync>;

/// A UPnP Internet Gateway Device stand-in on loopback: answers SSDP searches (sent to
/// [`ssdp_addr`](FakeRouter::ssdp_addr) instead of the multicast group) and the WANPPPConnection SOAP actions,
/// and records what it was asked.
pub struct FakeRouter {
    shared: Arc<RouterShared>,
    ssdp_addr: SocketAddrV4,
    http_port: u16,
    threads: Vec<JoinHandle<()>>,
}

pub struct RouterShared {
    closed: AtomicBool,
    pub external_ip: Mutex<String>,
    /// Returns (code, description) to fault an AddPortMapping, or `None` to accept it.
    pub refuse_add: Mutex<Option<Refusal>>,
    pub calls: Mutex<Vec<String>>,
    /// External port → "client:internalPort lease".
    pub mappings: Mutex<HashMap<u16, String>>,
    pub searches: AtomicUsize,
}

impl FakeRouter {
    pub fn new() -> FakeRouter {
        let ssdp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        // polled, not read with a timeout: on Windows a datagram arriving as a receive timeout expires is lost
        ssdp.set_nonblocking(true).unwrap();
        let http = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        http.set_nonblocking(true).unwrap();
        let SocketAddr::V4(ssdp_addr) = ssdp.local_addr().unwrap() else { unreachable!() };
        let http_port = http.local_addr().unwrap().port();
        let shared = Arc::new(RouterShared {
            closed: AtomicBool::new(false),
            external_ip: Mutex::new("203.0.113.7".to_string()),
            refuse_add: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
            mappings: Mutex::new(HashMap::new()),
            searches: AtomicUsize::new(0),
        });

        let for_ssdp = shared.clone();
        let ssdp_thread = thread::Builder::new()
            .name("fake-ssdp".into())
            .spawn(move || {
                let mut buffer = [0u8; 2048];
                while !for_ssdp.closed.load(Ordering::SeqCst) {
                    let Ok((n, from)) = ssdp.recv_from(&mut buffer) else {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    };
                    let text = String::from_utf8_lossy(&buffer[..n]).into_owned();
                    if !text.starts_with("M-SEARCH") {
                        continue;
                    }
                    for_ssdp.searches.fetch_add(1, Ordering::SeqCst);
                    let st = text.lines().find_map(|line| line.strip_prefix("ST:")).map(str::trim).unwrap_or("");
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nEXT:\r\nLOCATION: http://127.0.0.1:{http_port}/desc.xml\r\n\
                         SERVER: Fake/1.0 UPnP/1.0\r\nST: {st}\r\nUSN: uuid:fake\r\n\r\n"
                    );
                    let _ = ssdp.send_to(reply.as_bytes(), from);
                }
            })
            .unwrap();

        let for_http = shared.clone();
        let http_thread = thread::Builder::new()
            .name("fake-igd".into())
            .spawn(move || {
                while !for_http.closed.load(Ordering::SeqCst) {
                    match http.accept() {
                        Ok((stream, _)) => {
                            let _ = for_http.serve(stream);
                        }
                        Err(_) => thread::sleep(Duration::from_millis(5)),
                    }
                }
            })
            .unwrap();

        FakeRouter { shared, ssdp_addr, http_port, threads: vec![ssdp_thread, http_thread] }
    }

    pub fn ssdp_addr(&self) -> SocketAddrV4 {
        self.ssdp_addr
    }

    pub fn description_url(&self) -> String {
        format!("http://127.0.0.1:{}/desc.xml", self.http_port)
    }

    /// Mapping options that point at this router and never at the real one.
    pub fn options(&self) -> PortMappingOptions {
        self.options_renewing(600)
    }

    pub fn options_renewing(&self, renew_seconds: u64) -> PortMappingOptions {
        PortMappingOptions {
            ssdp_target: self.ssdp_addr,
            bind: Some(Ipv4Addr::LOCALHOST),
            search_timeout_ms: 1500,
            call_timeout_ms: 2000,
            retry_seconds: 1,
            renew_seconds,
            ..PortMappingOptions::default()
        }
    }

    pub fn set_external_ip(&self, ip: &str) {
        *self.shared.external_ip.lock().unwrap() = ip.to_string();
    }

    pub fn refuse_add(&self, decide: impl Fn(&HashMap<String, String>) -> Option<(i32, String)> + Send + Sync + 'static) {
        *self.shared.refuse_add.lock().unwrap() = Some(Box::new(decide));
    }

    /// How often `action` was called.
    pub fn count(&self, action: &str) -> usize {
        let prefix = format!("{action} ");
        self.shared.calls.lock().unwrap().iter().filter(|call| call.starts_with(&prefix)).count()
    }

    pub fn calls(&self) -> Vec<String> {
        self.shared.calls.lock().unwrap().clone()
    }

    pub fn mapping(&self, external_port: u16) -> Option<String> {
        self.shared.mappings.lock().unwrap().get(&external_port).cloned()
    }

    pub fn mappings(&self) -> HashMap<u16, String> {
        self.shared.mappings.lock().unwrap().clone()
    }

    pub fn searches(&self) -> usize {
        self.shared.searches.load(Ordering::SeqCst)
    }
}

impl Drop for FakeRouter {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl RouterShared {
    fn serve(&self, mut stream: TcpStream) -> std::io::Result<()> {
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_millis(3000)))?;
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte)? == 0 {
                return Ok(());
            }
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        let mut lines = head.split("\r\n");
        let request: Vec<&str> = lines.next().unwrap_or("").split(' ').collect();
        let (mut length, mut action) = (0usize, String::new());
        for line in lines {
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("content-length:") {
                length = value.trim().parse().unwrap_or(0);
            }
            if lower.starts_with("soapaction:") {
                action = line[line.find('#').map_or(0, |at| at + 1)..].trim().trim_matches('"').to_string();
            }
        }
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body)?;
        let body = String::from_utf8_lossy(&body).into_owned();

        if request.len() < 2 {
            return reply(&mut stream, 404, "");
        }
        if request[0] == "GET" && request[1] == "/desc.xml" {
            return reply(&mut stream, 200, &description());
        }
        if request[0] != "POST" || request[1] != "/ctl/wan" {
            return reply(&mut stream, 404, "");
        }

        let args = soap_arguments(&body);
        let arg = |name: &str| args.get(name).cloned().unwrap_or_default();
        match action.as_str() {
            "GetExternalIPAddress" => {
                self.calls.lock().unwrap().push("GetExternalIPAddress ".to_string());
                let ip = self.external_ip.lock().unwrap().clone();
                reply(&mut stream, 200, &soap(&action, &format!("<NewExternalIPAddress>{ip}</NewExternalIPAddress>")))
            }
            "AddPortMapping" => {
                self.calls.lock().unwrap().push(format!(
                    "AddPortMapping {}->{}:{} lease {}",
                    arg("NewExternalPort"),
                    arg("NewInternalClient"),
                    arg("NewInternalPort"),
                    arg("NewLeaseDuration")
                ));
                let refusal = self.refuse_add.lock().unwrap().as_ref().and_then(|decide| decide(&args));
                if let Some((code, text)) = refusal {
                    return reply(&mut stream, 500, &fault(code, &text));
                }
                self.mappings.lock().unwrap().insert(
                    arg("NewExternalPort").parse().unwrap_or(0),
                    format!("{}:{} {}", arg("NewInternalClient"), arg("NewInternalPort"), arg("NewLeaseDuration")),
                );
                reply(&mut stream, 200, &soap(&action, ""))
            }
            "DeletePortMapping" => {
                self.calls.lock().unwrap().push(format!("DeletePortMapping {}", arg("NewExternalPort")));
                let had = self.mappings.lock().unwrap().remove(&arg("NewExternalPort").parse().unwrap_or(0)).is_some();
                if had {
                    reply(&mut stream, 200, &soap(&action, ""))
                } else {
                    reply(&mut stream, 500, &fault(714, "NoSuchEntryInArray"))
                }
            }
            _ => reply(&mut stream, 500, &fault(401, "Invalid Action")),
        }
    }
}

/// `<NewName>value</NewName>` pairs of a SOAP request.
fn soap_arguments(body: &str) -> HashMap<String, String> {
    let mut args = HashMap::new();
    let mut rest = body;
    while let Some(start) = rest.find("<New") {
        let after = &rest[start + 1..];
        let Some(name_end) = after.find('>') else { break };
        let name = &after[..name_end];
        let value_and_rest = &after[name_end + 1..];
        let close = format!("</{name}>");
        match value_and_rest.find(&close) {
            Some(value_end) if !value_and_rest[..value_end].contains('<') => {
                args.insert(name.to_string(), value_and_rest[..value_end].to_string());
                rest = &value_and_rest[value_end + close.len()..];
            }
            _ => rest = value_and_rest,
        }
    }
    args
}

fn description() -> String {
    format!(
        "<?xml version=\"1.0\"?><root xmlns=\"urn:schemas-upnp-org:device-1-0\"><specVersion><major>1</major><minor>0</minor></specVersion>\
         <device><deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType><friendlyName>Fake Router</friendlyName>\
         <modelName>FR-100</modelName><deviceList><device><deviceType>urn:schemas-upnp-org:device:WANDevice:1</deviceType>\
         <serviceList><service><serviceType>urn:schemas-upnp-org:service:WANCommonInterfaceConfig:1</serviceType>\
         <controlURL>/ctl/common</controlURL></service></serviceList><deviceList><device>\
         <deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</deviceType><serviceList><service>\
         <serviceType>{FAKE_SERVICE_TYPE}</serviceType><serviceId>urn:upnp-org:serviceId:WANPPPConn1</serviceId>\
         <controlURL>/ctl/wan</controlURL><eventSubURL>/evt/wan</eventSubURL><SCPDURL>/wan.xml</SCPDURL>\
         </service></serviceList></device></deviceList></device></deviceList></device></root>"
    )
}

fn soap(action: &str, inner: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action}Response xmlns:u=\"{FAKE_SERVICE_TYPE}\">\
         {inner}</u:{action}Response></s:Body></s:Envelope>"
    )
}

fn fault(code: i32, description: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><s:Fault><faultcode>s:Client</faultcode>\
         <faultstring>UPnPError</faultstring><detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>{code}\
         </errorCode><errorDescription>{description}</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"
    )
}

fn reply(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let head = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

// ---- external tools ---------------------------------------------------------------------------------

/// The repository root (this crate lives in crates/p2p).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Says why a test does nothing. Written to the process's own stderr, past the test harness's capture, so the
/// note shows in a plain `cargo test` (where a skipped test otherwise looks exactly like one that ran).
pub fn skip(test: &str, why: &str) {
    let line = format!("SKIPPED {test}: {why}\n");
    let _ = std::io::stderr().write_all(line.as_bytes()); // in one piece: other tests print at the same time
}

/// ffmpeg / ffprobe: in the directory named by HDRCAST_TOOLS, then on PATH.
pub fn find_tool(tool: &str) -> Option<PathBuf> {
    let name = tool_locator::exe_name(tool);
    let mut directories: Vec<PathBuf> = std::env::var_os("HDRCAST_TOOLS").filter(|d| !d.is_empty()).map(PathBuf::from).into_iter().collect();
    directories.extend(std::env::var_os("PATH").iter().flat_map(std::env::split_paths));
    directories.into_iter().map(|directory| directory.join(&name)).find(|candidate| candidate.is_file())
}

/// ffmpeg and ffprobe, when both exist and ffmpeg has libx265 (what the media tests encode with).
pub fn media_tools() -> Option<(PathBuf, PathBuf)> {
    static TOOLS: OnceLock<Option<(PathBuf, PathBuf)>> = OnceLock::new();
    TOOLS
        .get_or_init(|| {
            let (ffmpeg, ffprobe) = (find_tool("ffmpeg")?, find_tool("ffprobe")?);
            run(&ffmpeg, &["-hide_banner", "-encoders"], 10_000).0.contains(" libx265 ").then_some((ffmpeg, ffprobe))
        })
        .clone()
}

/// A Python 3 that really runs ("python3" on Windows is usually the Store's placeholder, which only prints
/// where to get one).
pub fn python() -> Option<&'static str> {
    static PYTHON: OnceLock<Option<&'static str>> = OnceLock::new();
    *PYTHON.get_or_init(|| {
        ["python3", "python"].into_iter().find(|candidate| {
            Command::new(candidate)
                .arg("--version")
                .stdin(Stdio::null())
                .stderr(Stdio::piped())
                .stdout(Stdio::piped())
                .output()
                .is_ok_and(|output| output.status.success() && String::from_utf8_lossy(&[output.stdout, output.stderr].concat()).contains("Python 3"))
        })
    })
}

/// Runs a program to its end (killing it after `timeout_ms`); returns stdout + stderr and the exit code.
pub fn run(exe: &Path, args: &[&str], timeout_ms: u64) -> (String, Option<i32>) {
    let mut child = Command::new(exe).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stdout.read_to_end(&mut text);
        text
    });
    let err = thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stderr.read_to_end(&mut text);
        text
    });
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let mut text = out.join().unwrap_or_default();
    text.extend(err.join().unwrap_or_default());
    (String::from_utf8_lossy(&text).into_owned(), code)
}

pub fn free_tcp_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port()
}

pub fn free_udp_port() -> u16 {
    UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port()
}

// ---- the real signaling server ------------------------------------------------------------------------

/// Runs signal/hdrcast_signal.py on loopback for the duration of a test.
pub struct SignalServer {
    process: Child,
    state_dir: PathBuf,
    pub http_port: u16,
    pub echo_port: u16,
}

impl SignalServer {
    /// `None` (after printing why) when there is no Python to run it with.
    pub fn try_start(test: &str) -> Option<SignalServer> {
        let Some(python) = python() else {
            skip(test, "needs Python 3 (python3 or python on PATH) for signal/hdrcast_signal.py");
            return None;
        };
        let script = repo_root().join("signal").join("hdrcast_signal.py");
        if !script.is_file() {
            skip(test, "signal/hdrcast_signal.py is not there");
            return None;
        }
        // The ports are free now and bound by the server a moment later; should another process take one in
        // between, the server does not come up and another pair is tried.
        for _attempt in 0..3 {
            let (http_port, echo_port) = (free_tcp_port(), free_udp_port());
            let state_dir = std::env::temp_dir().join(format!("hdrcast-signal-{}", keys::new_id()));
            std::fs::create_dir_all(&state_dir).unwrap();
            let process = Command::new(python)
                .arg(&script)
                .args(["--listen", &format!("127.0.0.1:{http_port}"), "--echo-port", &echo_port.to_string(), "--state-dir"])
                .arg(&state_dir)
                .arg("--quiet")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("python starts");
            let mut server = SignalServer { process, state_dir, http_port, echo_port };
            let client = SignalingClient::new(&server.url(), &keys::new_id(), &random_keys(), 1000);
            // Generous: with a dozen tests starting servers at once (the script on a network share) one start took
            // over 8 s here. A server that exits is noticed at once anyway.
            let deadline = Instant::now() + Duration::from_millis(20_000);
            while Instant::now() < deadline {
                if !matches!(server.process.try_wait(), Ok(None)) {
                    break; // it exited: the port was taken after all
                }
                if client.health(&CancelToken::never()) {
                    return Some(server);
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
        panic!("the signaling server did not come up");
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.http_port)
    }

    pub fn echo_addr(&self) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.echo_port)
    }
}

impl Drop for SignalServer {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
        let _ = std::fs::remove_dir_all(&self.state_dir);
    }
}
