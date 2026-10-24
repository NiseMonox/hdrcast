//! The host side: many viewers on the same two WAN sockets. A datagram from a confirmed viewer address goes
//! to that viewer's own 127.0.0.1 socket, which talks to the viewer's remux child (SRT listener); what the
//! child answers goes back to wherever that viewer's data last came from.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use hdrcast_core::endpoint;
use hdrcast_core::GroupKeys;

use crate::control_packet::{self, PeerRole};
use crate::link_base::{LinkAddresses, LinkCore, LinkEvents};
use crate::peer_session::{LinkState, PeerSession};
use crate::side_channel;
use crate::srt_monitor::{SrtMonitor, SrtStats};
use crate::udp_channel::{mono_clock, UdpChannel};
use crate::util::{lock, Callbacks};

/// A snapshot of one viewer session for the UI and the host logic.
#[derive(Debug, Clone, PartialEq)]
pub struct HostSessionInfo {
    pub sid: String,
    /// Whatever [`HostLink::add_session`] was given.
    pub tag: Option<String>,
    pub state: LinkState,
    pub reason: String,
    pub path: String,
    pub path_is_ipv6: bool,
    /// The path is an address the viewer never advertised: its router picked a new port for this PC.
    pub path_learned: bool,
    pub rtt_ms: f64,
    pub bytes_to_viewer: u64,
    /// What the SRT packets on this path say about it (docs/design.md §8.11).
    pub srt: SrtStats,
}

/// Receives what viewers send on the side channel: (session id, payload). Runs on a network thread.
pub type HostSideHandler = Arc<dyn Fn(&str, &[u8]) + Send + Sync>;

/// Told (with the session id) when a viewer's SRT stream starts: the first data of a new SRT connection went
/// to it. Runs on a network thread.
pub type StreamStarted = Arc<dyn Fn(&str) + Send + Sync>;

type SessionChanged = dyn Fn(&HostSessionInfo) + Send + Sync;

pub struct HostLink {
    inner: Arc<Inner>,
}

struct Inner {
    core: LinkCore,
    sessions: Mutex<Sessions>,
    session_changed: Callbacks<SessionChanged>,
    side_handler: Mutex<Option<HostSideHandler>>,
    stream_started: Mutex<Option<StreamStarted>>,
}

#[derive(Default)]
struct Sessions {
    by_sid: HashMap<String, Arc<Entry>>,
    by_address: HashMap<SocketAddr, Arc<Entry>>,
}

impl Sessions {
    fn is_current(&self, entry: &Arc<Entry>) -> bool {
        self.by_sid.get(entry.session.sid()).is_some_and(|current| Arc::ptr_eq(current, entry))
    }

    fn forget(&mut self, entry: &Arc<Entry>) {
        self.by_sid.remove(entry.session.sid());
        self.by_address.retain(|_, other| !Arc::ptr_eq(other, entry));
    }
}

struct Entry {
    session: PeerSession,
    local: UdpChannel,
    target: Mutex<SocketAddr>,
    tag: Option<String>,
    reported: Mutex<LinkState>,
    bytes_to_viewer: AtomicU64,
    /// The remux child is the SRT sender, the viewer's player the receiver.
    srt: SrtMonitor,
}

impl HostLink {
    pub fn new(keys: &GroupKeys, addresses: &LinkAddresses) -> io::Result<HostLink> {
        let inner = Arc::new(Inner {
            core: LinkCore::new(keys, addresses, "p2p-host")?,
            sessions: Mutex::new(Sessions::default()),
            session_changed: Callbacks::new(),
            side_handler: Mutex::new(None),
            stream_started: Mutex::new(None),
        });
        let events: Weak<dyn LinkEvents> = Arc::downgrade(&(inner.clone() as Arc<dyn LinkEvents>));
        inner.core.start(events)?;
        Ok(HostLink { inner })
    }

