//! The hole-punching state machine for one session (docs/design.md §8.6), independent of sockets so it can be
//! driven by a fake clock in tests. Both sides PUNCH every candidate every 100 ms until they get a PUNCH_ACK;
//! each PUNCH is answered with a PUNCH_ACK to its source, so both firewalls see outbound traffic for the
//! 5-tuple and open it. An authentic PUNCH from an address the other side never advertised (its router gave the
//! packets a new port) makes that address a candidate too.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Mutex;

use hdrcast_core::endpoint;
use hdrcast_core::group::keys::{self, InvalidIdError, ID_BYTES};
use hdrcast_core::GroupKeys;

use crate::control_packet::{self, ControlType, PeerRole};
use crate::util::lock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkState {
    /// Sending PUNCH to every candidate, waiting for a PUNCH_ACK.
    Punching = 0,
    /// A path works both ways; data flows; keepalives while idle.
    Connected = 1,
    /// Ended normally (BYE, local close) or went silent after connecting.
    Closed = 2,
    /// No candidate answered within the punch deadline.
    Failed = 3,
}

/// One datagram the session wants sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outgoing {
    pub data: [u8; control_packet::LENGTH],
    pub to: SocketAddr,
}

pub const PUNCH_INTERVAL_US: i64 = 100_000;
pub const PUNCH_DEADLINE_US: i64 = 15_000_000;
pub const KEEPALIVE_INTERVAL_US: i64 = 2_000_000;
pub const SILENCE_TIMEOUT_US: i64 = 15_000_000;
/// After an IPv4 path answers, wait this long for IPv6 to answer too (IPv6 wins).
pub const IPV6_GRACE_US: i64 = 200_000;
/// Advertised plus learned addresses punched at most (learned ones need an authentic packet).
pub const MAX_REMOTE_CANDIDATES: usize = 16;

/// Thread-safe: the link's receive threads, its timer and the caller all use the same session.
pub struct PeerSession {
    sid: String,
    sid_bytes: [u8; ID_BYTES],
    role: PeerRole,
    start_us: i64,
    punch_key: [u8; 32],
    inner: Mutex<Inner>,
}

struct Inner {
    state: LinkState,
    reason: String,
    selected: Option<SocketAddr>,
    rtt_ms: f64,
    remote: Vec<SocketAddr>,
    advertised: HashSet<SocketAddr>,
    confirmed: HashSet<SocketAddr>,
    acked_v4: Option<(SocketAddr, i64)>,
    last_punch_us: Option<i64>,
    last_send_us: i64,
    last_recv_us: i64,
}

impl Inner {
    fn is_over(&self) -> bool {
        matches!(self.state, LinkState::Closed | LinkState::Failed)
    }

    fn close(&mut self, reason: &str) {
        self.state = LinkState::Closed;
        self.reason = reason.to_string();
    }
}

impl PeerSession {
    pub fn new(keys: &GroupKeys, sid: &str, role: PeerRole, now_us: i64) -> Result<PeerSession, InvalidIdError> {
        let sid_bytes = keys::id_bytes_of(sid)?;
        Ok(PeerSession {
            sid: sid.to_string(),
            sid_bytes,
            role,
            start_us: now_us,
            punch_key: keys.punch_key(sid),
            inner: Mutex::new(Inner {
                state: LinkState::Punching,
                reason: String::new(),
                selected: None,
                rtt_ms: -1.0,
                remote: Vec::new(),
                advertised: HashSet::new(),
                confirmed: HashSet::new(),
                acked_v4: None,
                last_punch_us: None,
                last_send_us: now_us,
                last_recv_us: now_us,
            }),
        })
    }

    pub fn sid(&self) -> &str {
        &self.sid
    }

    pub fn sid_bytes(&self) -> &[u8; ID_BYTES] {
        &self.sid_bytes
    }

    pub fn role(&self) -> PeerRole {
        self.role
    }

    pub fn start_us(&self) -> i64 {
        self.start_us
    }

    pub fn punch_key(&self) -> &[u8; 32] {
        &self.punch_key
    }

