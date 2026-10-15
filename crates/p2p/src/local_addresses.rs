//! Which addresses to bind and advertise (docs/design.md §8.7). The route source for a global destination
//! tells us the interface; on it we prefer the stable address, because Windows (and macOS) send from a
//! temporary address by default and a firewall pinhole opened from one address does not cover the other.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

/// One unicast address of this PC as the OS reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressCandidate {
    pub address: IpAddr,
    /// What tells interfaces apart: the adapter GUID on Windows, the interface name elsewhere.
    pub interface_id: String,
    /// A privacy (RFC 4941) address: Windows reports the suffix origin "random" for these, macOS the
    /// "temporary" flag.
    pub temporary: bool,
    /// Duplicate address detection passed and the address is not deprecated (tentative / deprecated ones are not).
    pub preferred: bool,
    pub preferred_lifetime_seconds: i64,
}

impl AddressCandidate {
    pub fn new(address: IpAddr, interface_id: &str, temporary: bool, preferred: bool, preferred_lifetime_seconds: i64) -> AddressCandidate {
        AddressCandidate { address, interface_id: interface_id.to_string(), temporary, preferred, preferred_lifetime_seconds }
    }
}

const PROBE_V6: Ipv6Addr = Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888);
const PROBE_V4: Ipv4Addr = Ipv4Addr::new(8, 8, 8, 8);

/// The IPv6 address this PC would send from for an internet destination (UDP connect, nothing is sent).
/// `None`: no route (no IPv6 at all), or IPv6 disabled.
pub fn route_source_v6() -> Option<Ipv6Addr> {
    let socket = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((PROBE_V6, 53)).ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V6(local) => Some(*local.ip()),
        SocketAddr::V4(_) => None,
    }
}

/// The IPv4 address this PC would send from for an internet destination (UDP connect, nothing is sent).
pub fn route_source_v4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((PROBE_V4, 53)).ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(local) => Some(*local.ip()),
        SocketAddr::V6(_) => None,
    }
}

/// This PC's stable global IPv6 address, or `None` when it has no IPv6 internet route.
pub fn best_ipv6() -> Option<Ipv6Addr> {
    let source = route_source_v6().filter(|source| is_global_unicast_v6(&IpAddr::V6(*source)))?;
    pick_stable_ipv6(Some(source), &read())
}

/// The LAN (or public) IPv4 address used for the internet route; `None` without IPv4.
pub fn lan_ipv4() -> Option<Ipv4Addr> {
    route_source_v4().filter(|a| !a.is_loopback() && !a.is_unspecified())
}

/// Pure choice: on the interface that owns `route_source`, a global, non-temporary, preferred IPv6 address
/// (same /64 first, then the longest preferred lifetime); otherwise the route source itself.
pub fn pick_stable_ipv6(route_source: Option<Ipv6Addr>, all: &[AddressCandidate]) -> Option<Ipv6Addr> {
    let route_source = route_source.filter(|source| is_global_unicast_v6(&IpAddr::V6(*source)))?;
    let Some(interface) = all.iter().find(|c| c.address == IpAddr::V6(route_source)).map(|c| c.interface_id.as_str()) else {
        return Some(route_source);
    };

    let mut best: Option<(&AddressCandidate, Ipv6Addr)> = None;
    for candidate in all {
        let IpAddr::V6(address) = candidate.address else { continue };
        if candidate.interface_id != interface || candidate.temporary || !candidate.preferred || !is_global_unicast_v6(&candidate.address) {
            continue;
        }
        let Some((current, current_address)) = best else {
            best = Some((candidate, address));
            continue;
        };
        let (new_same, current_same) = (same_prefix64(&address, &route_source), same_prefix64(&current_address, &route_source));
        if new_same != current_same {
            if new_same {
                best = Some((candidate, address));
            }
            continue;
        }
        if candidate.preferred_lifetime_seconds > current.preferred_lifetime_seconds {
            best = Some((candidate, address));
        }
    }
    Some(best.map_or(route_source, |(_, address)| address))
}

