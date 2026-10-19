//! Streaming to the group (sender side): advertises this host every 10 s, answers join requests, starts one
//! remux child per viewer fed from the shared encoder output, and lets [`HostLink`] punch through to each
//! viewer. Viewers come and go without touching the encoder.
//!
//! With an admission hook ([`P2pHostOptions::admit`]) a join waits until the app lets it in or refuses it
//! (docs/design.md §8.10): the app checks its rules for that device, may ask its user, and starts the encoder
//! first when this host only streams on request.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use hdrcast_core::group::device::{self, DeviceKey, Exchange, VerifiedDevice};
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
    /// This computer's device key: advertised in the live record ("dk"), signs every answer, and lets a viewer
    /// that signed its join agree on a remote-control key with this host (docs/design.md §8.10).
    pub device: Option<Arc<DeviceKey>>,
    /// Who decides about each join. `None` = everyone is let in at once (as before 0.3.1). Otherwise every join
    /// that passes the checks is handed over as a [`JoinTicket`] and waits for [`P2pHost::admit`] or
    /// [`P2pHost::refuse`] (on a thread of the caller's choosing; the hook itself must return at once).
    pub admit: Option<AdmitFn>,
    /// Whether this host streams on request rather than broadcasting ("od" in the live record). Asked at every
    /// heartbeat. `None` = a broadcast.
    pub on_demand: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

/// Receives each join waiting to be let in (see [`P2pHostOptions::admit`]).
pub type AdmitFn = Arc<dyn Fn(JoinTicket) + Send + Sync>;

/// A join waiting for the app's decision.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinTicket {
    pub sid: String,
    /// The peer id the viewer gave. Anyone in the group could have written it: trust [`device_key`](Self::device_key).
    pub viewer_id: String,
    pub name: String,
    /// The viewer's device key when the join was signed and the signature holds (docs/design.md §8.10); `None`
    /// for a viewer older than 0.3.1.
    pub device_key: Option<String>,
    /// The join asked for remote control ("rc"), which also means the viewer speaks the side channel.
    pub wants_control: bool,
    pub latency_ms: i32,
    pub overhead_percent: i32,
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
            device: None,
            admit: None,
            on_demand: None,
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
    pub viewer_id: String,
    /// The viewer's device key, when its join was signed (docs/design.md §8.10).
    pub device_key: Option<String>,
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
    /// The remote-control key agreed with this viewer's device; `None` = the group-derived one.
    control_key: Option<[u8; 32]>,
}

/// A join handed to the admission hook and not decided yet.
struct PendingJoin {
    join: JoinRequest,
    device: Option<VerifiedDevice>,
    since: Instant,
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
    pending: HashMap<String, PendingJoin>,
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
    /// Set to send the next heartbeat now instead of at its time.
    announce_soon: AtomicBool,
    since_ms: AtomicI64,
    next_child: AtomicU64,
    /// The stream's bitrate and the viewer limit: start from the options, changeable while running.
    bitrate_bps: AtomicU64,
    max_viewers: AtomicU32,
    /// The ffmpeg each viewer's remux child runs: from the options, changeable while running.
    ffmpeg_path: Mutex<OsString>,
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
    /// A join the admission hook never decided about is refused after this long (viewers wait a minute).
    pub const PENDING_LIMIT: Duration = Duration::from_secs(90);

