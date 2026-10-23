//! A bound UDP socket with a receive thread of its own, and the monotonic clock every timer here runs on.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};
use std::ops::Deref;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

use crate::util::Worker;

/// Microseconds from a monotonic clock. Only differences mean anything; the first call is 1.
pub mod mono_clock {
    use super::*;

    pub fn now_us() -> i64 {
        static START: OnceLock<Instant> = OnceLock::new();
        let start = *START.get_or_init(Instant::now);
        // +1: a control packet's timestamp of 0 means "none" (see `PeerSession`).
        i64::try_from(start.elapsed().as_micros()).unwrap_or(i64::MAX - 1).saturating_add(1)
    }
}

/// Test hook standing in for the stateful firewalls on the real path: it sees every datagram a
/// [`UdpChannel`] sends and decides which arriving datagrams get through.
pub trait PacketFilter: Send + Sync {
    fn on_outbound(&self, to: SocketAddr);
    fn allow_inbound(&self, from: SocketAddr) -> bool;

    /// Whether `datagram` from `from` gets through (a lossy path decides by content); by default what
    /// [`allow_inbound`](Self::allow_inbound) says.
    fn allow_datagram(&self, from: SocketAddr, datagram: &[u8]) -> bool {
        let _ = datagram;
        self.allow_inbound(from)
    }
}

/// Called on the channel's receive thread for every datagram; the buffer is the thread's own and is reused.
pub type ReceiveHandler = Box<dyn FnMut(&ChannelIo, &[u8], SocketAddr) + Send>;

pub const DEFAULT_BUFFER_BYTES: usize = 4 << 20;

/// Not on Windows: how long a blocked receive goes without looking at the closed flag. Closing normally wakes
/// the thread at once (see [`UdpChannel::dispose`]); this is the fallback when that datagram cannot be delivered.
///
/// On Windows the receive must never have a timeout: a datagram that arrives just as the timeout expires is
/// lost (measured here: 2 of 400 on loopback; the Winsock documentation calls the socket's state "indeterminate"
/// after a timeout). Traffic on a steady beat, such as keepalives every 2 s, would keep hitting that moment.
#[cfg(not(windows))]
const RECEIVE_POLL: Duration = Duration::from_millis(250);

/// The socket of a [`UdpChannel`]: what a receive handler may use while it runs.
pub struct ChannelIo {
    name: String,
    /// `None` once closed. The receive thread has a handle of its own; the socket is released when both are gone.
    socket: RwLock<Option<Arc<UdpSocket>>>,
    local: SocketAddr,
    filter: Option<Arc<dyn PacketFilter>>,
    closed: AtomicBool,
    datagrams_in: AtomicU64,
    datagrams_out: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
}

impl ChannelIo {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    pub fn is_ipv6(&self) -> bool {
        self.local.is_ipv6()
    }

    pub fn datagrams_in(&self) -> u64 {
        self.datagrams_in.load(Ordering::Relaxed)
    }

    pub fn datagrams_out(&self) -> u64 {
        self.datagrams_out.load(Ordering::Relaxed)
    }

    pub fn bytes_in(&self) -> u64 {
        self.bytes_in.load(Ordering::Relaxed)
    }

    pub fn bytes_out(&self) -> u64 {
        self.bytes_out.load(Ordering::Relaxed)
    }

    /// False when closed, when `to` is of the other address family, or when the system refused the datagram.
    pub fn send(&self, data: &[u8], to: SocketAddr) -> bool {
        if self.closed.load(Ordering::Acquire) || to.is_ipv6() != self.local.is_ipv6() {
            return false;
        }
        let socket = self.socket.read().unwrap_or_else(|e| e.into_inner());
        let Some(socket) = socket.as_ref() else { return false };
        if let Some(filter) = &self.filter {
            filter.on_outbound(to);
        }
        match socket.send_to(data, to) {
            Ok(_) => {
                self.datagrams_out.fetch_add(1, Ordering::Relaxed);
                self.bytes_out.fetch_add(data.len() as u64, Ordering::Relaxed);
                true
            }
            Err(_) => false,
        }
    }
}