    /// Called (on a network thread) when a session connects, closes or fails. Keep handlers short.
    pub fn on_session_changed(&self, handler: impl Fn(&HostSessionInfo) + Send + Sync + 'static) {
        self.inner.session_changed.add(Arc::new(handler));
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

    /// Starts punching towards a viewer; its data will be forwarded to 127.0.0.1:`child_port`.
    /// A session with the same id is replaced.
    pub fn add_session<S: AsRef<str>>(&self, sid: &str, viewer_candidates: &[S], child_port: u16, tag: Option<String>) -> io::Result<()> {
        let session = PeerSession::new(self.inner.core.keys(), sid, PeerRole::Host, mono_clock::now_us())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad session id"))?;
        session.add_remote_candidates(viewer_candidates);

        // The receive thread starts before the entry exists: until then what the child sends is dropped.
        let slot: Arc<OnceLock<Weak<Entry>>> = Arc::new(OnceLock::new());
        let link = Arc::downgrade(&self.inner);
        let ready = slot.clone();
        let local = UdpChannel::bind(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            &format!("p2p-child-{}", &sid[..6]),
            Box::new(move |_, datagram, from| {
                if let (Some(entry), Some(link)) = (ready.get().and_then(Weak::upgrade), link.upgrade()) {
                    link.on_child_data(&entry, datagram, from);
                }
            }),
        )?;
        let entry = Arc::new(Entry {
            session,
            local,
            target: Mutex::new(loopback(child_port)),
            tag,
            reported: Mutex::new(LinkState::Punching),
            bytes_to_viewer: AtomicU64::new(0),
            srt: SrtMonitor::new(),
        });
        let _ = slot.set(Arc::downgrade(&entry));

        let old = {
            let mut sessions = lock(&self.inner.sessions);
            let old = sessions.by_sid.get(sid).cloned();
            if let Some(old) = &old {
                sessions.forget(old);
            }
            sessions.by_sid.insert(sid.to_string(), entry);
            old
        };
        if let Some(old) = old {
            old.local.dispose();
        }
        Ok(())
    }

    /// The viewer's remux child moved to another port (it was restarted).
    pub fn retarget_session(&self, sid: &str, child_port: u16) {
        if let Some(entry) = lock(&self.inner.sessions).by_sid.get(sid) {
            *lock(&entry.target) = loopback(child_port);
        }
    }

    /// Ends a session (BYE to the viewer) and frees its local socket.
    pub fn remove_session(&self, sid: &str, reason: &str) {
        let entry = {
            let mut sessions = lock(&self.inner.sessions);
            let Some(entry) = sessions.by_sid.get(sid).cloned() else { return };
            sessions.forget(&entry);
            entry
        };
        self.inner.core.send_all(&entry.session.close_with_bye(reason, mono_clock::now_us()));
        entry.local.dispose();
        self.inner.report(&entry, true);
    }

    pub fn snapshot(&self) -> Vec<HostSessionInfo> {
        lock(&self.inner.sessions).by_sid.values().map(|entry| info(entry)).collect()
    }

    /// What the SRT packets of a viewer's session say about its path; `None` for an unknown session.
    pub fn srt_stats(&self, sid: &str) -> Option<SrtStats> {
        lock(&self.inner.sessions).by_sid.get(sid).map(|entry| entry.srt.snapshot())
    }

    pub fn session_count(&self) -> usize {
        lock(&self.inner.sessions).by_sid.len()
    }

    /// Who gets what viewers send on the side channel (docs/design.md §8.9). Replaces the previous handler.
    pub fn set_side_handler(&self, handler: HostSideHandler) {
        *lock(&self.inner.side_handler) = Some(handler);
    }

    /// Who is told when a viewer's SRT stream starts (a viewer joined, or its player connected again).
    pub fn on_stream_started(&self, handler: StreamStarted) {
        *lock(&self.inner.stream_started) = Some(handler);
    }

    /// Sends `payload` (1..=1200 bytes) to a viewer on the side channel, on the path its stream uses. False
    /// when the session is not connected or the payload is out of range. Only for viewers that asked for it:
    /// an older viewer would take the datagram for SRT.
    pub fn send_side(&self, sid: &str, payload: &[u8]) -> bool {
        if !side_channel::is_valid_payload(payload) {
            return false;
        }
        let Some(entry) = lock(&self.inner.sessions).by_sid.get(sid).cloned() else { return false };
        if entry.session.state() != LinkState::Connected {
            return false;
        }
        let (Some(to), Some(datagram)) = (entry.session.selected(), side_channel::build(entry.session.sid_bytes(), payload)) else {
            return false;
        };
        if !self.inner.core.send_wan(&datagram, to) {
            return false;
        }
        entry.session.note_sent(mono_clock::now_us());
        true
    }

    /// Ends every session with a BYE ("the host stopped") and closes the sockets. Also done when dropped.
    pub fn dispose(&self) {
        let inner = &self.inner;
        inner.core.dispose(|| {
            let entries: Vec<Arc<Entry>> = {
                let mut sessions = lock(&inner.sessions);
                sessions.by_address.clear();
                sessions.by_sid.drain().map(|(_, entry)| entry).collect()
            };
            for entry in entries {
                inner.core.send_all(&entry.session.close_with_bye("the host stopped", mono_clock::now_us()));
                entry.local.dispose();
            }
        });
    }
}

impl Drop for HostLink {
    fn drop(&mut self) {
        self.dispose();
    }
}

impl Inner {
    fn on_child_data(&self, entry: &Entry, datagram: &[u8], from: SocketAddr) {
        if from.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
            return;
        }
        let Some(to) = entry.session.selected() else { return };
        if entry.session.is_over() {
            return;
        }
        let started = entry.srt.from_sender(datagram);
        if self.core.send_wan(datagram, to) {
            entry.session.note_sent(mono_clock::now_us());
            entry.bytes_to_viewer.fetch_add(datagram.len() as u64, Ordering::Relaxed);
        }
        if started {
            let handler = lock(&self.stream_started).clone();
            if let Some(handler) = handler {
                handler(entry.session.sid());
            }
        }
    }