    pub fn state(&self) -> LinkState {
        lock(&self.inner).state
    }

    pub fn reason(&self) -> String {
        lock(&self.inner).reason.clone()
    }

    /// Where data goes: the viewer's choice, or (host) where the viewer's data last came from.
    pub fn selected(&self) -> Option<SocketAddr> {
        lock(&self.inner).selected
    }

    /// The round trip of the last answered PUNCH; -1 before the first.
    pub fn rtt_ms(&self) -> f64 {
        lock(&self.inner).rtt_ms
    }

    pub fn is_over(&self) -> bool {
        lock(&self.inner).is_over()
    }

    /// Adds candidates (host: the viewer's from the join; viewer: the host's from the live record / answer).
    pub fn add_remote_candidates<S: AsRef<str>>(&self, candidates: &[S]) {
        let mut inner = lock(&self.inner);
        for candidate in candidates {
            let Ok(hp) = endpoint::parse(candidate.as_ref()) else { continue };
            if !hp.is_literal() || hp.port().is_none() {
                continue;
            }
            let Some(address) = hp.to_socket_addr(0) else { continue };
            inner.advertised.insert(address);
            if !inner.remote.contains(&address) && inner.remote.len() < MAX_REMOTE_CANDIDATES {
                inner.remote.push(address);
            }
        }
    }

    pub fn remote_candidates(&self) -> Vec<SocketAddr> {
        lock(&self.inner).remote.clone()
    }

    /// False for an address learned from the other side's packets rather than its candidate list.
    pub fn is_advertised(&self, address: SocketAddr) -> bool {
        lock(&self.inner).advertised.contains(&address)
    }

    pub fn is_confirmed(&self, from: SocketAddr) -> bool {
        lock(&self.inner).confirmed.contains(&from)
    }

    pub fn confirmed_addresses(&self) -> Vec<SocketAddr> {
        lock(&self.inner).confirmed.iter().copied().collect()
    }

    /// Called for every authentic control packet of this session (the caller checked the MAC). Returns replies
    /// to send.
    pub fn on_control(&self, packet: &[u8], from: SocketAddr, now_us: i64) -> Vec<Outgoing> {
        let mut output = Vec::new();
        let Some(kind) = control_packet::type_of(packet) else { return output };
        let mut inner = lock(&self.inner);
        if inner.is_over() {
            return output;
        }
        if kind == ControlType::Bye {
            inner.close("the other side ended the session");
            return output;
        }

        inner.last_recv_us = now_us;
        inner.confirmed.insert(from);
        // A symmetric NAT gives the other side's packets a new port for every destination, so they arrive from
        // an address it never advertised, and our PUNCHes to the advertised one are dropped by that NAT. Punch the
        // address the packets really come from: its NAT made that mapping for us and lets our packets in.
        if matches!(kind, ControlType::Punch | ControlType::PunchAck) && !inner.remote.contains(&from) && inner.remote.len() < MAX_REMOTE_CANDIDATES {
            inner.remote.push(from);
        }

        match kind {
            ControlType::Punch => {
                output.push(Outgoing { data: self.build(ControlType::PunchAck, now_us, control_packet::ts_of(packet)), to: from });
                inner.last_send_us = now_us;
                if self.role == PeerRole::Host && inner.selected.is_none() {
                    inner.selected = Some(from);
                }
            }
            ControlType::PunchAck => {
                let echo = control_packet::echo_of(packet);
                if echo > 0 && i64::try_from(echo).is_ok_and(|echo| echo <= now_us) {
                    inner.rtt_ms = (now_us - echo as i64) as f64 / 1000.0;
                }
                if self.role == PeerRole::Viewer {
                    if from.is_ipv6() {
                        if !inner.selected.is_some_and(|s| s.is_ipv6()) {
                            inner.selected = Some(from);
                        }
                    } else if inner.acked_v4.is_none() {
                        inner.acked_v4 = Some((from, now_us));
                    }
                } else if inner.selected.is_none() {
                    inner.selected = Some(from);
                }
                if inner.selected.is_some() && inner.state == LinkState::Punching {
                    inner.state = LinkState::Connected;
                }
            }
            ControlType::Keepalive | ControlType::Bye => {}
        }
        output
    }