/// A bound UDP socket with a dedicated receive thread (a punch deadline is 15 s: nothing here may wait for a
/// shared pool). The handler runs on that thread with a shared buffer and must not keep it.
pub struct UdpChannel {
    io: Arc<ChannelIo>,
    thread: Worker,
}

impl UdpChannel {
    pub fn bind(bind: SocketAddr, name: &str, on_receive: ReceiveHandler) -> io::Result<UdpChannel> {
        UdpChannel::bind_with(bind, name, None, DEFAULT_BUFFER_BYTES, on_receive)
    }

    pub fn bind_with(
        bind: SocketAddr,
        name: &str,
        filter: Option<Arc<dyn PacketFilter>>,
        buffer_bytes: usize,
        mut on_receive: ReceiveHandler,
    ) -> io::Result<UdpChannel> {
        let socket = Socket::new(Domain::for_address(bind), Type::DGRAM, Some(Protocol::UDP))?;
        if bind.is_ipv6() {
            socket.set_only_v6(true)?;
        }
        // Best effort: the system may cap these.
        let _ = socket.set_recv_buffer_size(buffer_bytes);
        let _ = socket.set_send_buffer_size(buffer_bytes);
        disable_connection_reset(&socket);
        socket.bind(&bind.into())?;
        let socket: UdpSocket = socket.into();
        #[cfg(not(windows))]
        socket.set_read_timeout(Some(RECEIVE_POLL))?;
        let local = canonical(socket.local_addr()?);
        let socket = Arc::new(socket);

        let io = Arc::new(ChannelIo {
            name: name.to_string(),
            socket: RwLock::new(Some(socket.clone())),
            local,
            filter,
            closed: AtomicBool::new(false),
            datagrams_in: AtomicU64::new(0),
            datagrams_out: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
        });
        let shared = io.clone();
        let thread = Worker::spawn(&format!("udp-{name}"), move || receive_loop(&shared, &socket, &mut on_receive))?;
        Ok(UdpChannel { io, thread })
    }

    /// Stops receiving and sending and closes the socket: the port is free again when this returns. Waits (up
    /// to a second) for the receive thread unless called from it; a handler that is still running then keeps
    /// the socket until it returns.
    pub fn dispose(&self) {
        if self.io.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let socket = self.io.socket.write().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(socket) = &socket {
            // The receive thread is most likely blocked in a receive: wake it with a datagram to itself.
            let _ = socket.send_to(&[], wake_address(self.io.local));
        }
        if !self.thread.is_current() {
            // Should that datagram not arrive (the address it was bound to is gone), the receive is cancelled
            // (Windows) or runs into its poll interval (elsewhere).
            for _ in 0..10 {
                if self.thread.join(Duration::from_millis(100)) {
                    break;
                }
                if let Some(socket) = &socket {
                    cancel_blocked_receive(socket);
                }
            }
        }
        drop(socket);
    }
}

impl Deref for UdpChannel {
    type Target = ChannelIo;

    fn deref(&self) -> &ChannelIo {
        &self.io
    }
}

impl Drop for UdpChannel {
    fn drop(&mut self) {
        self.dispose();
    }
}

fn receive_loop(io: &ChannelIo, socket: &UdpSocket, on_receive: &mut ReceiveHandler) {
    let mut buffer = vec![0u8; 65536];
    let mut errors = 0u32;
    while !io.closed.load(Ordering::Acquire) {
        let (n, from) = match socket.recv_from(&mut buffer) {
            Ok(received) => {
                errors = 0;
                received
            }
            Err(e) => {
                if io.closed.load(Ordering::Acquire) {
                    return;
                }
                match e.kind() {
                    // the poll interval (not on Windows), or a signal
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted => {}
                    // An ICMP error for an earlier send (an unreachable candidate): keep going.
                    io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionRefused => {}
                    _ => {
                        errors += 1;
                        if errors > 50 {
                            thread::sleep(Duration::from_millis(20));
                        }
                    }
                }
                continue;
            }
        };
        if io.closed.load(Ordering::Acquire) {
            return;
        }
        let from = canonical(from);
        if io.filter.as_ref().is_some_and(|filter| !filter.allow_datagram(from, &buffer[..n])) {
            continue;
        }
        io.datagrams_in.fetch_add(1, Ordering::Relaxed);
        io.bytes_in.fetch_add(n as u64, Ordering::Relaxed);
        // a handler bug must not stop the socket
        let _ = catch_unwind(AssertUnwindSafe(|| on_receive(io, &buffer[..n], from)));
    }
}

