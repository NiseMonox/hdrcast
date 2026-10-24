//! The viewer side: mpv connects to srt://127.0.0.1:[`local_port`](ViewerLink::local_port); its datagrams go to
//! the host over the punched path and the host's come back to mpv. The local port and the WAN sockets survive
//! re-joins ([`start_session`](ViewerLink::start_session) again), so mpv can keep retrying the same URL while
//! the path is rebuilt.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use hdrcast_core::endpoint;
use hdrcast_core::group::keys::InvalidIdError;
use hdrcast_core::GroupKeys;

use crate::control_packet::{self, PeerRole};
use crate::link_base::{LinkAddresses, LinkCore, LinkEvents};
use crate::peer_session::{LinkState, PeerSession};
use crate::side_channel;
use crate::srt_monitor::{SrtMonitor, SrtStats};
use crate::udp_channel::{mono_clock, UdpChannel};
use crate::util::{lock, round_half_even, Callbacks};

/// Receives what the host sends on the side channel. Runs on a network thread.
pub type ViewerSideHandler = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Told when data of the stream has been missing on the way in for longer than SRT needs to get it again
/// twice, with how long that was (docs/design.md §8.12). Runs on a network thread.
pub type LastingLossHandler = Arc<dyn Fn(Duration) + Send + Sync>;

type StateChanged = dyn Fn(LinkState) + Send + Sync;

pub struct ViewerLink {
    inner: Arc<Inner>,
}

struct Inner {
    core: LinkCore,
    local: UdpChannel,
    state: Mutex<State>,
    bytes_from_host: AtomicU64,
    state_changed: Callbacks<StateChanged>,
    side_handler: Mutex<Option<ViewerSideHandler>>,
    lasting_loss: Mutex<Option<LastingLossHandler>>,
}

struct State {
    session: Option<Arc<PeerSession>>,
    mpv: Option<SocketAddr>,
    reported: LinkState,
    /// The current session's SRT packets: the host's remux child is the sender, mpv (or ffmpeg) the receiver.
    srt: Arc<SrtMonitor>,
}

