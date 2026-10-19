//! Watching one host (receiver side). Keeps a punched path to the host alive underneath mpv: joins, punches,
//! and when the path drops, joins again with a new session on the same local port, so mpv's retry loop simply
//! reconnects to srt://127.0.0.1:[`local_port`](ViewerSession::local_port). When something fails the status
//! explains whose side lacks what (see [`p2p_diagnostics`]).

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use hdrcast_core::group::device::{self, DeviceKey, Exchange};
use hdrcast_core::group::keys;
use hdrcast_core::group::now_ms;
use hdrcast_core::messages::{self, JoinAnswer, JoinRequest};
use hdrcast_core::{endpoint, GroupConfig, GroupKeys, LiveRecord, SignedEnvelope};

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
    /// This computer's device key: every join is signed with it, so a host can recognise this computer and
    /// agree on a remote-control key with it (docs/design.md §8.10). `None` = unsigned joins, as before 0.3.1.
    pub device: Option<Arc<DeviceKey>>,
    /// How long to wait for the answer of a host that may ask its user first (one that advertises a device
    /// key). Older hosts answer at once or not at all, and get 20 s as before.
    pub answer_wait_ms: u64,
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
            device: None,
            answer_wait_ms: 60_000,
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
    /// The host has not answered yet: it may be asking its user, or starting its stream (0.3.1 hosts).
    Approval = 6,
    /// The host said no, or nobody there answered: not trying again (the status says which).
    Refused = 7,
}

/// How one join attempt ended.
enum Attempt {
    Connected,
    /// Not this time (why): try again after a pause.
    Retry(String),
    /// The host refused (why): stop.
    Refused(String),
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
    /// From the host's live record as of the latest join.
    host_device_key: Option<String>,
    host_on_demand: bool,
    /// The remote-control key agreed with the host's device, and the session it belongs to.
    control: Option<(String, [u8; 32])>,
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
                    host_device_key: None,
                    host_on_demand: false,
                    control: None,
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

    /// A fresher live record of the host offers remote control: its user allowed it while this session ran.
    /// From now on the side channel may be used (only a host that speaks it ever offers it). Only ever turns
    /// the offer on; raises the change event when it did.
    pub fn host_now_offers_control(&self) {
        if self.inner.started.load(Ordering::Acquire) && !self.inner.host_offers_control.swap(true, Ordering::AcqRel) {
            (self.inner.log)("[p2p] the host now offers remote control");
            self.inner.changed.raise();
        }
    }

    /// The remote-control key agreed with the host's device for session `sid` (docs/design.md §8.10). `None`
    /// when the host is older than 0.3.1, or `sid` is not the current session: use the group-derived key then.
    pub fn control_key(&self, sid: &str) -> Option<[u8; 32]> {
        lock(&self.inner.state).control.as_ref().filter(|(current, _)| current == sid).map(|(_, key)| *key)
    }

    /// The host's device key from its live record (as of the latest join); `None` for hosts older than 0.3.1.
    pub fn host_device_key(&self) -> Option<String> {
        lock(&self.inner.state).host_device_key.clone()
    }