/// Socket addresses are compared and used as map keys: an IPv6 flow label (some systems report one for
/// received datagrams) must not make two packets of one peer look like two peers.
pub(crate) fn canonical(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) if v6.flowinfo() != 0 => SocketAddr::V6(SocketAddrV6::new(*v6.ip(), v6.port(), 0, v6.scope_id())),
        other => other,
    }
}

/// Where a datagram to this socket itself goes: its own address, or loopback when it is bound to "any".
fn wake_address(local: SocketAddr) -> SocketAddr {
    match local {
        SocketAddr::V4(v4) if v4.ip().is_unspecified() => SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, v4.port())),
        SocketAddr::V6(v6) if v6.ip().is_unspecified() => {
            SocketAddr::V6(SocketAddrV6::new(std::net::Ipv6Addr::LOCALHOST, v6.port(), 0, 0))
        }
        other => other,
    }
}

/// Windows turns an ICMP "port unreachable" for an earlier send into WSAECONNRESET on the next receive.
#[cfg(windows)]
fn disable_connection_reset(socket: &Socket) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{WSAIoctl, SIO_UDP_CONNRESET, SOCKET};

    let off = 0u32;
    let mut returned = 0u32;
    // SAFETY: the socket is open for the duration of the call; the input buffer is 4 readable bytes, there is
    // no output buffer and no overlapped operation.
    unsafe {
        WSAIoctl(
            SOCKET(socket.as_raw_socket() as usize),
            SIO_UDP_CONNRESET,
            Some(std::ptr::from_ref(&off).cast()),
            std::mem::size_of::<u32>() as u32,
            None,
            0,
            &mut returned,
            None,
            None,
        );
    }
}

/// Linux and macOS: not a thing there.
#[cfg(not(windows))]
fn disable_connection_reset(_socket: &Socket) {}

/// Makes a receive that another thread is blocked in on this socket return (with an error) now.
#[cfg(windows)]
fn cancel_blocked_receive(socket: &UdpSocket) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::CancelIoEx;

    // SAFETY: the socket is open for the duration of the call (we hold it); cancelling the I/O pending on a
    // handle has no other precondition, and "nothing was pending" is a harmless error.
    let _ = unsafe { CancelIoEx(HANDLE(socket.as_raw_socket() as usize as *mut std::ffi::c_void), None) };
}

/// Elsewhere the receive has a poll interval, after which the thread sees that the channel is closed.
#[cfg(not(windows))]
fn cancel_blocked_receive(_socket: &UdpSocket) {}