/// 2000::/3 minus Teredo (2001::/32), 6to4 (2002::/16) and documentation (2001:db8::/32).
pub fn is_global_unicast_v6(address: &IpAddr) -> bool {
    let IpAddr::V6(v6) = address else { return false };
    let b = v6.octets();
    if b[0] & 0xE0 != 0x20 {
        return false; // also every IPv4-mapped address
    }
    if b[0] == 0x20 && b[1] == 0x01 && b[2] == 0x00 && b[3] == 0x00 {
        return false; // Teredo
    }
    if b[0] == 0x20 && b[1] == 0x02 {
        return false; // 6to4
    }
    if b[0] == 0x20 && b[1] == 0x01 && b[2] == 0x0d && b[3] == 0xb8 {
        return false; // documentation
    }
    true
}

/// An IPv4 address outside the private, shared (CGNAT), loopback, link-local and multicast ranges.
pub fn is_public_ipv4(address: &IpAddr) -> bool {
    let IpAddr::V4(v4) = address else { return false };
    let b = v4.octets();
    !(b[0] == 0
        || b[0] == 10
        || b[0] == 127
        || (b[0] == 172 && (16..=31).contains(&b[1]))
        || (b[0] == 192 && b[1] == 168)
        || (b[0] == 100 && (64..=127).contains(&b[1]))
        || (b[0] == 169 && b[1] == 254)
        || b[0] >= 224)
}

fn same_prefix64(x: &Ipv6Addr, y: &Ipv6Addr) -> bool {
    x.octets()[..8] == y.octets()[..8]
}

/// Every unicast address of every interface that is up. Where the system does not say whether an address is
/// temporary or deprecated (Linux through `getifaddrs`), it counts as stable and preferred.
pub fn read() -> Vec<AddressCandidate> {
    platform::read()
}

#[cfg(windows)]
mod platform {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{
        IpDadStatePreferred, IpSuffixOriginRandom, AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
    };

    use super::AddressCandidate;

    pub fn read() -> Vec<AddressCandidate> {
        let mut list = Vec::new();
        // u64 storage: the structures hold pointers and 64-bit integers and must be aligned for them.
        let mut size: u32 = 16 * 1024;
        let mut buffer: Vec<u64> = Vec::new();
        let mut filled = false;
        for _ in 0..4 {
            buffer.resize((size as usize).div_ceil(8), 0);
            // SAFETY: `buffer` is writable for `size` bytes and suitably aligned; `size` is updated by the call.
            let result = unsafe {
                GetAdaptersAddresses(
                    u32::from(AF_UNSPEC.0),
                    GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                    None,
                    Some(buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()),
                    &mut size,
                )
            };
            if result == ERROR_SUCCESS.0 {
                filled = true;
                break;
            }
            if result != ERROR_BUFFER_OVERFLOW.0 {
                return list; // no adapters, or the query failed: nothing to offer
            }
            // `size` now says how much is needed; an adapter may appear between the two calls, so loop
        }
        if !filled {
            return list;
        }

        let mut adapter = buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: the list was written by GetAdaptersAddresses into `buffer`, which is alive and unchanged for
        // the whole walk; every pointer in it points into the same buffer or is null.
        unsafe {
            while !adapter.is_null() {
                let a = &*adapter;
                adapter = a.Next;
                if a.OperStatus != IfOperStatusUp {
                    continue;
                }
                let id = if a.AdapterName.is_null() { String::new() } else { String::from_utf8_lossy(a.AdapterName.as_bytes()).into_owned() };
                let mut unicast = a.FirstUnicastAddress;
                while !unicast.is_null() {
                    let u = &*unicast;
                    unicast = u.Next;
                    let sockaddr = u.Address.lpSockaddr;
                    if sockaddr.is_null() {
                        continue;
                    }
                    let family = (*sockaddr).sa_family;
                    let address = if family == AF_INET && u.Address.iSockaddrLength as usize >= std::mem::size_of::<SOCKADDR_IN>() {
                        let v4 = &*sockaddr.cast::<SOCKADDR_IN>();
                        IpAddr::V4(Ipv4Addr::from(v4.sin_addr.S_un.S_addr.to_ne_bytes()))
                    } else if family == AF_INET6 && u.Address.iSockaddrLength as usize >= std::mem::size_of::<SOCKADDR_IN6>() {
                        let v6 = &*sockaddr.cast::<SOCKADDR_IN6>();
                        IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.u.Byte))
                    } else {
                        continue;
                    };
                    list.push(AddressCandidate {
                        address,
                        interface_id: id.clone(),
                        temporary: u.SuffixOrigin == IpSuffixOriginRandom,
                        preferred: u.DadState == IpDadStatePreferred,
                        preferred_lifetime_seconds: i64::from(u.PreferredLifetime),
                    });
                }
            }
        }
        list
    }
}

