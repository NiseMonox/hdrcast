//! What both roles of a link share: the two WAN sockets, the candidate list, the UDP echo and the 20 ms timer.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant};

use hdrcast_core::endpoint;
use hdrcast_core::group::keys;
use hdrcast_core::GroupKeys;

use crate::peer_session::Outgoing;
use crate::udp_channel::{mono_clock, PacketFilter, UdpChannel, DEFAULT_BUFFER_BYTES};
use crate::util::{lock, Worker};
use crate::{control_packet, echo_protocol, local_addresses, side_channel};

/// Where a peer's WAN sockets bind and what it advertises.
#[derive(Clone)]
pub struct LinkAddresses {
    /// This PC's stable global IPv6 address (bound directly so replies and PUNCHes leave from it); `None` = no IPv6.
    pub ipv6: Option<Ipv6Addr>,
    /// IPv4 bind address (normally 0.0.0.0); `None` = no IPv4 socket.
    pub ipv4_bind: Option<Ipv4Addr>,
    /// IPv4 address to advertise when bound to 0.0.0.0 (the LAN address); `None` = do not advertise.
    pub ipv4_advertise: Option<Ipv4Addr>,
    /// Test hooks.
    pub filter_v6: Option<Arc<dyn PacketFilter>>,
    pub filter_v4: Option<Arc<dyn PacketFilter>>,
}

impl Default for LinkAddresses {
    fn default() -> Self {
        LinkAddresses { ipv6: None, ipv4_bind: Some(Ipv4Addr::UNSPECIFIED), ipv4_advertise: None, filter_v6: None, filter_v4: None }
    }
}

impl LinkAddresses {
    /// What this PC offers by default: its stable IPv6 and its LAN IPv4.
    pub fn detect() -> LinkAddresses {
        LinkAddresses {
            ipv6: local_addresses::best_ipv6(),
            ipv4_bind: Some(Ipv4Addr::UNSPECIFIED),
            ipv4_advertise: local_addresses::lan_ipv4(),
            filter_v6: None,
            filter_v4: None,
        }
    }
}

impl std::fmt::Debug for LinkAddresses {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkAddresses")
            .field("ipv6", &self.ipv6)
            .field("ipv4_bind", &self.ipv4_bind)
            .field("ipv4_advertise", &self.ipv4_advertise)
            .field("filter_v6", &self.filter_v6.is_some())
            .field("filter_v4", &self.filter_v4.is_some())
            .finish()
    }
}

/// The candidate the UDP echo vouches for: the public IPv4 mapping it saw.
pub const SOURCE_ECHO: &str = "echo";
/// The candidate the router vouches for: its forward of our port.
pub const SOURCE_ROUTER: &str = "router";

/// What a role does with what arrives on the WAN sockets and with the timer. Everything is called on a network
/// thread (the IPv6 socket's, the IPv4 socket's or the timer's).
pub(crate) trait LinkEvents: Send + Sync {
    /// A datagram that looks like a control packet (authenticity not yet checked).
    fn on_control(&self, packet: &[u8], from: SocketAddr);
    /// A well-formed side-channel datagram (nothing about its session or source checked yet).
    fn on_side(&self, sid: &str, payload: &[u8], from: SocketAddr);
    /// Any other datagram (SRT, or garbage).
    fn on_wan_data(&self, datagram: &[u8], from: SocketAddr);
    fn on_tick(&self, now_us: i64);
}

/// The two WAN sockets, the candidate list and the 20 ms timer shared by both roles.
pub(crate) struct LinkCore {
    keys: GroupKeys,
    v6: Option<UdpChannel>,
    v4: Option<UdpChannel>,
    candidates: Mutex<Candidates>,
    shared: Arc<Shared>,
    ticker: Mutex<Option<Worker>>,
    name: String,
}

#[derive(Default)]
struct Candidates {
    own: Vec<String>,
    /// (source, candidate), in the order the sources first spoke.
    learned: Vec<(String, String)>,
}

/// What the receive threads and the timer need: they outlive nothing, they only hold this.
struct Shared {
    disposed: AtomicBool,
    events: OnceLock<Weak<dyn LinkEvents>>,
    echo: Mutex<Echo>,
    echo_answered: Condvar,
}

#[derive(Default)]
struct Echo {
    nonce: Option<[u8; echo_protocol::NONCE_LENGTH]>,
    server: Option<SocketAddr>,
    result: Option<SocketAddr>,
}