    /// `ts_input`: the encoder's MPEG-TS (normally a [`SwitchableStream`](crate::SwitchableStream), so a
    /// restarted encoder is spliced in). [`stop`](Self::stop) cannot close it: whoever owns the other end does
    /// (close the `SwitchableStream` after stopping, as the apps always did).
    pub fn new(options: P2pHostOptions, ts_input: Box<dyn Read + Send>, log: impl Fn(&str) + Send + Sync + 'static) -> P2pHost {
        let (bitrate_bps, max_viewers, ffmpeg_path) = (options.bitrate_bps, options.max_viewers, options.ffmpeg_path.clone());
        P2pHost {
            inner: Arc::new(Inner {
                o: options,
                log: Arc::new(log),
                ts: Mutex::new(Some(ts_input)),
                state: Mutex::new(State {
                    viewers: HashMap::new(),
                    pending: HashMap::new(),
                    seen_sids: HashMap::new(),
                    status: "Starting".to_string(),
                }),
                cancel: CancelToken::new(),
                started: AtomicBool::new(false),
                stopped: AtomicBool::new(false),
                announced: AtomicBool::new(false),
                announce_soon: AtomicBool::new(false),
                since_ms: AtomicI64::new(0),
                next_child: AtomicU64::new(0),
                bitrate_bps: AtomicU64::new(bitrate_bps),
                max_viewers: AtomicU32::new(max_viewers),
                ffmpeg_path: Mutex::new(ffmpeg_path),
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
        let bitrate = self.inner.bitrate_bps() as f64 / 1e6;
        self.inner.viewers().iter().map(|v| bitrate * (1.0 + f64::from(v.overhead_percent) / 100.0)).sum()
    }

    /// New settings for the stream: the bitrate (sizes the viewers' SRT buffers and goes into the live record)
    /// and how many may watch at once. Viewers already watching keep what they started with.
    pub fn set_stream(&self, bitrate_bps: u64, max_viewers: u32) {
        let old = (self.inner.bitrate_bps.swap(bitrate_bps, Ordering::AcqRel), self.inner.max_viewers.swap(max_viewers, Ordering::AcqRel));
        if old != (bitrate_bps, max_viewers) {
            self.announce_now();
        }
    }

    /// Sends the next heartbeat now (the live record changed: on request or broadcasting, remote control).
    pub fn announce_now(&self) {
        self.inner.announce_soon.store(true, Ordering::Release);
    }

    /// The ffmpeg that viewers' remux children run from now on.
    pub fn set_ffmpeg_path(&self, path: OsString) {
        *lock(&self.inner.ffmpeg_path) = path;
    }

    /// Bytes of the encoder's stream read so far (0 before [`start`](Self::start)): it grows while the encoder
    /// delivers.
    pub fn stream_bytes(&self) -> u64 {
        self.inner.fan.get().map_or(0, TsFanOut::bytes_in)
    }

    /// The joins handed to the admission hook that are not decided yet, oldest first.
    pub fn pending(&self) -> Vec<JoinTicket> {
        let state = lock(&self.inner.state);
        let mut list: Vec<(&PendingJoin, JoinTicket)> = state.pending.values().map(|p| (p, ticket_of(p))).collect();
        list.sort_by_key(|(p, _)| p.since);
        list.into_iter().map(|(_, t)| t).collect()
    }

    /// Lets a waiting join in: starts its remux child, punches towards it and answers yes. Fails (and answers
    /// no, with why) when this host stopped or is full meanwhile, or the viewer could not be served. Returns
    /// the remote-control key agreed with the viewer's device, `None` when the session uses the group's.
    pub fn admit(&self, sid: &str) -> Result<Option<[u8; 32]>, String> {
        self.inner.admit(sid)
    }

    /// Refuses a waiting join (`reason`: one of the `JoinAnswer::REASON_*`). False when it was no longer waiting.
    pub fn refuse(&self, sid: &str, reason: &str) -> bool {
        let Some(pending) = lock(&self.inner.state).pending.remove(sid) else { return false };
        let exchange = pending.device.as_ref().map_or("", |d| d.exchange.as_str()).to_string();
        self.inner.answer(sid, false, reason, &exchange, None);
        true
    }

    /// Ends one viewer's session (BYE): "disconnect" in the viewer list.
    pub fn kick(&self, sid: &str, reason: &str) {
        if let Some(link) = self.inner.link.get() {
            link.remove_session(sid, reason);
        }
    }

    /// The remote-control key agreed with the device of the viewer in session `sid`; `None` when that viewer
    /// did not sign its join (use the group-derived `GroupKeys::control_key`) or is gone.
    pub fn control_key(&self, sid: &str) -> Option<[u8; 32]> {
        lock(&self.inner.state).viewers.get(sid).and_then(|v| v.control_key)
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
                    self.wait_for_next_beat();
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

    /// Until the next heartbeat is due, or until [`P2pHost::announce_now`] asks for it (or the host stops).
    fn wait_for_next_beat(&self) {
        let due = Instant::now() + Duration::from_millis(self.o.heartbeat_ms);
        while !self.announce_soon.swap(false, Ordering::AcqRel) {
            let left = due.saturating_duration_since(Instant::now());
            if left.is_zero() || self.cancel.wait(left.min(Duration::from_millis(200))) {
                return;
            }
        }
    }

    fn bitrate_bps(&self) -> u64 {
        self.bitrate_bps.load(Ordering::Acquire)
    }

    fn max_viewers(&self) -> u32 {
        self.max_viewers.load(Ordering::Acquire)
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
            self.expire_pending();
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
        let Some(keys) = self.keys.get() else { return };
        let Some(payload) = envelope.try_open(keys, messages::KIND_JOIN, &g.group_id, &g.peer_id) else { return };
        let Some(join) = JoinRequest::from_payload(&payload) else { return };
        if (now_ms() - join.time_ms).abs() > 10 * 60 * 1000 {
            return;
        }
        let device = device::verify_join(&g.group_id, &g.peer_id, &join);
        let viewer_exchange = device.as_ref().map_or(String::new(), |d| d.exchange.clone());

        let refusal = {
            let mut state = lock(&self.state);
            state.seen_sids.retain(|_, seen| seen.elapsed() <= Duration::from_secs(600));
            if state.seen_sids.contains_key(&join.session_id) {
                return;
            }
            state.seen_sids.insert(join.session_id.clone(), Instant::now());
            if self.stopped.load(Ordering::Acquire) {
                JoinAnswer::REASON_STOPPED
            } else if join.device.is_some() && device.is_none() {
                // signed, but not by the key it names (or not for this host): somebody posing as another computer
                JoinAnswer::REASON_DENIED
            } else if self.watching(&state) >= self.max_viewers() as usize {
                JoinAnswer::REASON_FULL
            } else {
                ""
            }
        };
        if !refusal.is_empty() {
            if refusal == JoinAnswer::REASON_DENIED {
                (self.log)(&format!("[p2p] {} wanted to join with a device signature that does not hold: refused", join.name));
            } else {
                (self.log)(&format!("[p2p] {} wanted to join: {refusal}", join.name));
            }
            self.answer(&join.session_id, false, refusal, &viewer_exchange, None);
            return;
        }

        match self.o.admit.clone() {
            None => {
                let _ = self.accept(join, device);
            }
            Some(admit) => {
                let pending = PendingJoin { join, device, since: Instant::now() };
                let ticket = ticket_of(&pending);
                (self.log)(&format!(
                    "[p2p] {} asks to join{}",
                    ticket.name,
                    match &ticket.device_key {
                        Some(key) => format!(" from device {}", device::code_of(key)),
                        None => " (no device signature: an older version)".to_string(),
                    }
                ));
                lock(&self.state).pending.insert(ticket.sid.clone(), pending);
                admit(ticket);
            }
        }
    }

    /// Viewers being served (not counting joins that wait for a decision).
    fn watching(&self, state: &State) -> usize {
        state.viewers.values().filter(|v| !v.ended.load(Ordering::Acquire)).count()
    }

    fn admit(self: &Arc<Self>, sid: &str) -> Result<Option<[u8; 32]>, String> {
        let pending = lock(&self.state).pending.remove(sid).ok_or_else(|| "that request is no longer waiting".to_string())?;
        let refusal = {
            let state = lock(&self.state);
            if self.stopped.load(Ordering::Acquire) {
                JoinAnswer::REASON_STOPPED
            } else if self.watching(&state) >= self.max_viewers() as usize {
                JoinAnswer::REASON_FULL
            } else {
                ""
            }
        };
        if !refusal.is_empty() {
            let exchange = pending.device.as_ref().map_or("", |d| d.exchange.as_str()).to_string();
            self.answer(sid, false, refusal, &exchange, None);
            return Err(if refusal == JoinAnswer::REASON_FULL {
                "this computer already has as many viewers as it takes".to_string()
            } else {
                "this computer stopped sharing".to_string()
            });
        }
        self.accept(pending.join, pending.device)
    }

    /// Serves `join`: starts its remux child, punches towards it, answers yes. Returns the remote-control key
    /// agreed with the viewer's device (`None`: the group-derived one).
    fn accept(self: &Arc<Self>, join: JoinRequest, device: Option<VerifiedDevice>) -> Result<Option<[u8; 32]>, String> {
        let Some(link) = self.link.get() else { return Err("the host is not running".to_string()) };
        let viewer_exchange = device.as_ref().map_or(String::new(), |d| d.exchange.clone());
        // A viewer that signed its join gets a remote-control key nobody else in the group can work out.
        let mut exchange = (device.is_some() && self.o.device.is_some()).then(Exchange::new);
        let control_key = match (&exchange, &device) {
            (Some(exchange), Some(device)) => device::parse_exchange_key(&device.exchange)
                .and_then(|theirs| exchange.agree(&theirs))
                .map(|shared| device::session_control_key(&shared, &join.session_id, &device.exchange, exchange.public_key())),
            _ => None,
        };
        if control_key.is_none() {
            exchange = None; // their exchange key was unusable: answer without ours, both sides use the group's
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
                viewer_id: join.viewer_id.clone(),
                device_key: device.as_ref().map(|d| d.key.clone()),
            }),
            child: Mutex::new(ChildSlot::default()),
            ended: AtomicBool::new(false),
            control_key,
        });
        let port = match self.start_child(&viewer) {
            Ok(port) => port,
            Err(e) => {
                (self.log)(&format!("[p2p] cannot start ffmpeg for {}: {e}", join.name));
                self.answer(&join.session_id, false, JoinAnswer::REASON_BUSY, &viewer_exchange, None);
                return Err(format!("cannot start ffmpeg for this viewer: {e}"));
            }
        };
        lock(&self.state).viewers.insert(join.session_id.clone(), viewer.clone());
        if let Err(e) = link.add_session(&join.session_id, &join.candidates, port, Some(join.session_id.clone())) {
            // no local socket for this viewer: the C# apps would have crashed here; refuse the one viewer instead
            (self.log)(&format!("[p2p] cannot open a local socket for {}: {e}", join.name));
            self.end_viewer(&viewer, "the host could not serve this viewer");
            self.answer(&join.session_id, false, JoinAnswer::REASON_BUSY, &viewer_exchange, None);
            self.changed.raise();
            return Err(format!("cannot open a local socket for this viewer: {e}"));
        }
        (self.log)(&format!("[p2p] {} is joining (latency {} ms, candidates {})", join.name, join.latency_ms, join.candidates.join(", ")));
        self.answer(&join.session_id, true, "", &viewer_exchange, exchange.as_ref());
        self.changed.raise();
        Ok(control_key)
    }

    /// Joins nobody decided about for [`P2pHost::PENDING_LIMIT`] are refused: their viewers gave up long ago.
    fn expire_pending(&self) {
        let expired: Vec<(String, String)> = {
            let mut state = lock(&self.state);
            let old: Vec<String> =
                state.pending.iter().filter(|(_, p)| p.since.elapsed() > P2pHost::PENDING_LIMIT).map(|(sid, _)| sid.clone()).collect();
            old.into_iter()
                .filter_map(|sid| state.pending.remove(&sid).map(|p| (sid, p.device.map(|d| d.exchange).unwrap_or_default())))
                .collect()
        };
        for (sid, exchange) in expired {
            self.answer(&sid, false, JoinAnswer::REASON_TIMEOUT, &exchange, None);
        }
    }

    /// Starts a remux child for `viewer` and feeds it; returns the UDP port its SRT listener is on.
    fn start_child(self: &Arc<Self>, viewer: &Arc<Viewer>) -> Result<u16, String> {
        let (Some(keys), Some(fan)) = (self.keys.get(), self.fan.get()) else { return Err("the host is not running".to_string()) };
        let (latency_ms, overhead_percent) = {
            let info = lock(&viewer.info);
            (info.latency_ms, info.overhead_percent)
        };
        let port = RemuxChild::free_loopback_udp_port().map_err(|e| e.to_string())?;
        let url = RemuxChild::build_listener_url(port, keys.srt_passphrase(), latency_ms.max(0) as u32, overhead_percent, self.bitrate_bps())
            .map_err(|e| e.to_string())?;
        let ffmpeg = lock(&self.ffmpeg_path).clone();
        let mut child = RemuxChild::new(&ffmpeg, &url, self.o.job.clone());
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
            Some(input) => fan.add_sink(&viewer.sid, Box::new(input), TsFanOut::queue_bytes_for(self.bitrate_bps())).map_err(|e| e.to_string()),
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

    /// Puts the answer for session `sid`, signed with this computer's device key when it has one.
    /// `viewer_exchange` is the exchange key of the join ("" when it had none); `exchange` is ours, when agreed.
    fn answer(&self, sid: &str, ok: bool, reason: &str, viewer_exchange: &str, exchange: Option<&Exchange>) {
        let (Some(keys), Some(signal)) = (self.keys.get(), self.signal.get()) else { return };
        let mut answer = JoinAnswer {
            session_id: sid.to_string(),
            ok,
            reason: reason.to_string(),
            time_ms: now_ms(),
            candidates: if ok { self.link_candidates() } else { Vec::new() },
            device: None,
        };
        if let Some(device) = &self.o.device {
            device::sign_answer(device, exchange, &self.o.group.group_id, viewer_exchange, &mut answer);
        }
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
            mbps: round_half_even(self.bitrate_bps() as f64 / 1e6) as i32,
            max_viewers: self.max_viewers() as i32,
            viewers: self.viewers().len() as i32,
            open_ipv4: self.ipv4_open(),
            remote_control: self.o.offer_remote_control.as_ref().is_some_and(|offers| offers()).then_some(true),
            device_key: self.o.device.as_ref().map(|d| d.public_key().to_string()),
            on_demand: self.on_demand(),
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
        format!(
            "{} in \"{}\" - {n}{}{reach}",
            if self.on_demand() { "Available" } else { "Live" },
            self.o.group.group_name,
            if n == 1 { " viewer" } else { " viewers" }
        )
    }

    fn on_demand(&self) -> bool {
        self.o.on_demand.as_ref().is_some_and(|ask| ask())
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
        // whoever still waits for a decision gives up on its own: the server is told nothing more
        lock(&self.state).pending.clear();
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

fn ticket_of(pending: &PendingJoin) -> JoinTicket {
    let join = &pending.join;
    JoinTicket {
        sid: join.session_id.clone(),
        viewer_id: join.viewer_id.clone(),
        name: join.name.clone(),
        device_key: pending.device.as_ref().map(|d| d.key.clone()),
        wants_control: join.remote_control,
        latency_ms: join.latency_ms,
        overhead_percent: join.overhead_percent,
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
