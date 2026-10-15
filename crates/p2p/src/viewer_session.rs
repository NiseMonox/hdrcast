//! Watching one host (receiver side). Keeps a punched path to the host alive underneath mpv: joins, punches,
//! and when the path drops, joins again with a new session on the same local port, so mpv's retry loop simply
//! reconnects to srt://127.0.0.1:[`local_port`](ViewerSession::local_port). When something fails the status
//! explains whose side lacks what (see [`p2p_diagnostics`]).

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use hdrcast_core::group::keys;
use hdrcast_core::group::now_ms;
use hdrcast_core::messages::{self, JoinAnswer, JoinRequest};
use hdrcast_core::{endpoint, GroupConfig, GroupKeys, SignedEnvelope};

use crate::cancel::CancelToken;
use crate::echo_protocol;
use crate::link_base::{LinkAddresses, SOURCE_ECHO, SOURCE_ROUTER};
use crate::p2p_diagnostics;
use crate::p2p_host::resolve_ipv4;
use crate::peer_session::{LinkState, PUNCH_DEADLINE_US};
use crate::signaling_client::{SignalingClient, SignalingError};
use crate::upnp::{PortMappingOptions, RouterPortMapping};
use crate::util::{lock, Callbacks, LogFn, Worker};
use crate::viewer_link::{ViewerLink, ViewerSideHandler};
use crate::StartError;

#[derive(Clone)]
pub struct ViewerSessionOptions {
    pub group: GroupConfig,
    pub host_id: String,
    /// SRT latency this viewer wants (its region preset).
    pub latency_ms: i32,
    pub overhead_percent: i32,
    /// `None` = detect this PC's stable IPv6 and LAN IPv4.
    pub addresses: Option<LinkAddresses>,
    pub discover_public_ipv4: bool,
    pub echo_port: u16,
    /// Fixed local port for mpv (0 = any free port).
    pub local_port: u16,
    /// Forward this PC's IPv4 port on the router (UPnP), so hosts whose routers change ports can reach it.
    pub map_router_port: bool,
    /// `None` = the real router (tests point it at a stand-in).
    pub router_mapping: Option<PortMappingOptions>,
    /// Ask the host for remote control: the join carries "rc", which also tells the host that this viewer
    /// understands the side channel (docs/design.md §8.9).
    pub request_control: bool,
}