impl Shared {
    fn events(&self) -> Option<Arc<dyn LinkEvents>> {
        self.events.get()?.upgrade()
    }

    fn on_wan_receive(&self, datagram: &[u8], from: SocketAddr) {
        if self.disposed.load(Ordering::Acquire) {
            return;
        }
        if control_packet::looks_like_control(datagram) {
            if let Some(events) = self.events() {
                events.on_control(datagram, from);
            }
        } else if echo_protocol::looks_like_reply(datagram) {
            self.on_echo_reply(datagram, from);
        } else if side_channel::has_magic(datagram) {
            // Never SRT, even when malformed: an old or confused peer's ffmpeg / mpv must not see it.
            if let (Some((sid, payload)), Some(events)) = (side_channel::parse(datagram), self.events()) {
                events.on_side(&sid, payload, from);
            }
        } else if let Some(events) = self.events() {
            events.on_wan_data(datagram, from);
        }
    }

    fn on_echo_reply(&self, datagram: &[u8], from: SocketAddr) {
        let mut echo = lock(&self.echo);
        let (Some(nonce), Some(server)) = (echo.nonce, echo.server) else { return };
        if from != server {
            return;
        }
        if let Some(mapped) = echo_protocol::parse_reply(datagram, &nonce) {
            echo.result = Some(mapped);
            self.echo_answered.notify_all();
        }
    }
}

impl LinkCore {
    /// Binds the sockets. Nothing that arrives is acted on before [`start`](Self::start).
    pub fn new(keys: &GroupKeys, addresses: &LinkAddresses, name: &str) -> io::Result<LinkCore> {
        let shared = Arc::new(Shared {
            disposed: AtomicBool::new(false),
            events: OnceLock::new(),
            echo: Mutex::new(Echo::default()),
            echo_answered: Condvar::new(),
        });
        let mut candidates = Candidates::default();
        let wan = |bind: SocketAddr, suffix: &str, filter: &Option<Arc<dyn PacketFilter>>| {
            let shared = shared.clone();
            UdpChannel::bind_with(
                bind,
                &format!("{name}-{suffix}"),
                filter.clone(),
                DEFAULT_BUFFER_BYTES,
                Box::new(move |_, datagram, from| shared.on_wan_receive(datagram, from)),
            )
        };

        let mut v6 = None;
        if let Some(address) = addresses.ipv6 {
            let channel = wan(SocketAddr::new(IpAddr::V6(address), 0), "v6", &addresses.filter_v6)?;
            candidates.own.push(endpoint::format(&channel.local_addr()));
            v6 = Some(channel);
        }
        let mut v4 = None;
        if let Some(bind) = addresses.ipv4_bind {
            // an error here drops (and so closes) the IPv6 socket that was just bound
            let channel = wan(SocketAddr::new(IpAddr::V4(bind), 0), "v4", &addresses.filter_v4)?;
            let advertise = if bind.is_unspecified() { addresses.ipv4_advertise } else { Some(bind) };
            if let Some(advertise) = advertise {
                candidates.own.push(endpoint::format(&SocketAddr::new(IpAddr::V4(advertise), channel.local_addr().port())));
            }
            v4 = Some(channel);
        }
        Ok(LinkCore { keys: keys.clone(), v6, v4, candidates: Mutex::new(candidates), shared, ticker: Mutex::new(None), name: name.to_string() })
    }

    /// Connects the sockets and the timer to the role's handlers and starts the timer. Call once, when the
    /// role object exists.
    pub fn start(&self, events: Weak<dyn LinkEvents>) -> io::Result<()> {
        let _ = self.shared.events.set(events);
        let shared = self.shared.clone();
        let ticker = Worker::spawn(&format!("{}-tick", self.name), move || {
            while !shared.disposed.load(Ordering::Acquire) {
                if let Some(events) = shared.events() {
                    // keep ticking; a failed send is retried on the next tick
                    let _ = catch_unwind(AssertUnwindSafe(|| events.on_tick(mono_clock::now_us())));
                }
                thread::sleep(Duration::from_millis(20));
            }
        })?;
        *lock(&self.ticker) = Some(ticker);
        Ok(())
    }

    pub fn keys(&self) -> &GroupKeys {
        &self.keys
    }

    pub fn has_ipv6(&self) -> bool {
        self.v6.is_some()
    }