    /// Data (SRT, or a side-channel datagram) arrived from `from`. False = not ours (drop it).
    pub fn on_data(&self, from: SocketAddr, now_us: i64) -> bool {
        let mut inner = lock(&self.inner);
        if inner.is_over() || !inner.confirmed.contains(&from) {
            return false;
        }
        inner.last_recv_us = now_us;
        if self.role == PeerRole::Host {
            // The host follows the viewer: answer wherever its data comes from (the viewer picked that path).
            if inner.selected != Some(from) {
                inner.selected = Some(from);
            }
            if inner.state == LinkState::Punching {
                inner.state = LinkState::Connected;
            }
        }
        true
    }

    /// Data is being sent on [`selected`](Self::selected) (resets the keepalive timer).
    pub fn note_sent(&self, now_us: i64) {
        lock(&self.inner).last_send_us = now_us;
    }

    /// Timers: punching, the IPv6 grace window, keepalives, deadlines. Returns datagrams to send.
    pub fn tick(&self, now_us: i64) -> Vec<Outgoing> {
        let mut output = Vec::new();
        let mut inner = lock(&self.inner);
        if inner.is_over() {
            return output;
        }

        if self.role == PeerRole::Viewer && inner.selected.is_none() {
            if let Some((address, acked_us)) = inner.acked_v4 {
                if now_us - acked_us >= IPV6_GRACE_US {
                    inner.selected = Some(address);
                    inner.state = LinkState::Connected;
                }
            }
        }

        if inner.state == LinkState::Punching {
            if now_us - self.start_us > PUNCH_DEADLINE_US {
                inner.state = LinkState::Failed;
                inner.reason = if inner.confirmed.is_empty() { "no candidate answered" } else { "packets arrived but the path never worked both ways" }.to_string();
                return output;
            }
            if inner.last_punch_us.is_none_or(|last| now_us - last >= PUNCH_INTERVAL_US) {
                inner.last_punch_us = Some(now_us);
                let punch = self.build(ControlType::Punch, now_us, 0);
                output.extend(inner.remote.iter().map(|&to| Outgoing { data: punch, to }));
                if !inner.remote.is_empty() {
                    inner.last_send_us = now_us;
                }
            }
            return output;
        }

        // Connected
        if now_us - inner.last_recv_us > SILENCE_TIMEOUT_US {
            inner.close("the other side went silent");
            return output;
        }
        if let Some(selected) = inner.selected {
            if now_us - inner.last_send_us >= KEEPALIVE_INTERVAL_US {
                output.push(Outgoing { data: self.build(ControlType::Keepalive, now_us, 0), to: selected });
                inner.last_send_us = now_us;
            }
        }
        output
    }

    /// Ends the session; returns the BYE datagrams (sent three times, fire and forget).
    pub fn close_with_bye(&self, reason: &str, now_us: i64) -> Vec<Outgoing> {
        let mut output = Vec::new();
        let mut inner = lock(&self.inner);
        if inner.is_over() {
            return output;
        }
        let mut targets: Vec<SocketAddr> = inner.selected.into_iter().collect();
        for address in &inner.confirmed {
            if !targets.contains(address) {
                targets.push(*address);
            }
        }
        let bye = self.build(ControlType::Bye, now_us, 0);
        for _ in 0..3 {
            output.extend(targets.iter().map(|&to| Outgoing { data: bye, to }));
        }
        inner.close(reason);
        output
    }

    fn build(&self, kind: ControlType, now_us: i64, echo: u64) -> [u8; control_packet::LENGTH] {
        control_packet::build(kind, self.role, &self.sid_bytes, now_us.max(1) as u64, echo, &self.punch_key)
    }
}

impl std::fmt::Debug for PeerSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = lock(&self.inner);
        f.debug_struct("PeerSession")
            .field("sid", &self.sid)
            .field("role", &self.role)
            .field("state", &inner.state)
            .field("reason", &inner.reason)
            .field("selected", &inner.selected)
            .finish()
    }
}