impl Default for ViewerSessionOptions {
    fn default() -> Self {
        ViewerSessionOptions {
            group: GroupConfig::default(),
            host_id: String::new(),
            latency_ms: 1500,
            overhead_percent: 25,
            addresses: None,
            discover_public_ipv4: true,
            echo_port: echo_protocol::DEFAULT_PORT,
            local_port: 0,
            map_router_port: true,
            router_mapping: None,
            request_control: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewerSessionState {
    Starting = 0,
    /// Asking the host through the signaling server.
    Joining = 1,
    /// Opening the path.
    Punching = 2,
    /// The path works; mpv can play.
    Connected = 3,
    /// Not connected; trying again after a pause (the status says why).
    Waiting = 4,
    Stopped = 5,
}

pub struct ViewerSession {
    inner: Arc<Inner>,
}

struct State {
    state: ViewerSessionState,
    status: String,
    host_name: String,
    echo_candidate: String,
    ever_connected: bool,
    ipv4_only_failure: bool,
}

struct Inner {
    o: ViewerSessionOptions,
    log: LogFn,
    state: Mutex<State>,
    cancel: CancelToken,
    started: AtomicBool,
    host_offers_control: AtomicBool,
    keys: OnceLock<GroupKeys>,
    signal: OnceLock<SignalingClient>,
    link: OnceLock<ViewerLink>,
    router: OnceLock<RouterPortMapping>,
    thread: Mutex<Option<Worker>>,
    changed: Callbacks<dyn Fn() + Send + Sync>,
    side_handler: Mutex<Option<ViewerSideHandler>>,
}

impl ViewerSession {
    pub fn new(options: ViewerSessionOptions, log: impl Fn(&str) + Send + Sync + 'static) -> ViewerSession {
        ViewerSession {
            inner: Arc::new(Inner {
                o: options,
                log: Arc::new(log),
                state: Mutex::new(State {
                    state: ViewerSessionState::Starting,
                    status: "Starting".to_string(),
                    host_name: String::new(),
                    echo_candidate: String::new(),
                    ever_connected: false,
                    ipv4_only_failure: false,
                }),
                cancel: CancelToken::new(),
                started: AtomicBool::new(false),
                host_offers_control: AtomicBool::new(false),
                keys: OnceLock::new(),
                signal: OnceLock::new(),
                link: OnceLock::new(),
                router: OnceLock::new(),
                thread: Mutex::new(None),
                changed: Callbacks::new(),
                side_handler: Mutex::new(None),
            }),
        }
    }

    /// The state, the status line, the path or the router forward changed (called on a background thread).
    pub fn on_changed(&self, handler: impl Fn() + Send + Sync + 'static) {
        self.inner.changed.add(Arc::new(handler));
    }

    pub fn state(&self) -> ViewerSessionState {
        lock(&self.inner.state).state
    }

    pub fn status(&self) -> String {
        lock(&self.inner.state).status.clone()
    }

    pub fn host_name(&self) -> String {
        lock(&self.inner.state).host_name.clone()
    }

    /// mpv's port on 127.0.0.1; 0 before [`start`](Self::start).
    pub fn local_port(&self) -> u16 {
        self.inner.link.get().map_or(0, ViewerLink::local_port)
    }

    pub fn path_description(&self) -> String {
        self.inner.link.get().map(ViewerLink::describe_path).unwrap_or_default()
    }

    pub fn ever_connected(&self) -> bool {
        lock(&self.inner.state).ever_connected
    }

    /// The group's SRT passphrase for mpv's URL; "" before [`start`](Self::start).
    pub fn srt_passphrase(&self) -> String {
        self.inner.keys.get().map(|keys| keys.srt_passphrase().to_string()).unwrap_or_default()
    }

    /// The router forward of this PC's IPv4 port (`None` when not asked for, or before [`start`](Self::start)).
    pub fn router_mapping(&self) -> Option<&RouterPortMapping> {
        self.inner.router.get()
    }

    /// The last attempt failed on IPv4 alone and the host's side is not known to be open: if the host's router
    /// changes ports, only this PC taking unsolicited IPv4 (router forward + firewall rule) can help.
    pub fn this_side_could_help(&self) -> bool {
        lock(&self.inner.state).ipv4_only_failure
    }

    /// The id of the session with the host (the latest join's; it changes with every re-join). `None` before
    /// the first join. Remote control derives its key from it (`GroupKeys::control_key`).
    pub fn sid(&self) -> Option<String> {
        self.inner.link.get().and_then(ViewerLink::session_id)
    }

    /// The live record of the host being watched said it offers remote control ("rc"), as of the latest join.
    pub fn host_offers_control(&self) -> bool {
        self.inner.host_offers_control.load(Ordering::Acquire)
    }

    /// Who gets what the host sends on the side channel, on a network thread (docs/design.md §8.9). May be set
    /// before or after [`start`](Self::start); replaces the previous handler.
    pub fn set_side_handler(&self, handler: ViewerSideHandler) {
        *lock(&self.inner.side_handler) = Some(handler);
    }

    /// Sends `payload` (1..=1200 bytes) to the host on the side channel. False unless the host's live record
    /// offered remote control (a 0.2.4 host would hand the datagram to its ffmpeg), and when the session is not
    /// connected or the payload is out of range.
    pub fn send_side(&self, payload: &[u8]) -> bool {
        self.host_offers_control() && self.inner.link.get().is_some_and(|link| link.send_side(payload))
    }

    /// Opens the sockets, asks the router, and starts joining the host (and re-joining, until stopped).
    pub fn start(&self) -> Result<(), StartError> {
        self.inner.start()
    }

    /// Ends the session (BYE to the host), closes the sockets, removes the router forward. Also done when dropped.
    pub fn stop(&self) {
        self.inner.stop();
    }
}

impl Drop for ViewerSession {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Inner {
    fn start(self: &Arc<Self>) -> Result<(), StartError> {
        if self.started.swap(true, Ordering::AcqRel) {
            return Err(StartError::AlreadyStarted);
        }
        let keys = self.o.group.keys()?;
        let signal = SignalingClient::for_group(&self.o.group)?;
        let link = ViewerLink::new(&keys, &self.o.addresses.clone().unwrap_or_else(LinkAddresses::detect), self.o.local_port)?;
        let weak = Arc::downgrade(self);
        link.on_state_changed({
            let weak = weak.clone();
            move |_| {
                if let Some(session) = weak.upgrade() {
                    session.changed.raise();
                }
            }
        });
        link.set_side_handler({
            let weak = weak.clone();
            Arc::new(move |payload| {
                let handler = weak.upgrade().and_then(|session| lock(&session.side_handler).clone());
                if let Some(handler) = handler {
                    handler(payload);
                }
            })
        });
        (self.log)(&format!("[p2p] this PC offers: {}{}", link.candidates().join(", "), if link.has_ipv6() { "" } else { "  (no IPv6)" }));
        let ipv4_port = link.ipv4_port();
        let _ = self.keys.set(keys);
        let _ = self.signal.set(signal);
        let _ = self.link.set(link);

        if let (true, Some(port)) = (self.o.map_router_port, ipv4_port) {
            let router = RouterPortMapping::new(port, self.o.router_mapping.clone(), self.log.clone());
            router.on_changed({
                let weak = weak.clone();
                move || {
                    let Some(session) = weak.upgrade() else { return };
                    if let (Some(router), Some(link)) = (session.router.get(), session.link.get()) {
                        let candidate = router.external().filter(|_| router.open()).map(|external| endpoint::format(&SocketAddr::V4(external)));
                        link.set_candidate(SOURCE_ROUTER, candidate);
                    }
                    session.changed.raise();
                }
            });
            let _ = self.router.set(router);
            if let Some(router) = self.router.get() {
                router.start();
            }
        }

        let session = self.clone();
        *lock(&self.thread) = Some(Worker::spawn("p2p-viewer", move || session.run())?);
        Ok(())
    }

    fn host_name(&self) -> String {
        lock(&self.state).host_name.clone()
    }

    fn run(&self) {
        let ct = &self.cancel;
        let Some(link) = self.link.get() else { return };
        let mut echo_tried = false;
        let mut failures: u32 = 0;
        while !ct.is_cancelled() {
            if self.o.discover_public_ipv4 {
                // before every join: the echo may have been opened meanwhile, and the router may have dropped the
                // mapping while we waited, so the address the host would punch towards could be stale
                self.discover_public_ipv4(!echo_tried);
                echo_tried = true;
                if ct.is_cancelled() {
                    return;
                }
            }
            let wait_reason = match self.join_once(ct) {
                Ok(reason) => reason,
                Err(SignalingError::Cancelled) => return,
                Err(e) => {
                    if ct.is_cancelled() {
                        return;
                    }
                    if e.is_unreachable() {
                        p2p_diagnostics::SIGNALING_DOWN.to_string()
                    } else {
                        format!("Signaling error: {e}")
                    }
                }
            };
            if ct.is_cancelled() {
                return;
            }
            if wait_reason.is_empty() {
                failures = 0;
                // Connected: stay until the path ends.
                while !ct.is_cancelled() && link.state() == LinkState::Connected {
                    ct.wait_ms(200);
                }
                if ct.is_cancelled() {
                    return;
                }
                let reason = link.reason();
                let lower = reason.to_ascii_lowercase();
                let ended = lower.contains("ended") || lower.contains("stopped");
                let host = self.host_name();
                (self.log)(&format!("[p2p] path to {host} closed: {reason}"));
                self.set_state(
                    ViewerSessionState::Waiting,
                    if ended { format!("The stream ended - waiting for {host} to go live again") } else { format!("Connection lost ({reason}) - reconnecting") },
                );
                ct.wait_ms(if ended { 5000 } else { 1000 });
                continue;
            }
            failures += 1;
            let delay: u64 = match failures {
                0 | 1 => 3000,
                2 => 6000,
                3 => 12000,
                _ => 30000,
            };
            self.set_state(ViewerSessionState::Waiting, format!("{wait_reason} (retrying in {} s)", delay / 1000));
            ct.wait_ms(delay);
        }
    }

    /// One join attempt. Returns "" when connected, otherwise why not.
    fn join_once(&self, ct: &CancelToken) -> Result<String, SignalingError> {
        let g = &self.o.group;
        let (Some(keys), Some(signal), Some(link)) = (self.keys.get(), self.signal.get(), self.link.get()) else {
            return Err(SignalingError::Cancelled);
        };
        self.set_state(ViewerSessionState::Joining, "Looking for the host".to_string());
        let Some(entry) = signal.get_live(ct)?.into_iter().find(|entry| entry.record.host_id == self.o.host_id) else {
            self.host_offers_control.store(false, Ordering::Release);
            return Ok("Not live right now".to_string());
        };
        let record = entry.record;
        lock(&self.state).host_name = record.name.clone();
        self.host_offers_control.store(record.remote_control == Some(true), Ordering::Release);
        if record.max_viewers > 0 && record.viewers >= record.max_viewers {
            return Ok(format!("{} is full ({} viewers)", record.name, record.max_viewers));
        }

        let sid = keys::new_id();
        // punch right away with the advertised candidates
        if link.start_session(&sid, &record.candidates).is_err() {
            return Ok("Not live right now".to_string()); // cannot happen: the id was just made
        }
        let join = JoinRequest {
            session_id: sid.clone(),
            viewer_id: g.peer_id.clone(),
            name: g.display_name.clone(),
            time_ms: now_ms(),
            candidates: link.candidates(),
            latency_ms: self.o.latency_ms,
            overhead_percent: self.o.overhead_percent,
            remote_control: self.o.request_control,
        };
        let envelope = SignedEnvelope::seal(keys, messages::KIND_JOIN, &g.group_id, &self.o.host_id, &join.to_payload());
        match signal.post_join(&self.o.host_id, &envelope, ct) {
            Ok(_) => {}
            Err(e) if e.status() == 404 => {
                link.end_session("host offline");
                return Ok("Not live right now".to_string());
            }
            Err(e) if e.status() == 429 => {
                link.end_session("busy");
                return Ok(format!("{} is busy, too many people joining at once", record.name));
            }
            Err(e) => return Err(e),
        }

        self.set_state(ViewerSessionState::Punching, format!("Connecting to {}", record.name));
        let answer = signal
            .wait_answer(&sid, 20, ct)?
            .and_then(|envelope| envelope.try_open(keys, messages::KIND_ANSWER, &g.group_id, &sid))
            .and_then(|payload| JoinAnswer::from_payload(&payload, &sid));
        if let Some(answer) = answer {
            if !answer.ok {
                link.end_session("refused");
                return Ok(match answer.reason.as_str() {
                    JoinAnswer::REASON_FULL => format!("{} is full", record.name),
                    JoinAnswer::REASON_STOPPED => format!("{} is stopping", record.name),
                    _ => format!("{} could not take another viewer", record.name),
                });
            }
            link.add_host_candidates(&answer.candidates);
        }

        let deadline = Instant::now() + Duration::from_millis(PUNCH_DEADLINE_US as u64 / 1000 + 1000);
        while !ct.is_cancelled() && Instant::now() < deadline {
            match link.state() {
                LinkState::Connected => {
                    {
                        let mut state = lock(&self.state);
                        state.ever_connected = true;
                        state.ipv4_only_failure = false;
                    }
                    self.set_state(ViewerSessionState::Connected, format!("Connected to {} ({})", record.name, link.describe_path()));
                    (self.log)(&format!(
                        "[p2p] connected to {}: {}{}",
                        record.name,
                        link.describe_path(),
                        if link.path_learned() { " (a port the host's router picked for this PC, not the advertised one: it changes ports per destination)" } else { "" }
                    ));
                    return Ok(String::new());
                }
                LinkState::Failed | LinkState::Closed => break,
                LinkState::Punching => {}
            }
            ct.wait_ms(100);
        }
        if ct.is_cancelled() {
            return Err(SignalingError::Cancelled);
        }
        let reason = link.reason(); // before end_session replaces it
        link.end_session("no path");
        let mine = link.candidates();
        let ipv4_tried = !(link.has_ipv6() && p2p_diagnostics::has_ipv6(&record.candidates))
            && p2p_diagnostics::has_public_ipv4(&record.candidates)
            && p2p_diagnostics::has_public_ipv4(&mine);
        lock(&self.state).ipv4_only_failure = ipv4_tried && record.open_ipv4 != Some(true);
        let why = p2p_diagnostics::explain(link.has_ipv6(), &mine, &record.candidates, record.open_ipv4);
        (self.log)(&format!(
            "[p2p] could not reach {}: {why} ({}host offers {}{})",
            record.name,
            if reason.is_empty() { String::new() } else { format!("{reason}; ") },
            record.candidates.join(", "),
            match record.open_ipv4 {
                Some(open) => format!(", host IPv4 {} to every router", if open { "open" } else { "not open" }),
                None => String::new(),
            }
        ));
        Ok(why)
    }

    /// This PC's public IPv4 mapping for the IPv4 attempt (IPv6 does not need it). False = no answer yet.
    fn discover_public_ipv4(&self, log_failure: bool) -> bool {
        let (Some(signal), Some(link)) = (self.signal.get(), self.link.get()) else { return false };
        let server = match resolve_ipv4(&signal.host()) {
            Ok(Some(server)) => server,
            Ok(None) => return true, // IPv6-only signaling server: nothing to ask
            Err(_) => return false,
        };
        let echo = SocketAddr::new(IpAddr::V4(server), self.o.echo_port);
        let Some(mapped) = link.discover_public_ipv4(echo, 1500) else {
            if log_failure {
                (self.log)(&format!(
                    "[p2p] public IPv4 check: no answer from the signaling server's UDP echo at {} (is UDP {} open on the server?); \
                     without it IPv4 only works inside one network",
                    endpoint::format(&echo),
                    self.o.echo_port
                ));
            }
            return false;
        };
        let candidate = endpoint::format(&mapped);
        link.set_candidate(SOURCE_ECHO, Some(candidate.clone()));
        let previous = std::mem::replace(&mut lock(&self.state).echo_candidate, candidate.clone());
        if candidate != previous {
            (self.log)(&format!("[p2p] public IPv4 mapping: {candidate}{}", if previous.is_empty() { String::new() } else { format!(" (was {previous})") }));
        }
        true
    }

    fn set_state(&self, new_state: ViewerSessionState, status: String) {
        let changed = {
            let mut state = lock(&self.state);
            let changed = state.state != new_state || state.status != status;
            state.state = new_state;
            state.status = status;
            changed
        };
        if changed {
            self.changed.raise();
        }
    }

    fn stop(&self) {
        if self.cancel.is_cancelled() {
            return;
        }
        self.cancel.cancel();
        if let Some(thread) = lock(&self.thread).take() {
            thread.join(Duration::from_millis(3000));
        }
        if let Some(link) = self.link.get() {
            link.dispose();
        }
        if let Some(router) = self.router.get() {
            router.dispose(); // deletes the router forward (waits for the router, a few seconds at most)
        }
        self.set_state(ViewerSessionState::Stopped, "Stopped".to_string());
    }
}