/// A UDP port that is free on 127.0.0.1 right now (whoever asked binds it a moment later).
pub fn free_loopback_udp_port() -> io::Result<u16> {
    Ok(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?.local_addr()?.port())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn loopback() -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
    }

    #[test]
    fn datagrams_arrive_with_their_source_and_are_counted() {
        let (tx, rx) = mpsc::channel();
        let receiver = UdpChannel::bind(
            loopback(),
            "test-rx",
            Box::new(move |io, data, from| {
                let _ = tx.send((data.to_vec(), from, io.local_addr()));
            }),
        )
        .unwrap();
        let sender = UdpChannel::bind(loopback(), "test-tx", Box::new(|_, _, _| {})).unwrap();
        assert!(sender.send(b"hello", receiver.local_addr()));
        let (data, from, local) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(data, b"hello");
        assert_eq!(from, sender.local_addr());
        assert_eq!(local, receiver.local_addr());
        assert_eq!((sender.datagrams_out(), sender.bytes_out()), (1, 5));
        assert_eq!((receiver.datagrams_in(), receiver.bytes_in()), (1, 5));
        assert!(!sender.send(b"x", "[::1]:9".parse().unwrap()), "the other address family");
    }

    #[test]
    fn closing_is_prompt_and_stops_both_directions() {
        let (tx, rx) = mpsc::channel();
        let channel = UdpChannel::bind(
            loopback(),
            "test-close",
            Box::new(move |_, data, _| {
                let _ = tx.send(data.len());
            }),
        )
        .unwrap();
        let address = channel.local_addr();
        let started = Instant::now();
        channel.dispose();
        channel.dispose();
        assert!(started.elapsed() < Duration::from_millis(900), "closing took {:?}", started.elapsed());
        assert!(!channel.send(b"x", address));
        // the port is free again at once (mpv's fixed local port is bound anew by the next session)
        let again = UdpSocket::bind(address).expect("a closed channel has let go of its port");
        again.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let other = UdpSocket::bind(loopback()).unwrap();
        other.send_to(b"late", address).unwrap();
        let mut buffer = [0u8; 16];
        assert_eq!(again.recv_from(&mut buffer).map(|(n, _)| n).ok(), Some(4), "the new owner of the port gets its datagrams");
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err(), "nothing is delivered after closing");
    }

    /// The receive thread blocks without a timeout on Windows (see RECEIVE_POLL for why); closing still ends
    /// it, even when the datagram that normally wakes it never arrives.
    #[test]
    fn a_blocked_receive_is_ended_without_the_wake_datagram() {
        let channel = UdpChannel::bind(loopback(), "test-cancel", Box::new(|_, _, _| {})).unwrap();
        let socket = channel.io.socket.read().unwrap().clone().expect("open");
        #[cfg(windows)]
        assert_eq!(socket.read_timeout().unwrap(), None, "a receive timeout loses datagrams on Windows");
        thread::sleep(Duration::from_millis(100)); // the thread is in its receive by now

        // what dispose does, minus the datagram to itself
        channel.io.closed.store(true, Ordering::Release);
        let started = Instant::now();
        let mut ended = false;
        for _ in 0..20 {
            cancel_blocked_receive(&socket);
            if channel.thread.join(Duration::from_millis(100)) {
                ended = true;
                break;
            }
        }
        assert!(ended, "the receive thread is still blocked after {:?}", started.elapsed());
        assert!(started.elapsed() < Duration::from_millis(1500), "{:?}", started.elapsed());
    }

    #[test]
    fn a_panicking_handler_does_not_stop_the_socket() {
        let (tx, rx) = mpsc::channel();
        let channel = UdpChannel::bind(
            loopback(),
            "test-panic",
            Box::new(move |_, data, _| {
                if data == b"boom" {
                    panic!("handler bug (on purpose)");
                }
                let _ = tx.send(data.to_vec());
            }),
        )
        .unwrap();
        let other = UdpSocket::bind(loopback()).unwrap();
        other.send_to(b"boom", channel.local_addr()).unwrap();
        other.send_to(b"after", channel.local_addr()).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), b"after");
    }

    #[test]
    fn the_clock_only_goes_forward_and_never_reads_zero() {
        let a = mono_clock::now_us();
        thread::sleep(Duration::from_millis(5));
        let b = mono_clock::now_us();
        assert!(a >= 1);
        assert!(b - a >= 4_000, "{a} {b}");
    }

    #[test]
    fn flow_labels_do_not_tell_peers_apart() {
        let labelled = SocketAddr::V6(SocketAddrV6::new("2400::1".parse().unwrap(), 5000, 0x1234, 0));
        assert_eq!(canonical(labelled), "[2400::1]:5000".parse().unwrap());
        let v4: SocketAddr = "198.51.100.1:5".parse().unwrap();
        assert_eq!(canonical(v4), v4);
    }
}
