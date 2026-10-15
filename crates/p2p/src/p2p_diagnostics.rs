//! Plain-language reasons for a failed connection, based on who offers which address family.

use std::net::IpAddr;

use hdrcast_core::endpoint;

use crate::local_addresses;

pub const SIGNALING_DOWN: &str = "The signaling server cannot be reached - check this PC's internet connection";

/// Why the viewer did not get through. A public IPv4 candidate exists only when that side's UDP echo answered
/// (docs/design.md §8.7); without one on the host there was no IPv4 path to try at all. `host_ipv4_open` is the
/// host's "open4" (router forward + firewall rule): it tells whose side kept the IPv4 attempt out.
pub fn explain<A: AsRef<str>, B: AsRef<str>>(this_pc_has_ipv6: bool, this_pc_candidates: &[A], host_candidates: &[B], host_ipv4_open: Option<bool>) -> String {
    let host_v6 = has_ipv6(host_candidates);
    if this_pc_has_ipv6 && host_v6 {
        return "Both sides have IPv6 but the path is blocked - usually a firewall or VPN that drops all incoming UDP".to_string();
    }
    let ipv4 = if !has_public_ipv4(host_candidates) {
        "the host offered no public IPv4 address"
    } else if !has_public_ipv4(this_pc_candidates) {
        "the IPv4 attempt did not get through (this PC's public IPv4 address could not be found)"
    } else {
        match host_ipv4_open {
            Some(false) => {
                "the IPv4 attempt did not get through: the host's PC only takes IPv4 from routers that keep ports, \
                 so they should click Allow next to Firewall in HdrCast and turn UPnP on in their router"
            }
            Some(true) => {
                "the IPv4 attempt did not get through although the host takes IPv4 from any port: \
                 something on this side drops UDP (a VPN, proxy or game accelerator?)"
            }
            None => "the IPv4 attempt did not get through",
        }
    };
    if host_v6 {
        return format!("This PC has no IPv6 internet connection, and {ipv4}. Turn on IPv6 on your router or ask your ISP (check at test-ipv6.com)");
    }
    if this_pc_has_ipv6 {
        return format!("The host's PC has no IPv6 internet connection, and {ipv4}; they need to turn IPv6 on (check at test-ipv6.com)");
    }
    format!("Neither side has IPv6, and {ipv4}; at least one side needs IPv6 (check at test-ipv6.com)")
}

/// A global IPv6 candidate (loopback counts too, for tests).
pub fn has_ipv6<S: AsRef<str>>(candidates: &[S]) -> bool {
    any_address(candidates, |a| a.is_ipv6() && (local_addresses::is_global_unicast_v6(a) || a.is_loopback()))
}

/// An IPv4 candidate outside the private, shared (CGNAT), loopback and link-local ranges.
pub fn has_public_ipv4<S: AsRef<str>>(candidates: &[S]) -> bool {
    any_address(candidates, local_addresses::is_public_ipv4)
}

fn any_address<S: AsRef<str>>(candidates: &[S], test: impl Fn(&IpAddr) -> bool) -> bool {
    candidates.iter().any(|candidate| endpoint::parse(candidate.as_ref()).ok().and_then(|hp| hp.ip()).is_some_and(|ip| test(&ip)))
}