    pub fn has_ipv4(&self) -> bool {
        self.v4.is_some()
    }

    pub fn ipv4_port(&self) -> Option<u16> {
        self.v4.as_ref().map(|channel| channel.local_addr().port())
    }

    pub fn ipv6_addr(&self) -> Option<SocketAddr> {
        self.v6.as_ref().map(|channel| channel.local_addr())
    }

    /// "[v6]:port" and "lan4:port", then the ones set with [`set_candidate`](Self::set_candidate) (duplicates once).
    pub fn candidates(&self) -> Vec<String> {
        let candidates = lock(&self.candidates);
        let mut all = candidates.own.clone();
        for (_, candidate) in &candidates.learned {
            if !all.contains(candidate) {
                all.push(candidate.clone());
            }
        }
        all
    }

    /// Sets, replaces or (with `None`) removes the candidate one source vouches for: [`SOURCE_ECHO`] or
    /// [`SOURCE_ROUTER`].
    pub fn set_candidate(&self, source: &str, candidate: Option<String>) {
        let mut candidates = lock(&self.candidates);
        let at = candidates.learned.iter().position(|(s, _)| s == source);
        match (candidate, at) {
            (None, Some(i)) => {
                candidates.learned.remove(i);
            }
            (None, None) => {}
            (Some(candidate), Some(i)) => candidates.learned[i].1 = candidate,
            (Some(candidate), None) => candidates.learned.push((source.to_string(), candidate)),
        }
    }

    pub fn wan_bytes_in(&self) -> u64 {
        self.v6.as_ref().map_or(0, |c| c.bytes_in()) + self.v4.as_ref().map_or(0, |c| c.bytes_in())
    }

    pub fn wan_bytes_out(&self) -> u64 {
        self.v6.as_ref().map_or(0, |c| c.bytes_out()) + self.v4.as_ref().map_or(0, |c| c.bytes_out())
    }

    pub fn send_wan(&self, datagram: &[u8], to: SocketAddr) -> bool {
        let channel = if to.is_ipv6() { &self.v6 } else { &self.v4 };
        channel.as_ref().is_some_and(|channel| channel.send(datagram, to))
    }

    pub fn send_all(&self, packets: &[Outgoing]) {
        for packet in packets {
            self.send_wan(&packet.data, packet.to);
        }
    }

    /// Asks the signaling server's UDP echo what address and port it saw for the IPv4 socket (the NAT mapping
    /// peers must punch towards; docs/design.md §8.7). `None` without an IPv4 socket, or when no answer comes
    /// in time.
    pub fn discover_public_ipv4(&self, echo_server: SocketAddr, timeout_ms: u64) -> Option<SocketAddr> {
        let v4 = self.v4.as_ref()?;
        if !echo_server.is_ipv4() {
            return None;
        }
        let nonce: [u8; echo_protocol::NONCE_LENGTH] = keys::random_bytes(echo_protocol::NONCE_LENGTH).try_into().expect("16 bytes");
        {
            let mut echo = lock(&self.shared.echo);
            echo.nonce = Some(nonce);
            echo.server = Some(echo_server);
            echo.result = None;
        }
        let request = echo_protocol::build_request(&nonce);
        let slice = Duration::from_millis((timeout_ms / 3).max(100));
        for _ in 0..3 {
            if lock(&self.shared.echo).result.is_some() {
                break;
            }
            v4.send(&request, echo_server);
            let deadline = Instant::now() + slice;
            let mut echo = lock(&self.shared.echo);
            while echo.result.is_none() {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                echo = self.shared.echo_answered.wait_timeout(echo, left).unwrap_or_else(|e| e.into_inner()).0;
            }
        }
        let mut echo = lock(&self.shared.echo);
        echo.nonce = None;
        echo.result.take()
    }

    /// Runs `before_closing` (the role's goodbyes, which still need the sockets), then closes the sockets and
    /// stops the timer. Does nothing the second time.
    pub fn dispose(&self, before_closing: impl FnOnce()) {
        if self.shared.disposed.load(Ordering::Acquire) {
            return;
        }
        let _ = catch_unwind(AssertUnwindSafe(before_closing));
        if self.shared.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(channel) = &self.v6 {
            channel.dispose();
        }
        if let Some(channel) = &self.v4 {
            channel.dispose();
        }
        if let Some(ticker) = lock(&self.ticker).take() {
            ticker.join(Duration::from_millis(1000));
        }
    }
}