    fn report(&self, entry: &Entry, force: bool) {
        let now = entry.session.state();
        let changed = {
            let mut reported = lock(&entry.reported);
            let changed = force || now != *reported;
            *reported = now;
            changed
        };
        if changed {
            let info = info(entry);
            for handler in self.session_changed.snapshot() {
                handler(&info);
            }
        }
    }
}

impl LinkEvents for Inner {
    fn on_control(&self, packet: &[u8], from: SocketAddr) {
        if control_packet::role_of(packet) != Some(PeerRole::Viewer) {
            return; // our own packets reflected back
        }
        let sid = control_packet::sid_of(packet);
        let Some(entry) = lock(&self.sessions).by_sid.get(&sid).cloned() else { return };
        if !control_packet::verify_mac(packet, entry.session.punch_key()) {
            return;
        }

        let replies = entry.session.on_control(packet, from, mono_clock::now_us());
        if !entry.session.is_over() {
            let mut sessions = lock(&self.sessions);
            if sessions.is_current(&entry) {
                sessions.by_address.insert(from, entry.clone());
            }
        }
        self.core.send_all(&replies);
        self.report(&entry, false);
    }

    fn on_side(&self, sid: &str, payload: &[u8], from: SocketAddr) {
        let Some(entry) = lock(&self.sessions).by_sid.get(sid).cloned() else { return };
        // the same test and the same bookkeeping as for SRT data: a confirmed address of a live session
        if !entry.session.on_data(from, mono_clock::now_us()) {
            return;
        }
        let handler = lock(&self.side_handler).clone();
        if let Some(handler) = handler {
            handler(sid, payload);
        }
    }

    fn on_wan_data(&self, datagram: &[u8], from: SocketAddr) {
        let Some(entry) = lock(&self.sessions).by_address.get(&from).cloned() else {
            return; // not a confirmed viewer address
        };
        if !entry.session.on_data(from, mono_clock::now_us()) {
            return;
        }
        entry.srt.from_receiver(datagram);
        let target = *lock(&entry.target);
        entry.local.send(datagram, target);
    }

    fn on_tick(&self, now_us: i64) {
        let entries: Vec<Arc<Entry>> = lock(&self.sessions).by_sid.values().cloned().collect();
        for entry in entries {
            self.core.send_all(&entry.session.tick(now_us));
            if entry.session.is_over() {
                {
                    let mut sessions = lock(&self.sessions);
                    if sessions.is_current(&entry) {
                        sessions.forget(&entry);
                    }
                }
                entry.local.dispose();
            }
            self.report(&entry, false);
        }
    }
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

fn info(entry: &Entry) -> HostSessionInfo {
    let path = entry.session.selected();
    HostSessionInfo {
        sid: entry.session.sid().to_string(),
        tag: entry.tag.clone(),
        state: entry.session.state(),
        reason: entry.session.reason(),
        path: path.map(|p| endpoint::format(&p)).unwrap_or_default(),
        path_is_ipv6: path.is_some_and(|p| p.is_ipv6()),
        path_learned: path.is_some_and(|p| !entry.session.is_advertised(p)),
        rtt_ms: entry.session.rtt_ms(),
        bytes_to_viewer: entry.bytes_to_viewer.load(Ordering::Relaxed),
        srt: entry.srt.snapshot(),
    }
}
