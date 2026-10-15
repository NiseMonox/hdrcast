//! Streaming to the group (sender side): advertises this host every 10 s, answers join requests, starts one
//! remux child per viewer fed from the shared encoder output, and lets [`HostLink`] punch through to each
//! viewer. Viewers come and go without touching the encoder.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use hdrcast_core::group::now_ms;
use hdrcast_core::messages::{self, JoinAnswer, JoinRequest};
use hdrcast_core::{endpoint, GroupConfig, GroupKeys, LiveRecord, ProcessJob, SignedEnvelope};

use crate::cancel::CancelToken;
use crate::echo_protocol;
use crate::host_link::{HostLink, HostSessionInfo, HostSideHandler};
use crate::link_base::{LinkAddresses, SOURCE_ECHO, SOURCE_ROUTER};
use crate::p2p_diagnostics;
use crate::peer_session::LinkState;
use crate::remux_child::RemuxChild;
use crate::signaling_client::{SignalingClient, SignalingError};
use crate::ts_fan_out::TsFanOut;
use crate::upnp::{PortMappingOptions, PortMappingState, RouterPortMapping};
use crate::util::{lock, round_half_even, Callbacks, LogFn, Worker};
use crate::StartError;

#[derive(Clone)]
pub struct P2pHostOptions {
    pub group: GroupConfig,
    pub ffmpeg_path: OsString,
    pub bitrate_bps: u64,
    pub codec: String,
    pub max_viewers: u32,
    pub job: Option<Arc<ProcessJob>>,
    /// `None` = detect this PC's stable IPv6 and LAN IPv4.
    pub addresses: Option<LinkAddresses>,
    /// Ask the signaling server's UDP echo for the public IPv4 mapping (the extra IPv4 attempt).
    pub discover_public_ipv4: bool,
    pub echo_port: u16,
    pub heartbeat_ms: u64,
    /// Forward the IPv4 port on the router (UPnP) while live, so viewers whose routers change ports get in.
    pub map_router_port: bool,
    /// `None` = the real router (tests point it at a stand-in).
    pub router_mapping: Option<PortMappingOptions>,
    /// Whether the firewall lets unsolicited UDP reach this program: `Some(true)` (a rule, or no firewall),
    /// `Some(false)`, or `None` (unknown). Supplied by the app, which owns the Windows Firewall check.
    pub inbound_allowed: Option<Arc<dyn Fn() -> Option<bool> + Send + Sync>>,
    /// Whether this host offers remote control right now (docs/design.md §8.9). Asked at every heartbeat; the
    /// answer goes into the live record as "rc". `None` = never.
    pub offer_remote_control: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl Default for P2pHostOptions {
    fn default() -> Self {
        P2pHostOptions {
            group: GroupConfig::default(),
            ffmpeg_path: OsString::from("ffmpeg"),
            bitrate_bps: 40_000_000,
            codec: "hevc".to_string(),
            max_viewers: 4,
            job: None,
            addresses: None,
            discover_public_ipv4: true,
            echo_port: echo_protocol::DEFAULT_PORT,
            heartbeat_ms: 10_000,
            map_router_port: true,
            router_mapping: None,
            inbound_allowed: None,
            offer_remote_control: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewerState {
    Connecting = 0,
    Watching = 1,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewerInfo {
    pub sid: String,
    pub name: String,
    pub state: ViewerState,
    pub path: String,
    pub ipv6: bool,
    pub rtt_ms: f64,
    pub joined_utc: SystemTime,
    pub latency_ms: i32,
    pub overhead_percent: i32,
    /// The viewer's join asked for remote control ("rc"): it speaks the side channel (docs/design.md §8.9).
    pub wants_control: bool,
}

pub struct P2pHost {
    inner: Arc<Inner>,
}

struct Viewer {
    sid: String,
    name: String,
    candidates: Vec<String>,
    info: Mutex<ViewerInfo>,
    child: Mutex<ChildSlot>,
    ended: AtomicBool,
}

#[derive(Default)]
struct ChildSlot {
    child: Option<Arc<RemuxChild>>,
    /// Which child the slot holds (or held last): an exit is only acted on by the child it belongs to.
    id: u64,
    port: u16,
    restarts: RestartLimiter,
}

/// A viewer's remux child may exit (its SRT connection ended) and be replaced six times within a minute; the
/// seventh exit in that minute means something is wrong, and the viewer is ended instead.
#[derive(Default)]
struct RestartLimiter {
    exits: VecDeque<Instant>,
}

impl RestartLimiter {
    /// Notes an exit at `now`; true = start another child.
    fn allow(&mut self, now: Instant) -> bool {
        while self.exits.front().is_some_and(|at| now.saturating_duration_since(*at) > Duration::from_secs(60)) {
            self.exits.pop_front();
        }
        self.exits.push_back(now);
        self.exits.len() <= P2pHost::MAX_CHILD_RESTARTS_PER_MINUTE
    }
}

struct State {
    viewers: HashMap<String, Arc<Viewer>>,
    seen_sids: HashMap<String, Instant>,
    status: String,
}

struct Inner {
    o: P2pHostOptions,
    log: LogFn,
    ts: Mutex<Option<Box<dyn Read + Send>>>,
    state: Mutex<State>,
    cancel: CancelToken,
    started: AtomicBool,
    stopped: AtomicBool,
    announced: AtomicBool,
    since_ms: AtomicI64,
    next_child: AtomicU64,
    keys: OnceLock<GroupKeys>,
    signal: OnceLock<SignalingClient>,
    link: OnceLock<HostLink>,
    router: OnceLock<RouterPortMapping>,
    fan: OnceLock<TsFanOut>,
    threads: Mutex<Vec<Worker>>,
    changed: Callbacks<dyn Fn() + Send + Sync>,
    input_ended: Callbacks<dyn Fn() + Send + Sync>,
    side_handler: Mutex<Option<HostSideHandler>>,
}

impl P2pHost {
    pub const MAX_CHILD_RESTARTS_PER_MINUTE: usize = 6;

    /// `ts_input`: the encoder's MPEG-TS (normally a [`SwitchableStream`](crate::SwitchableStream), so a
    /// restarted encoder is spliced in). [`stop`](Self::stop) cannot close it: whoever owns the other end does
    /// (close the `SwitchableStream` after stopping, as the apps always did).
    pub fn new(options: P2pHostOptions, ts_input: Box<dyn Read + Send>, log: impl Fn(&str) + Send + Sync + 'static) -> P2pHost {
        P2pHost {
            inner: Arc::new(Inner {
                o: options,
                log: Arc::new(log),
                ts: Mutex::new(Some(ts_input)),
                state: Mutex::new(State { viewers: HashMap::new(), seen_sids: HashMap::new(), status: "Starting".to_string() }),
                cancel: CancelToken::new(),
                started: AtomicBool::new(false),
                stopped: AtomicBool::new(false),
                announced: AtomicBool::new(false),
                since_ms: AtomicI64::new(0),
                next_child: AtomicU64::new(0),
                keys: OnceLock::new(),
                signal: OnceLock::new(),
                link: OnceLock::new(),
                router: OnceLock::new(),
                fan: OnceLock::new(),
                threads: Mutex::new(Vec::new()),
                changed: Callbacks::new(),
                input_ended: Callbacks::new(),
                side_handler: Mutex::new(None),
            }),
        }
    }

    /// The viewer list or the status line changed (called on a background thread).
    pub fn on_changed(&self, handler: impl Fn() + Send + Sync + 'static) {
        self.inner.changed.add(Arc::new(handler));
    }

    /// The encoder's stream ended.
    pub fn on_input_ended(&self, handler: impl Fn() + Send + Sync + 'static) {
        self.inner.input_ended.add(Arc::new(handler));
    }

    pub fn status(&self) -> String {
        lock(&self.inner.state).status.clone()
    }

    pub fn candidates(&self) -> Vec<String> {
        self.inner.link.get().map(HostLink::candidates).unwrap_or_default()
    }

    pub fn has_ipv6(&self) -> bool {
        self.inner.has_ipv6()
    }

    /// The router forward of the IPv4 port (`None` when not asked for, or before [`start`](Self::start)).
    pub fn router_mapping(&self) -> Option<&RouterPortMapping> {
        self.inner.router.get()
    }

    /// Viewers can reach the IPv4 port from any address and port: the router forwards it (or there is no router)
    /// and the firewall lets it in. `None` while that is still being found out. Advertised as "open4".
    pub fn ipv4_open(&self) -> Option<bool> {
        self.inner.ipv4_open()
    }

    /// Who is watching or connecting, oldest first.
    pub fn viewers(&self) -> Vec<ViewerInfo> {
        self.inner.viewers()
    }

    /// Upload this stream needs right now: bitrate × viewers × (1 + each viewer's SRT overhead).
    pub fn upload_mbps(&self) -> f64 {
        let bitrate = self.inner.o.bitrate_bps as f64 / 1e6;
        self.inner.viewers().iter().map(|v| bitrate * (1.0 + f64::from(v.overhead_percent) / 100.0)).sum()
    }

    pub fn upload_mbps_for(bitrate_bps: u64, viewers: u32, overhead_percent: i32) -> f64 {
        bitrate_bps as f64 / 1e6 * f64::from(viewers) * (1.0 + f64::from(overhead_percent) / 100.0)
    }

    /// Goes live: opens the sockets, asks the router, starts announcing and answering joins.
    pub fn start(&self) -> Result<(), StartError> {
        self.inner.start()
    }

    /// Goes offline: tells the server and every viewer, stops all remux children. Also done when dropped.
    pub fn stop(&self) {
        self.inner.stop();
    }

    /// Who gets what viewers send on the side channel: (session id, payload), on a network thread
    /// (docs/design.md §8.9). May be set before or after [`start`](Self::start); replaces the previous handler.
    pub fn set_side_handler(&self, handler: HostSideHandler) {
        *lock(&self.inner.side_handler) = Some(handler);
    }

    /// Sends `payload` (1..=1200 bytes) to one viewer on the side channel. False when that viewer's join did not
    /// ask for remote control (it may be a 0.2.4 viewer, which would hand the datagram to mpv), when its
    /// session is not connected, or when the payload is out of range.
    pub fn send_side(&self, sid: &str, payload: &[u8]) -> bool {
        let Some(viewer) = lock(&self.inner.state).viewers.get(sid).cloned() else { return false };
        if viewer.ended.load(Ordering::Acquire) || !lock(&viewer.info).wants_control {
            return false;
        }
        self.inner.link.get().is_some_and(|link| link.send_side(sid, payload))
    }
}

impl Drop for P2pHost {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Inner {
    fn start(self: &Arc<Self>) -> Result<(), StartError> {
        if self.started.swap(true, Ordering::AcqRel) {
            return Err(StartError::AlreadyStarted);
        }
        let g = &self.o.group;
        let keys = g.keys()?;
        let signal = SignalingClient::for_group(g)?;
        let link = HostLink::new(&keys, &self.o.addresses.clone().unwrap_or_else(LinkAddresses::detect))?;
        let weak = Arc::downgrade(self);
        link.on_session_changed({
            let weak = weak.clone();
            move |info| {
                if let Some(host) = weak.upgrade() {
                    host.on_session_changed(info);
                }
            }
        });
        link.set_side_handler({
            let weak = weak.clone();
            Arc::new(move |sid, payload| {
                let handler = weak.upgrade().and_then(|host| lock(&host.side_handler).clone());
                if let Some(handler) = handler {
                    handler(sid, payload);
                }
            })
        });
        self.since_ms.store(now_ms(), Ordering::Relaxed);
        (self.log)(&format!("[p2p] candidates: {}{}", link.candidates().join(", "), if link.has_ipv6() { "" } else { "  (no IPv6 on this PC)" }));
        let ipv4_port = link.ipv4_port();
        let _ = self.keys.set(keys);
        let _ = self.signal.set(signal);
        let _ = self.link.set(link);

        if let (true, Some(port)) = (self.o.map_router_port, ipv4_port) {
            let router = RouterPortMapping::new(port, self.o.router_mapping.clone(), self.log.clone());
            router.on_changed({
                let weak = weak.clone();
                move || {
                    if let Some(host) = weak.upgrade() {
                        host.on_router_changed();
                    }
                }
            });
            let _ = self.router.set(router);
            if let Some(router) = self.router.get() {
                router.start();
            }
        }

        let input = lock(&self.ts).take().ok_or(StartError::AlreadyStarted)?;
        let fan = TsFanOut::new(input);
        fan.on_input_ended({
            let weak = weak.clone();
            move || {
                if let Some(host) = weak.upgrade() {
                    if !host.stopped.load(Ordering::Acquire) {
                        (host.log)("[p2p] the encoder output ended");
                        host.input_ended.raise();
                    }
                }
            }
        });
        // A sink fails when its child's stdin closed: the child's exit handler does the restart.
        fan.start()?;
        let _ = self.fan.set(fan);

        let heartbeat = {
            let host = self.clone();
            Worker::spawn("p2p-heartbeat", move || host.heartbeat_loop())?
        };
        lock(&self.threads).push(heartbeat);
        let joins = {
            let host = self.clone();
            Worker::spawn("p2p-joins", move || host.join_loop())?
        };
        lock(&self.threads).push(joins);
        Ok(())
    }

    fn has_ipv6(&self) -> bool {
        self.link.get().is_some_and(HostLink::has_ipv6)
    }

    fn link_candidates(&self) -> Vec<String> {
        self.link.get().map(HostLink::candidates).unwrap_or_default()
    }

    fn inbound_allowed(&self) -> Option<bool> {
        self.o.inbound_allowed.as_ref().and_then(|ask| ask())
    }

    fn ipv4_open(&self) -> Option<bool> {
        let router = self.router.get()?;
        if matches!(router.state(), PortMappingState::Off | PortMappingState::Working) {
            return None;
        }
        let firewall = self.inbound_allowed();
        if !router.open() || firewall == Some(false) {
            return Some(false);
        }
        firewall
    }

    fn viewers(&self) -> Vec<ViewerInfo> {
        let viewers: Vec<Arc<Viewer>> = lock(&self.state).viewers.values().cloned().collect();
        let mut list: Vec<ViewerInfo> = viewers.iter().filter(|v| !v.ended.load(Ordering::Acquire)).map(|v| lock(&v.info).clone()).collect();
        list.sort_by_key(|v| v.joined_utc);
        list
    }

    fn on_router_changed(&self) {
        let (Some(router), Some(link)) = (self.router.get(), self.link.get()) else { return };
        let candidate = router.external().filter(|_| router.open()).map(|external| endpoint::format(&SocketAddr::V4(external)));
        link.set_candidate(SOURCE_ROUTER, candidate);
        if self.announced.load(Ordering::Acquire) && !self.stopped.load(Ordering::Acquire) {
            self.set_status(self.describe_live());
        }
    }

    fn heartbeat_loop(&self) {
        let ct = &self.cancel;
        let (Some(signal), g) = (self.signal.get(), &self.o.group) else { return };
        let mut registered = false;
        let mut need_echo = self.o.discover_public_ipv4;
        let mut echo_tried = false;
        let mut next_echo: Option<Instant> = None;
        let mut failures: u32 = 0;
        while !ct.is_cancelled() {
            let beat = (|| {
                if need_echo && next_echo.is_none_or(|at| Instant::now() >= at) {
                    // until the echo answers, ask again every minute (the port may be opened while live)
                    need_echo = !self.discover_public_ipv4(!echo_tried);
                    echo_tried = true;
                    next_echo = Some(Instant::now() + Duration::from_secs(60));
                }
                if !registered {
                    signal.register_group(ct)?;
                    registered = true;
                }
                signal.put_live(&g.peer_id, &self.seal_live(), ct)
            })();
            match beat {
                Ok(()) => {
                    if failures > 0 {
                        (self.log)("[p2p] signaling server reachable again");
                    }
                    failures = 0;
                    self.announced.store(true, Ordering::Release);
                    self.set_status(self.describe_live());
                    ct.wait_ms(self.o.heartbeat_ms);
                }
                Err(SignalingError::Cancelled) => return,
                Err(e) => {
                    if ct.is_cancelled() {
                        return;
                    }
                    if e.status() == 404 {
                        registered = false;
                    }
                    if e.status() == 409 {
                        self.set_status("This group's id is registered with a different key on the signaling server. Create a new group.".to_string());
                        (self.log)(&format!("[p2p] {e}"));
                        return;
                    }
                    failures += 1;
                    let delay = backoff_ms(failures);
                    if failures == 1 || failures % 10 == 0 {
                        (self.log)(&format!("[p2p] {e} - retrying"));
                    }
                    self.set_status(format!("Signaling server unreachable - retrying in {} s (viewers already watching are not affected)", delay / 1000));
                    ct.wait_ms(delay);
                }
            }
        }
    }

    /// This PC's public IPv4 mapping, the only IPv4 candidate a viewer outside this network can use. False = no
    /// answer yet (worth retrying).
    fn discover_public_ipv4(&self, log_failure: bool) -> bool {
        let (Some(signal), Some(link)) = (self.signal.get(), self.link.get()) else { return false };
        let server = match resolve_ipv4(&signal.host()) {
            Ok(Some(server)) => server,
            Ok(None) => return true, // IPv6-only signaling server: nothing to ask
            Err(e) => {
                if log_failure {
                    (self.log)(&format!("[p2p] public IPv4 check skipped: {e}"));
                }
                return false;
            }
        };
        let echo = SocketAddr::new(IpAddr::V4(server), self.o.echo_port);
        let Some(mapped) = link.discover_public_ipv4(echo, 1500) else {
            if log_failure {
                (self.log)(&format!(
                    "[p2p] public IPv4 check: no answer from the signaling server's UDP echo at {}, so this PC advertises no public IPv4 and \
                     viewers without IPv6 cannot reach it from outside this network. Is UDP {} open in the server's firewall / cloud security group? \
                     Retrying every minute.",
                    endpoint::format(&echo),
                    self.o.echo_port
                ));
            }
            return false;
        };
        let candidate = endpoint::format(&mapped);
        link.set_candidate(SOURCE_ECHO, Some(candidate.clone()));
        (self.log)(&format!("[p2p] public IPv4 mapping: {candidate}"));
        true
    }

    fn join_loop(self: &Arc<Self>) {
        let ct = &self.cancel;
        let Some(signal) = self.signal.get() else { return };
        let mut after: i64 = 0;
        let mut failures: u32 = 0;
        while !ct.is_cancelled() {
            match signal.poll_joins(&self.o.group.peer_id, after, 20, ct) {
                Ok((joins, next)) => {
                    after = next;
                    failures = 0;
                    for (_, envelope) in joins {
                        self.handle_join(&envelope);
                    }
                }
                Err(SignalingError::Cancelled) => return,
                Err(e) => {
                    if ct.is_cancelled() {
                        return;
                    }
                    // A restarted server numbers its mailboxes from 1 again; duplicates are dropped by session id.
                    if e.is_unreachable() {
                        after = 0;
                    }
                    failures += 1;
                    ct.wait_ms(if e.status() == 404 { 1000 } else { backoff_ms(failures) });
                }
            }
        }
    }

    fn handle_join(self: &Arc<Self>, envelope: &SignedEnvelope) {
        let g = &self.o.group;
        let (Some(keys), Some(link)) = (self.keys.get(), self.link.get()) else { return };
        let Some(payload) = envelope.try_open(keys, messages::KIND_JOIN, &g.group_id, &g.peer_id) else { return };
        let Some(join) = JoinRequest::from_payload(&payload) else { return };
        if (now_ms() - join.time_ms).abs() > 10 * 60 * 1000 {
            return;
        }

        let refusal = {
            let mut state = lock(&self.state);
            state.seen_sids.retain(|_, seen| seen.elapsed() <= Duration::from_secs(600));
            if state.seen_sids.contains_key(&join.session_id) {
                return;
            }
            state.seen_sids.insert(join.session_id.clone(), Instant::now());
            if self.stopped.load(Ordering::Acquire) {
                JoinAnswer::REASON_STOPPED
            } else if state.viewers.values().filter(|v| !v.ended.load(Ordering::Acquire)).count() >= self.o.max_viewers as usize {
                JoinAnswer::REASON_FULL
            } else {
                ""
            }
        };
        if !refusal.is_empty() {
            (self.log)(&format!("[p2p] {} wanted to join: {refusal}", join.name));
            self.answer(&join.session_id, false, refusal);
            return;
        }

        let viewer = Arc::new(Viewer {
            sid: join.session_id.clone(),
            name: join.name.clone(),
            candidates: join.candidates.clone(),
            info: Mutex::new(ViewerInfo {
                sid: join.session_id.clone(),
                name: join.name.clone(),
                state: ViewerState::Connecting,
                path: String::new(),
                ipv6: false,
                rtt_ms: -1.0,
                joined_utc: SystemTime::now(),
                latency_ms: join.latency_ms,
                overhead_percent: join.overhead_percent,
                wants_control: join.remote_control,
            }),
            child: Mutex::new(ChildSlot::default()),
            ended: AtomicBool::new(false),
        });
        let port = match self.start_child(&viewer) {
            Ok(port) => port,
            Err(e) => {
                (self.log)(&format!("[p2p] cannot start ffmpeg for {}: {e}", join.name));
                self.answer(&join.session_id, false, JoinAnswer::REASON_BUSY);
                return;
            }
        };
        lock(&self.state).viewers.insert(join.session_id.clone(), viewer.clone());
        if let Err(e) = link.add_session(&join.session_id, &join.candidates, port, Some(join.session_id.clone())) {
            // no local socket for this viewer: the C# apps would have crashed here; refuse the one viewer instead
            (self.log)(&format!("[p2p] cannot open a local socket for {}: {e}", join.name));
            self.end_viewer(&viewer, "the host could not serve this viewer");
            self.answer(&join.session_id, false, JoinAnswer::REASON_BUSY);
            self.changed.raise();
            return;
        }
        (self.log)(&format!("[p2p] {} is joining (latency {} ms, candidates {})", join.name, join.latency_ms, join.candidates.join(", ")));
        self.answer(&join.session_id, true, "");
        self.changed.raise();
    }

    /// Starts a remux child for `viewer` and feeds it; returns the UDP port its SRT listener is on.
    fn start_child(self: &Arc<Self>, viewer: &Arc<Viewer>) -> Result<u16, String> {
        let (Some(keys), Some(fan)) = (self.keys.get(), self.fan.get()) else { return Err("the host is not running".to_string()) };
        let (latency_ms, overhead_percent) = {
            let info = lock(&viewer.info);
            (info.latency_ms, info.overhead_percent)
        };
        let port = RemuxChild::free_loopback_udp_port().map_err(|e| e.to_string())?;
        let url = RemuxChild::build_listener_url(port, keys.srt_passphrase(), latency_ms.max(0) as u32, overhead_percent, self.o.bitrate_bps)
            .map_err(|e| e.to_string())?;
        let mut child = RemuxChild::new(&self.o.ffmpeg_path, &url, self.o.job.clone());
        let (log, tag) = (self.log.clone(), format!("[remux {}] ", viewer.name));
        child.on_log(move |line| log(&format!("{tag}{line}")));
        let id = self.next_child.fetch_add(1, Ordering::Relaxed) + 1;
        let (weak, exited) = (Arc::downgrade(self), viewer.clone());
        child.on_exited(move |code| {
            if let Some(host) = weak.upgrade() {
                host.on_child_exited(&exited, id, code);
            }
        });

        let input = {
            // Held across the start: an exit handler that runs right away waits here and then finds its child.
            let mut slot = lock(&viewer.child);
            child.start().map_err(|e| e.to_string())?;
            let input = child.input();
            slot.child = Some(Arc::new(child));
            slot.id = id;
            slot.port = port;
            input
        };
        let fed = match input {
            Some(input) => fan.add_sink(&viewer.sid, Box::new(input), TsFanOut::queue_bytes_for(self.o.bitrate_bps)).map_err(|e| e.to_string()),
            None => Err("ffmpeg has no stdin".to_string()),
        };
        if let Err(why) = fed {
            // a child nobody feeds would sit there for good: it and its viewer hold on to each other
            let unfed = {
                let mut slot = lock(&viewer.child);
                if slot.id == id {
                    slot.child.take()
                } else {
                    None
                }
            };
            if let Some(child) = unfed {
                child.dispose();
            }
            return Err(why);
        }
        Ok(port)
    }

    /// A child ends when its SRT connection ends (the viewer's mpv restarted): give the viewer a fresh one.
    fn on_child_exited(self: &Arc<Self>, viewer: &Arc<Viewer>, id: u64, code: i32) {
        let stopped = || self.stopped.load(Ordering::Acquire);
        let ended = || viewer.ended.load(Ordering::Acquire);
        let (restart, child) = {
            let mut slot = lock(&viewer.child);
            let mine = slot.id == id && slot.child.is_some();
            let mut restart = !stopped() && !ended() && mine;
            if restart {
                restart = slot.restarts.allow(Instant::now());
            }
            (restart, if mine { slot.child.clone() } else { None })
        };
        // Not in the slot any more: its viewer ended (or it was never fed) and whoever took it out disposes it.
        let Some(child) = child else { return };
        child.dispose();
        let link = self.link.get();
        if !restart {
            if !ended() && !stopped() {
                (self.log)(&format!("[p2p] ffmpeg for {} keeps exiting (code {code}); ending that viewer", viewer.name));
                if let Some(link) = link {
                    link.remove_session(&viewer.sid, "the host could not serve this viewer");
                }
            }
            return;
        }
        match self.start_child(viewer) {
            Ok(port) => {
                if let Some(link) = link {
                    link.retarget_session(&viewer.sid, port);
                }
            }
            Err(e) => {
                (self.log)(&format!("[p2p] cannot restart ffmpeg for {}: {e}", viewer.name));
                if let Some(link) = link {
                    link.remove_session(&viewer.sid, "the host could not serve this viewer");
                }
            }
        }
    }

    fn on_session_changed(&self, info: &HostSessionInfo) {
        let Some(viewer) = lock(&self.state).viewers.get(&info.sid).cloned() else { return };
        match info.state {
            LinkState::Connected => {
                {
                    let mut v = lock(&viewer.info);
                    v.state = ViewerState::Watching;
                    v.path = info.path.clone();
                    v.ipv6 = info.path_is_ipv6;
                    v.rtt_ms = info.rtt_ms;
                }
                (self.log)(&format!(
                    "[p2p] {} connected via {}{}{}",
                    viewer.name,
                    if info.path_is_ipv6 { "IPv6 " } else { "IPv4 " },
                    info.path,
                    if info.path_learned { " (a port their router picked for this PC, not the advertised one: it changes ports per destination)" } else { "" }
                ));
            }
            LinkState::Closed => self.end_viewer(&viewer, &info.reason),
            LinkState::Failed => {
                let reason = format!("could not connect: {}{}", info.reason, self.explain_failure(&viewer, &info.reason));
                self.end_viewer(&viewer, &reason);
            }
            LinkState::Punching => {}
        }
        self.changed.raise();
    }

    /// For the log when a viewer never got through: whose side most likely kept it out.
    fn explain_failure(&self, viewer: &Viewer, reason: &str) -> String {
        if !reason.starts_with("no candidate") {
            return String::new(); // one-way: a firewall on their side
        }
        if self.has_ipv6() && p2p_diagnostics::has_ipv6(&viewer.candidates) {
            return "; both have IPv6, so something drops UDP (a VPN, or a strict router firewall)".to_string();
        }
        if !p2p_diagnostics::has_public_ipv4(&viewer.candidates) {
            return "; they have no IPv6 and offered no public IPv4 address".to_string();
        }
        match self.ipv4_open() {
            Some(true) => "; nothing of theirs arrived although this PC takes IPv4 from any port: their side drops UDP (VPN, proxy, game accelerator?) or they need IPv6".to_string(),
            Some(false) => format!("; if their router changes ports per destination, this PC must take IPv4 from any port first: {}", self.closed_ipv4_parts()),
            None => String::new(),
        }
    }

    /// What keeps the IPv4 port from taking packets from any address and port, in words.
    fn closed_ipv4_parts(&self) -> String {
        let mut parts = Vec::new();
        if let Some(router) = self.router.get().filter(|router| !router.open()) {
            parts.push(if router.behind_another_nat() { "router behind another NAT" } else { "no router forward (UPnP)" });
        }
        if self.inbound_allowed() == Some(false) {
            parts.push("no firewall rule (click Allow)");
        }
        if parts.is_empty() {
            "unknown".to_string()
        } else {
            parts.join(", ")
        }
    }

    fn end_viewer(&self, viewer: &Arc<Viewer>, reason: &str) {
        if viewer.ended.swap(true, Ordering::AcqRel) {
            return;
        }
        let child = lock(&viewer.child).child.take();
        lock(&self.state).viewers.remove(&viewer.sid);
        if let Some(fan) = self.fan.get() {
            fan.remove_sink(&viewer.sid);
        }
        (self.log)(&format!("[p2p] {} left ({reason})", viewer.name));
        if let Some(child) = child {
            // the kill waits for the process to be gone: not on a network thread
            let _ = thread::Builder::new().name("p2p-child-end".to_string()).spawn(move || child.dispose());
        }
    }

    fn answer(&self, sid: &str, ok: bool, reason: &str) {
        let (Some(keys), Some(signal)) = (self.keys.get(), self.signal.get()) else { return };
        let answer = JoinAnswer {
            session_id: sid.to_string(),
            ok,
            reason: reason.to_string(),
            time_ms: now_ms(),
            candidates: if ok { self.link_candidates() } else { Vec::new() },
        };
        let envelope = SignedEnvelope::seal(keys, messages::KIND_ANSWER, &self.o.group.group_id, sid, &answer.to_payload());
        for attempt in 0..2 {
            match signal.put_answer(sid, &envelope, &self.cancel) {
                Ok(()) | Err(SignalingError::Cancelled) => return,
                Err(e) => {
                    if attempt == 1 {
                        (self.log)(&format!("[p2p] could not answer a join: {e}"));
                    }
                }
            }
        }
    }

    fn seal_live(&self) -> SignedEnvelope {
        let g = &self.o.group;
        let record = LiveRecord {
            host_id: g.peer_id.clone(),
            name: g.display_name.clone(),
            time_ms: now_ms(),
            since_ms: self.since_ms.load(Ordering::Relaxed),
            candidates: self.link_candidates(),
            codec: self.o.codec.clone(),
            mbps: round_half_even(self.o.bitrate_bps as f64 / 1e6) as i32,
            max_viewers: self.o.max_viewers as i32,
            viewers: self.viewers().len() as i32,
            open_ipv4: self.ipv4_open(),
            remote_control: self.o.offer_remote_control.as_ref().is_some_and(|offers| offers()).then_some(true),
        };
        let keys = self.keys.get().expect("sealed only while running");
        SignedEnvelope::seal(keys, messages::KIND_LIVE, &g.group_id, &g.peer_id, &record.to_payload())
    }

    fn describe_live(&self) -> String {
        let n = self.viewers().len();
        let public_v4 = p2p_diagnostics::has_public_ipv4(&self.link_candidates());
        let mut reach = match (self.has_ipv6(), public_v4) {
            (true, true) => "",
            (true, false) => " (no public IPv4 found: viewers without IPv6 can only join from this network)",
            (false, true) => " (no IPv6 on this PC: IPv4 only, which some viewers' routers block)",
            (false, false) => " (no IPv6 on this PC: only IPv4 viewers on the same network can join)",
        }
        .to_string();
        if public_v4 && self.ipv4_open() == Some(false) {
            reach.push_str(&format!(" (IPv4 closed to routers that change ports: {})", self.closed_ipv4_parts()));
        }
        format!("Live in \"{}\" - {n}{}{reach}", self.o.group.group_name, if n == 1 { " viewer" } else { " viewers" })
    }

    fn set_status(&self, status: String) {
        let changed = {
            let mut state = lock(&self.state);
            let changed = state.status != status;
            state.status = status;
            changed
        };
        if changed {
            self.changed.raise();
        }
    }

    fn stop(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        self.cancel.cancel();
        let threads: Vec<Worker> = lock(&self.threads).drain(..).collect();
        for thread in &threads {
            thread.join(Duration::from_millis(2000));
        }
        if let Some(signal) = self.signal.get() {
            let saved = signal.timeout_ms();
            signal.set_timeout_ms(2000);
            let _ = signal.delete_live(&self.o.group.peer_id, &CancelToken::never());
            signal.set_timeout_ms(saved);
        }
        if let Some(link) = self.link.get() {
            link.dispose();
        }
        if let Some(router) = self.router.get() {
            router.dispose(); // deletes the router forward (waits for the router, a few seconds at most)
        }
        let viewers: Vec<Arc<Viewer>> = lock(&self.state).viewers.drain().map(|(_, viewer)| viewer).collect();
        let mut children = Vec::new();
        for viewer in &viewers {
            viewer.ended.store(true, Ordering::Release);
            children.extend(lock(&viewer.child).child.take());
        }
        // children in parallel: each kill waits for the process to exit
        let (done, finished) = mpsc::channel();
        let mut waiting = 0;
        for child in children {
            let done = done.clone();
            let spawned = thread::Builder::new().name("p2p-child-end".to_string()).spawn(move || {
                child.dispose();
                let _ = done.send(());
            });
            if spawned.is_ok() {
                waiting += 1;
            }
        }
        let deadline = Instant::now() + Duration::from_millis(5000);
        while waiting > 0 {
            match finished.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(()) => waiting -= 1,
                Err(_) => break,
            }
        }
        if let Some(fan) = self.fan.get() {
            fan.dispose();
        }
        self.set_status("Offline".to_string());
    }
}

fn backoff_ms(failures: u32) -> u64 {
    match failures {
        0 | 1 => 3000,
        2 => 6000,
        3 => 12000,
        _ => 30000,
    }
}

/// The IPv4 address of `host` (a name or a literal); `Ok(None)` when it only has IPv6.
pub(crate) fn resolve_ipv4(host: &str) -> io::Result<Option<Ipv4Addr>> {
    if let Ok(literal) = host.parse::<IpAddr>() {
        return Ok(match literal {
            IpAddr::V4(v4) => Some(v4),
            IpAddr::V6(_) => None,
        });
    }
    if host.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "the signaling address names no server"));
    }
    Ok((host, 0).to_socket_addrs()?.find_map(|address| match address {
        SocketAddr::V4(v4) => Some(*v4.ip()),
        SocketAddr::V6(_) => None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_restarts_a_minute_and_the_seventh_exit_ends_the_viewer() {
        let start = Instant::now();
        let at = |seconds: u64| start + Duration::from_secs(seconds);
        let mut limiter = RestartLimiter::default();
        for exit in 0..6 {
            assert!(limiter.allow(at(exit)), "exit {}", exit + 1);
        }
        assert!(!limiter.allow(at(6)), "the seventh exit within a minute");
        assert_eq!(P2pHost::MAX_CHILD_RESTARTS_PER_MINUTE, 6);

        // exits older than a minute no longer count: a viewer whose mpv reconnects now and then is fine forever
        let mut limiter = RestartLimiter::default();
        for exit in 0..100 {
            assert!(limiter.allow(at(exit * 11)), "an exit every 11 s (number {})", exit + 1);
        }
        let mut limiter = RestartLimiter::default();
        for exit in 0..6 {
            assert!(limiter.allow(at(exit)));
        }
        assert!(limiter.allow(at(61)), "the first exit is more than a minute old");
        assert!(!limiter.allow(at(61)), "the other five and these two are not");
    }

    #[test]
    fn retries_back_off_to_half_a_minute() {
        assert_eq!([0, 1, 2, 3, 4, 50].map(backoff_ms), [3000, 3000, 6000, 12000, 30000, 30000]);
    }

    #[test]
    fn the_echo_is_only_asked_at_an_ipv4_address() {
        assert_eq!(resolve_ipv4("101.43.23.94").unwrap(), Some(Ipv4Addr::new(101, 43, 23, 94)));
        assert_eq!(resolve_ipv4("2001:db8::5").unwrap(), None);
        assert_eq!(resolve_ipv4("localhost").unwrap(), Some(Ipv4Addr::LOCALHOST));
        assert!(resolve_ipv4("").is_err());
    }
}