impl ViewerLink {
    /// `local_port`: mpv's port on 127.0.0.1 (0 = any free port).
    pub fn new(keys: &GroupKeys, addresses: &LinkAddresses, local_port: u16) -> io::Result<ViewerLink> {
        let core = LinkCore::new(keys, addresses, "p2p-viewer")?;
        let slot: Arc<OnceLock<Weak<Inner>>> = Arc::new(OnceLock::new());
        let ready = slot.clone();
        let local = UdpChannel::bind(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), local_port),
            "p2p-mpv",
            Box::new(move |_, datagram, from| {
                if let Some(link) = ready.get().and_then(Weak::upgrade) {
                    link.on_local_data(datagram, from);
                }
            }),
        )?;
        let inner = Arc::new(Inner {
            core,
            local,
            state: Mutex::new(State { session: None, mpv: None, reported: LinkState::Punching, srt: Arc::new(SrtMonitor::new()) }),
            bytes_from_host: AtomicU64::new(0),
            state_changed: Callbacks::new(),
            side_handler: Mutex::new(None),
            lasting_loss: Mutex::new(None),
        });
        let _ = slot.set(Arc::downgrade(&inner));
        let events: Weak<dyn LinkEvents> = Arc::downgrade(&(inner.clone() as Arc<dyn LinkEvents>));
        inner.core.start(events)?;
        Ok(ViewerLink { inner })
    }

    /// mpv's URL port on 127.0.0.1.
    pub fn local_port(&self) -> u16 {
        self.inner.local.local_addr().port()
    }

    /// Called (on a network thread) with the new state when the current session connects, closes or fails.
    pub fn on_state_changed(&self, handler: impl Fn(LinkState) + Send + Sync + 'static) {
        self.inner.state_changed.add(Arc::new(handler));
    }

    pub fn has_ipv6(&self) -> bool {
        self.inner.core.has_ipv6()
    }

    pub fn has_ipv4(&self) -> bool {
        self.inner.core.has_ipv4()
    }

    pub fn ipv4_port(&self) -> Option<u16> {
        self.inner.core.ipv4_port()
    }

    pub fn ipv6_addr(&self) -> Option<SocketAddr> {
        self.inner.core.ipv6_addr()
    }

    /// What this side advertises: `[v6]:port` and `lan4:port`, then the candidates set with
    /// [`set_candidate`](Self::set_candidate) (the echo's, the router's; duplicates once).
    pub fn candidates(&self) -> Vec<String> {
        self.inner.core.candidates()
    }

    /// Sets, replaces or (with `None`) removes the candidate one source vouches for:
    /// [`SOURCE_ECHO`](crate::SOURCE_ECHO) or [`SOURCE_ROUTER`](crate::SOURCE_ROUTER).
    pub fn set_candidate(&self, source: &str, candidate: Option<String>) {
        self.inner.core.set_candidate(source, candidate);
    }

    pub fn wan_bytes_in(&self) -> u64 {
        self.inner.core.wan_bytes_in()
    }

    pub fn wan_bytes_out(&self) -> u64 {
        self.inner.core.wan_bytes_out()
    }

    /// Asks the signaling server's UDP echo what address and port it saw for the IPv4 socket: the NAT mapping
    /// peers must punch towards (docs/design.md §8.7). `None` without an IPv4 socket, for an IPv6 server address,
    /// or when no answer comes within about `timeout_ms` (three tries).
    pub fn discover_public_ipv4(&self, echo_server: SocketAddr, timeout_ms: u64) -> Option<SocketAddr> {
        self.inner.core.discover_public_ipv4(echo_server, timeout_ms)
    }

    pub fn session_id(&self) -> Option<String> {
        self.inner.session().map(|s| s.sid().to_string())
    }

    /// The current session's state; `Closed` before the first one.
    pub fn state(&self) -> LinkState {
        self.inner.session().map_or(LinkState::Closed, |s| s.state())
    }

    pub fn reason(&self) -> String {
        self.inner.session().map(|s| s.reason()).unwrap_or_default()
    }

    pub fn path(&self) -> Option<SocketAddr> {
        self.inner.session().and_then(|s| s.selected())
    }

    /// The path is an address the host never advertised: its router picked a new port for this PC.
    pub fn path_learned(&self) -> bool {
        let Some(session) = self.inner.session() else { return false };
        session.selected().is_some_and(|path| !session.is_advertised(path))
    }

    pub fn rtt_ms(&self) -> f64 {
        self.inner.session().map_or(-1.0, |s| s.rtt_ms())
    }

    /// What the current session's SRT packets say about the path (docs/design.md §8.11).
    pub fn srt_stats(&self) -> SrtStats {
        lock(&self.inner.state).srt.snapshot()
    }

    pub fn bytes_from_host(&self) -> u64 {
        self.inner.bytes_from_host.load(Ordering::Relaxed)
    }

    /// Begins (or restarts with a new session id) punching towards the host's candidates.
    pub fn start_session<S: AsRef<str>>(&self, sid: &str, host_candidates: &[S]) -> Result<(), InvalidIdError> {
        let next = PeerSession::new(self.inner.core.keys(), sid, PeerRole::Viewer, mono_clock::now_us())?;
        next.add_remote_candidates(host_candidates);
        let previous = {
            let mut state = lock(&self.inner.state);
            state.reported = LinkState::Punching;
            if state.session.as_ref().is_none_or(|s| s.sid() != sid) {
                state.srt = Arc::new(SrtMonitor::new());
            }
            state.session.replace(Arc::new(next))
        };
        if let Some(previous) = previous.filter(|p| p.sid() != sid) {
            self.inner.core.send_all(&previous.close_with_bye("the viewer re-joined", mono_clock::now_us()));
        }
        Ok(())
    }

    /// More host candidates (the answer can carry a fresher list than the live record).
    pub fn add_host_candidates<S: AsRef<str>>(&self, host_candidates: &[S]) {
        if let Some(session) = self.inner.session() {
            session.add_remote_candidates(host_candidates);
        }
    }

    /// Ends the current session with a BYE (the sockets stay open for a later re-join).
    pub fn end_session(&self, reason: &str) {
        if let Some(session) = self.inner.session() {
            self.inner.core.send_all(&session.close_with_bye(reason, mono_clock::now_us()));
        }
        self.inner.report();
    }

    /// Who gets what the host sends on the side channel (docs/design.md §8.9). Replaces the previous handler.
    pub fn set_side_handler(&self, handler: ViewerSideHandler) {
        *lock(&self.inner.side_handler) = Some(handler);
    }

    /// Who is told when data of the stream stays missing on the way in (see [`LastingLossHandler`]).
    pub fn on_lasting_loss(&self, handler: LastingLossHandler) {
        *lock(&self.inner.lasting_loss) = Some(handler);
    }

    /// Sends `payload` (1..=1200 bytes) to the host on the side channel, on the path the stream uses. False
    /// when the session is not connected or the payload is out of range. Only for hosts that announced it:
    /// an older host would take the datagram for SRT.
    pub fn send_side(&self, payload: &[u8]) -> bool {
        if !side_channel::is_valid_payload(payload) {
            return false;
        }
        let Some(session) = self.inner.session() else { return false };
        if session.state() != LinkState::Connected {
            return false;
        }
        let (Some(to), Some(datagram)) = (session.selected(), side_channel::build(session.sid_bytes(), payload)) else {
            return false;
        };
        if !self.inner.core.send_wan(&datagram, to) {
            return false;
        }
        session.note_sent(mono_clock::now_us());
        true
    }

    /// "IPv6 [2400:…]:51234, ping 38 ms" for the status line (the round trip, not the picture's delay).
    pub fn describe_path(&self) -> String {
        let Some(path) = self.path() else { return String::new() };
        let family = if path.is_ipv6() { "IPv6" } else { "IPv4" };
        let rtt = self.rtt_ms();
        let ping = if rtt >= 0.0 { format!(", ping {} ms", round_half_even(rtt)) } else { String::new() };
        format!("{family} {}{ping}", endpoint::format(&path))
    }

    /// Ends the session with a BYE ("the viewer closed") and closes the sockets. Also done when dropped.
    pub fn dispose(&self) {
        let inner = &self.inner;
        inner.core.dispose(|| {
            if let Some(session) = inner.session() {
                inner.core.send_all(&session.close_with_bye("the viewer closed", mono_clock::now_us()));
            }
            inner.local.dispose();
        });
    }
}