#[cfg(unix)]
mod platform {
    use std::ffi::CStr;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::AddressCandidate;

    pub fn read() -> Vec<AddressCandidate> {
        let mut list = Vec::new();
        let mut first: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: getifaddrs writes one pointer; the list it allocates is freed below.
        if unsafe { libc::getifaddrs(&mut first) } != 0 || first.is_null() {
            return list;
        }
        let flags = v6_flags::Query::open();
        let mut at = first;
        // SAFETY: the list is valid until freeifaddrs; every `ifa_addr` is null or points to a socket address
        // whose `sa_family` says how large it is.
        unsafe {
            while !at.is_null() {
                let ifa = &*at;
                at = ifa.ifa_next;
                let up = libc::IFF_UP as libc::c_uint | libc::IFF_RUNNING as libc::c_uint;
                if ifa.ifa_addr.is_null() || ifa.ifa_name.is_null() || ifa.ifa_flags & up != up {
                    continue;
                }
                let name = CStr::from_ptr(ifa.ifa_name);
                match libc::c_int::from((*ifa.ifa_addr).sa_family) {
                    libc::AF_INET => {
                        let v4 = &*ifa.ifa_addr.cast::<libc::sockaddr_in>();
                        list.push(AddressCandidate {
                            address: IpAddr::V4(Ipv4Addr::from(v4.sin_addr.s_addr.to_ne_bytes())),
                            interface_id: name.to_string_lossy().into_owned(),
                            temporary: false,
                            preferred: true,
                            preferred_lifetime_seconds: i64::MAX,
                        });
                    }
                    libc::AF_INET6 => {
                        let v6 = &*ifa.ifa_addr.cast::<libc::sockaddr_in6>();
                        let state = flags.of(name, v6);
                        list.push(AddressCandidate {
                            address: IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.s6_addr)),
                            interface_id: name.to_string_lossy().into_owned(),
                            temporary: state.temporary,
                            preferred: state.preferred,
                            preferred_lifetime_seconds: state.preferred_lifetime_seconds,
                        });
                    }
                    _ => {}
                }
            }
            libc::freeifaddrs(first);
        }
        list
    }

    /// What the system says about one IPv6 address beyond the address itself.
    pub struct V6State {
        pub temporary: bool,
        pub preferred: bool,
        pub preferred_lifetime_seconds: i64,
    }

    impl V6State {
        /// The system does not say: the address counts as stable and preferred, as it always did off Windows.
        const UNKNOWN: V6State = V6State { temporary: false, preferred: true, preferred_lifetime_seconds: i64::MAX };
    }

    /// macOS: the per-address flags (temporary, deprecated, tentative, …) and lifetimes are read with the
    /// SIOCGIFAFLAG_IN6 / SIOCGIFALIFETIME_IN6 ioctls on any IPv6 datagram socket (what `ifconfig` does).
    /// Should an ioctl fail, that address reads as stable and preferred; `best_ipv6` then still returns an
    /// address of the route's interface, possibly the temporary one the route source already was.
    ///
    /// Written from the XNU headers and type-checked for macOS, but not run on a Mac yet.
    #[cfg(target_os = "macos")]
    mod v6_flags {
        use std::ffi::CStr;

        use super::V6State;

        const IN6_IFF_TENTATIVE: libc::c_int = 0x0002;
        const IN6_IFF_DUPLICATED: libc::c_int = 0x0004;
        const IN6_IFF_DETACHED: libc::c_int = 0x0008;
        const IN6_IFF_DEPRECATED: libc::c_int = 0x0010;
        const IN6_IFF_TEMPORARY: libc::c_int = 0x0080;
        const ND6_INFINITE_LIFETIME: u32 = 0xffff_ffff;

        /// `_IOWR('i', number, struct in6_ifreq)`.
        const fn iowr_in6_ifreq(number: libc::c_ulong) -> libc::c_ulong {
            const IOC_INOUT: libc::c_ulong = 0xC000_0000;
            const IOCPARM_MASK: libc::c_ulong = 0x1fff;
            IOC_INOUT | ((std::mem::size_of::<libc::in6_ifreq>() as libc::c_ulong & IOCPARM_MASK) << 16) | ((b'i' as libc::c_ulong) << 8) | number
        }
        const SIOCGIFAFLAG_IN6: libc::c_ulong = iowr_in6_ifreq(73);
        const SIOCGIFALIFETIME_IN6: libc::c_ulong = iowr_in6_ifreq(81);
        // <netinet6/in6_var.h>: 0xc1206949. If libc's struct ever disagrees with the kernel's, do not guess.
        const _: () = assert!(SIOCGIFAFLAG_IN6 == 0xC120_6949 && SIOCGIFALIFETIME_IN6 == 0xC120_6951);

        pub struct Query(libc::c_int);

        impl Query {
            pub fn open() -> Query {
                // SAFETY: plain socket(2); a failure (-1) makes every query answer "unknown".
                Query(unsafe { libc::socket(libc::AF_INET6, libc::SOCK_DGRAM, 0) })
            }

            pub fn of(&self, name: &CStr, address: &libc::sockaddr_in6) -> V6State {
                if self.0 < 0 {
                    return V6State::UNKNOWN;
                }
                let Some(flags) = self.request(name, address, SIOCGIFAFLAG_IN6).map(|r| {
                    // SAFETY: SIOCGIFAFLAG_IN6 fills the flags member of the union.
                    unsafe { r.ifr_ifru.ifru_flags6 }
                }) else {
                    return V6State::UNKNOWN;
                };
                let lifetime = self.request(name, address, SIOCGIFALIFETIME_IN6).map(|r| {
                    // SAFETY: SIOCGIFALIFETIME_IN6 fills the lifetime member of the union.
                    unsafe { r.ifr_ifru.ifru_lifetime.ia6t_pltime }
                });
                V6State {
                    temporary: flags & IN6_IFF_TEMPORARY != 0,
                    preferred: flags & (IN6_IFF_TENTATIVE | IN6_IFF_DUPLICATED | IN6_IFF_DETACHED | IN6_IFF_DEPRECATED) == 0,
                    preferred_lifetime_seconds: match lifetime {
                        Some(ND6_INFINITE_LIFETIME) | None => i64::MAX,
                        Some(seconds) => i64::from(seconds),
                    },
                }
            }

            fn request(&self, name: &CStr, address: &libc::sockaddr_in6, command: libc::c_ulong) -> Option<libc::in6_ifreq> {
                let bytes = name.to_bytes();
                // SAFETY: an all-zero in6_ifreq is a valid value (integers, arrays and a union of them).
                let mut request: libc::in6_ifreq = unsafe { std::mem::zeroed() };
                if bytes.len() >= request.ifr_name.len() {
                    return None;
                }
                for (to, from) in request.ifr_name.iter_mut().zip(bytes) {
                    *to = *from as libc::c_char;
                }
                request.ifr_ifru.ifru_addr = *address;
                // SAFETY: the socket is open, `request` is a properly sized in6_ifreq the kernel reads and
                // writes in place.
                let result = unsafe { libc::ioctl(self.0, command, &mut request) };
                (result == 0).then_some(request)
            }
        }

        impl Drop for Query {
            fn drop(&mut self) {
                if self.0 >= 0 {
                    // SAFETY: the descriptor is ours and closed exactly once.
                    unsafe { libc::close(self.0) };
                }
            }
        }
    }

    /// Linux and the others: `getifaddrs` does not carry the flags (they are a netlink attribute), so every
    /// address reads as stable and preferred, which is what the C# apps saw there too.
    #[cfg(not(target_os = "macos"))]
    mod v6_flags {
        use std::ffi::CStr;

        use super::V6State;

        pub struct Query;

        impl Query {
            pub fn open() -> Query {
                Query
            }

            pub fn of(&self, _name: &CStr, _address: &libc::sockaddr_in6) -> V6State {
                V6State::UNKNOWN
            }
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod platform {
    pub fn read() -> Vec<super::AddressCandidate> {
        Vec::new()
    }
}