    /// The host streams on request rather than broadcasting (as of the latest join).
    pub fn host_on_demand(&self) -> bool {
        lock(&self.inner.state).host_on_demand
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
                Ok(Attempt::Connected) => String::new(),
                Ok(Attempt::Retry(reason)) => reason,
                Ok(Attempt::Refused(reason)) => {
                    if !ct.is_cancelled() {
                        (self.log)(&format!("[p2p] {reason}"));
                        self.set_state(ViewerSessionState::Refused, reason);
                    }
                    return;
                }
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
                let status = match (ended, lock(&self.state).host_on_demand) {
                    (true, true) => format!("{host} ended the session - asking again"),
                    (true, false) => format!("The stream ended - waiting for {host} to go live again"),
                    (false, _) => format!("Connection lost ({reason}) - reconnecting"),
                };
                self.set_state(ViewerSessionState::Waiting, status);
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

    /// One join attempt.
    fn join_once(&self, ct: &CancelToken) -> Result<Attempt, SignalingError> {
        let g = &self.o.group;
        let (Some(keys), Some(signal), Some(link)) = (self.keys.get(), self.signal.get(), self.link.get()) else {
            return Err(SignalingError::Cancelled);
        };
        self.set_state(ViewerSessionState::Joining, "Looking for the host".to_string());
        let Some(entry) = signal.get_live(ct)?.into_iter().find(|entry| entry.record.host_id == self.o.host_id) else {
            self.host_offers_control.store(false, Ordering::Release);
            return Ok(Attempt::Retry("Not live right now".to_string()));
        };
        let record = entry.record;
        {
            let mut state = lock(&self.state);
            state.host_name = record.name.clone();
            state.host_device_key = record.device_key.clone();
            state.host_on_demand = record.on_demand;
            state.control = None;
        }
        self.host_offers_control.store(record.remote_control == Some(true), Ordering::Release);
        if record.max_viewers > 0 && record.viewers >= record.max_viewers {
            return Ok(Attempt::Retry(format!("{} is full ({} viewers)", record.name, record.max_viewers)));
        }

        let sid = keys::new_id();
        // punch right away with the advertised candidates
        if link.start_session(&sid, &record.candidates).is_err() {
            return Ok(Attempt::Retry("Not live right now".to_string())); // cannot happen: the id was just made
        }
        let punching_since = Instant::now();
        let mut join = JoinRequest {
            session_id: sid.clone(),
            viewer_id: g.peer_id.clone(),
            name: g.display_name.clone(),
            time_ms: now_ms(),
            candidates: link.candidates(),
            latency_ms: self.o.latency_ms,
            overhead_percent: self.o.overhead_percent,
            remote_control: self.o.request_control,
            device: None,
        };
        let exchange = self.o.device.as_ref().map(|device| {
            let exchange = Exchange::new();
            device::sign_join(device, &exchange, &g.group_id, &self.o.host_id, &mut join);
            exchange
        });
        let envelope = SignedEnvelope::seal(keys, messages::KIND_JOIN, &g.group_id, &self.o.host_id, &join.to_payload());
        match signal.post_join(&self.o.host_id, &envelope, ct) {
            Ok(_) => {}
            Err(e) if e.status() == 404 => {
                link.end_session("host offline");
                return Ok(Attempt::Retry("Not live right now".to_string()));
            }
            Err(e) if e.status() == 429 => {
                link.end_session("busy");
                return Ok(Attempt::Retry(format!("{} is busy, too many people joining at once", record.name)));
            }
            Err(e) => return Err(e),
        }

        self.set_state(ViewerSessionState::Punching, format!("Connecting to {}", record.name));
        let my_exchange = exchange.as_ref().map_or("", |e| e.public_key());
        if let Some(answer) = self.wait_for_answer(&sid, &record, my_exchange, ct)? {
            if !answer.ok {
                link.end_session("refused");
                return Ok(match answer.reason.as_str() {
                    JoinAnswer::REASON_FULL => Attempt::Retry(format!("{} is full", record.name)),
                    JoinAnswer::REASON_STOPPED => Attempt::Retry(format!("{} is stopping", record.name)),
                    JoinAnswer::REASON_DENIED => Attempt::Refused(format!("{} did not let you in", record.name)),
                    JoinAnswer::REASON_TIMEOUT => Attempt::Refused(format!("Nobody at {} answered your request", record.name)),
                    _ => Attempt::Retry(format!("{} could not take another viewer", record.name)),
                });
            }
            // the key remote control seals its datagrams with, when both sides exchanged one
            let theirs = answer.device.as_ref().map(|proof| proof.exchange.as_str()).filter(|e| !e.is_empty());
            if let (Some(exchange), Some(theirs)) = (&exchange, theirs) {
                let key = device::parse_exchange_key(theirs)
                    .and_then(|bytes| exchange.agree(&bytes))
                    .map(|shared| device::session_control_key(&shared, &sid, exchange.public_key(), theirs));
                lock(&self.state).control = key.map(|key| (sid.clone(), key));
            }
            if punching_since.elapsed() > Duration::from_secs(5) {
                // A late yes (the host asked its user, or started its stream first): the first punches are about
                // to give up, so start over now that the host punches too.
                let mut candidates = record.candidates.clone();
                candidates.extend(answer.candidates.iter().cloned());
                let _ = link.start_session(&sid, &candidates);
            } else {
                link.add_host_candidates(&answer.candidates);
            }
            self.set_state(ViewerSessionState::Punching, format!("Connecting to {}", record.name));
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
                    return Ok(Attempt::Connected);
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
        Ok(Attempt::Retry(why))
    }

    /// The host's answer to the join of session `sid`; `None` when none came in time.
    ///
    /// A host that advertises a device key (0.3.1 on) may ask its user, or start its stream, before it answers,
    /// so it gets [`answer_wait_ms`](ViewerSessionOptions::answer_wait_ms), and only an answer signed with that key
    /// counts: anyone in the group can put an answer for a session, and one that is not the host's would hand
    /// remote control a key its author knows. Older hosts answer at once or not at all and get 20 s, as before.
    fn wait_for_answer(&self, sid: &str, record: &LiveRecord, my_exchange: &str, ct: &CancelToken) -> Result<Option<JoinAnswer>, SignalingError> {
        let g = &self.o.group;
        let (Some(keys), Some(signal)) = (self.keys.get(), self.signal.get()) else { return Err(SignalingError::Cancelled) };
        let patient = record.device_key.is_some();
        let limit = Duration::from_millis(if patient { self.o.answer_wait_ms } else { 20_000 });
        let begun = Instant::now();
        let mut said_waiting = false;
        let mut said_ignored = false;
        loop {
            let left = limit.saturating_sub(begun.elapsed());
            if left.is_zero() {
                return Ok(None);
            }
            // A short first poll, so that a host that takes its time can be shown as such after a moment.
            let seconds = if patient && !said_waiting { 2 } else { left.as_secs().clamp(1, 20) as u32 };
            let answer = signal
                .wait_answer(sid, seconds, ct)?
                .and_then(|envelope| envelope.try_open(keys, messages::KIND_ANSWER, &g.group_id, sid))
                .and_then(|payload| JoinAnswer::from_payload(&payload, sid));
            match (answer, &record.device_key) {
                (Some(answer), Some(host_key)) if !device::verify_answer(&g.group_id, host_key, my_exchange, &answer) => {
                    if !said_ignored {
                        said_ignored = true;
                        (self.log)(&format!("[p2p] ignored an answer for {} that its device did not sign", record.name));
                    }
                    // the server keeps one answer per session and hands it out at once: wait for the real one
                    // to replace it instead of asking in a tight loop
                    if ct.wait_ms(500) {
                        return Err(SignalingError::Cancelled);
                    }
                }
                (Some(answer), _) => return Ok(Some(answer)),
                (None, _) => {}
            }
            if patient && !said_waiting {
                said_waiting = true;
                self.set_state(ViewerSessionState::Approval, format!("Waiting for {} to answer", record.name));
            }
        }
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