impl Drop for ViewerLink {
    fn drop(&mut self) {
        self.dispose();
    }
}

impl Inner {
    fn session(&self) -> Option<Arc<PeerSession>> {
        lock(&self.state).session.clone()
    }

    fn on_local_data(&self, datagram: &[u8], from: SocketAddr) {
        if from.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
            return;
        }
        let (session, srt) = {
            let mut state = lock(&self.state);
            state.mpv = Some(from); // a restarted mpv comes from a new port: follow it
            (state.session.clone(), state.srt.clone())
        };
        let Some(session) = session else { return };
        let Some(to) = session.selected() else { return };
        if session.is_over() {
            return;
        }
        srt.from_receiver(datagram);
        if self.core.send_wan(datagram, to) {
            session.note_sent(mono_clock::now_us());
        }
    }

    fn report(&self) {
        let changed = {
            let mut state = lock(&self.state);
            let now = state.session.as_ref().map_or(LinkState::Closed, |s| s.state());
            let changed = now != state.reported;
            state.reported = now;
            changed.then_some(now)
        };
        if let Some(now) = changed {
            for handler in self.state_changed.snapshot() {
                handler(now);
            }
        }
    }
}

impl LinkEvents for Inner {
    fn on_control(&self, packet: &[u8], from: SocketAddr) {
        if control_packet::role_of(packet) != Some(PeerRole::Host) {
            return;
        }
        let Some(session) = self.session() else { return };
        if control_packet::sid_of(packet) != session.sid() || !control_packet::verify_mac(packet, session.punch_key()) {
            return;
        }
        self.core.send_all(&session.on_control(packet, from, mono_clock::now_us()));
        self.report();
    }

    fn on_side(&self, sid: &str, payload: &[u8], from: SocketAddr) {
        let Some(session) = self.session() else { return };
        // the same test and the same bookkeeping as for SRT data: a confirmed address of the live session
        if session.sid() != sid || !session.on_data(from, mono_clock::now_us()) {
            return;
        }
        let handler = lock(&self.side_handler).clone();
        if let Some(handler) = handler {
            handler(payload);
        }
    }

    fn on_wan_data(&self, datagram: &[u8], from: SocketAddr) {
        let (session, mpv, srt) = {
            let state = lock(&self.state);
            (state.session.clone(), state.mpv, state.srt.clone())
        };
        let Some(session) = session else { return };
        if !session.on_data(from, mono_clock::now_us()) {
            return;
        }
        let Some(mpv) = mpv else { return };
        srt.from_sender(datagram);
        if self.local.send(datagram, mpv) {
            self.bytes_from_host.fetch_add(datagram.len() as u64, Ordering::Relaxed);
        }
        if let Some(missing) = srt.take_lasting_loss() {
            let handler = lock(&self.lasting_loss).clone();
            if let Some(handler) = handler {
                handler(missing);
            }
        }
    }

    fn on_tick(&self, now_us: i64) {
        let Some(session) = self.session() else { return };
        self.core.send_all(&session.tick(now_us));
        self.report();
    }
}
