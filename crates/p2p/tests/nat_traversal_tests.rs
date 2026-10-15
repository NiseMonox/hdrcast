//! Port of tests/NatTraversalTests.cs: UpnpMessageTests, RouterPortMappingTests (against the fake router of
//! tests/common), SymmetricNatTests and the P2P half of OpenIpv4FlagTests. (WindowsFirewallTests and the
//! live-record half of OpenIpv4FlagTests are about hdrcast-core and live in its tests.)
//!
//! Nothing here touches the real router: every mapping is pointed at the stand-in on loopback.

mod common;

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hdrcast_core::group::keys;
use hdrcast_p2p::control_packet::{self, ControlType, PeerRole};
use hdrcast_p2p::peer_session::{MAX_REMOTE_CANDIDATES, PUNCH_DEADLINE_US};
use hdrcast_p2p::upnp::{self, PortMappingOptions, PortMappingState, RouterPortMapping, UpnpGateway};
use hdrcast_p2p::{local_addresses, p2p_diagnostics, CancelToken, LinkState, LogFn, PeerSession};

use common::{vectors, wait_for, FakeRouter};

fn quiet() -> LogFn {
    Arc::new(|_| {})
}

fn collecting() -> (LogFn, Arc<Mutex<Vec<String>>>) {
    let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = lines.clone();
    (Arc::new(move |line: &str| sink.lock().unwrap().push(line.to_string())), lines)
}

// ---- UpnpMessageTests -------------------------------------------------------------------------------

#[test]
fn an_ssdp_answer_gives_the_description_url_and_anything_else_is_ignored() {
    let url = upnp::parse_search_response(
        "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nlocation: http://192.168.1.1:2869/upnp/790a830.xml\r\nST: upnp:rootdevice\r\n\r\n",
    );
    assert_eq!(url.as_deref(), Some("http://192.168.1.1:2869/upnp/790a830.xml"));
    assert_eq!(upnp::parse_search_response("NOTIFY * HTTP/1.1\r\nLOCATION: http://192.168.1.1/x.xml\r\n\r\n"), None);
    assert_eq!(upnp::parse_search_response("HTTP/1.1 200 OK\r\nST: upnp:rootdevice\r\n\r\n"), None);
    assert_eq!(upnp::parse_search_response("HTTP/1.1 200 OK\r\nLOCATION: file:///etc/passwd\r\n\r\n"), None);
    assert_eq!(upnp::parse_search_response("HTTP/1.1 200 OK\r\nLOCATION: https://192.168.1.1/x.xml\r\n\r\n"), None, "http only");
    assert_eq!(upnp::parse_search_response(""), None);
    assert_eq!(upnp::parse_search_response("HTTP/1.1 404 Not Found\r\nLOCATION: http://192.168.1.1/x.xml\r\n\r\n"), None);
}

const DESCRIPTION: &str = concat!(
    "<?xml version=\"1.0\"?><root xmlns=\"urn:schemas-upnp-org:device-1-0\"><device><modelName>XG-100NE</modelName><serviceList>",
    "<service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType><controlURL>/l3f</controlURL></service>",
    "<service><serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType><controlURL>/upnp/control/0a1b2c3d</controlURL></service>",
    "<service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:2</serviceType><controlURL>http://192.168.1.1:5000/ip</controlURL></service>",
    "</serviceList></device></root>"
);

#[test]
fn the_wan_connection_services_come_out_of_the_description_with_absolute_control_urls() {
    let list = upnp::parse_description(DESCRIPTION, "http://192.168.1.1:2869/upnp/790a830.xml").unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].control_url, "http://192.168.1.1:2869/upnp/control/0a1b2c3d");
    assert_eq!(list[0].service_type, "urn:schemas-upnp-org:service:WANPPPConnection:1");
    assert_eq!(list[0].name, "XG-100NE");
    assert_eq!(list[1].control_url, "http://192.168.1.1:5000/ip");

    // URLBase, when present, is what relative control URLs hang off
    let with_base = DESCRIPTION.replace("<device>", "<URLBase>http://192.168.1.254:80/</URLBase><device>");
    assert_eq!(upnp::parse_description(&with_base, "http://192.168.1.1:2869/d.xml").unwrap()[0].control_url, "http://192.168.1.254/upnp/control/0a1b2c3d");

    // without a model name the friendly name, then the address, names the router
    let friendly = DESCRIPTION.replace("<modelName>XG-100NE</modelName>", "<friendlyName> Home Gateway </friendlyName>");
    assert_eq!(upnp::parse_description(&friendly, "http://192.168.1.1:2869/d.xml").unwrap()[0].name, "Home Gateway");
    let nameless = DESCRIPTION.replace("<modelName>XG-100NE</modelName>", "");
    assert_eq!(upnp::parse_description(&nameless, "http://192.168.1.1:2869/d.xml").unwrap()[0].name, "192.168.1.1");
    assert!(upnp::parse_description("<root><device/></root>", "http://192.168.1.1/d.xml").unwrap().is_empty());
}

#[test]
fn a_description_with_a_dtd_is_read_without_resolving_it() {
    // any device on the LAN can answer the search: its DTD and external entities are never followed
    let body = "<root><device><modelName>M</modelName><serviceList><service><serviceType>\
                urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>/c</controlURL></service></serviceList></device></root>";
    let dtd = "<?xml version=\"1.0\"?><!DOCTYPE root [<!ENTITY x SYSTEM \"file:///c:/windows/win.ini\">]>";
    assert_eq!(upnp::parse_description(&format!("{dtd}{body}"), "http://10.0.0.1/d.xml").unwrap().len(), 1);
    let error = upnp::parse_description(&format!("{dtd}{}", body.replace(">M<", ">&x;<")), "http://10.0.0.1/d.xml").unwrap_err();
    assert_eq!(error.message, "the router's description is not valid XML");
    assert_eq!(error.code, 0);
}

#[test]
fn requests_are_escaped_and_answers_and_faults_are_read() {
    let envelope = upnp::build_envelope("urn:schemas-upnp-org:service:WANIPConnection:1", "AddPortMapping", &[("NewPortMappingDescription", "A & <B>".to_string())]);
    assert!(envelope.contains("<u:AddPortMapping xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\">"), "{envelope}");
    assert!(envelope.contains("<NewPortMappingDescription>A &amp; &lt;B&gt;</NewPortMappingDescription>"), "{envelope}");
    assert_eq!(
        upnp::build_envelope("t", "GetExternalIPAddress", &[]),
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:GetExternalIPAddress xmlns:u=\"t\">\
         </u:GetExternalIPAddress></s:Body></s:Envelope>"
    );

    let answer = upnp::parse_response(
        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:GetExternalIPAddressResponse xmlns:u=\"x\">\
         <NewExternalIPAddress>203.0.113.65</NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>",
        "GetExternalIPAddress",
    )
    .unwrap();
    assert_eq!(answer["NewExternalIPAddress"], "203.0.113.65");
    let missing = upnp::parse_response("<a/>", "GetExternalIPAddress").unwrap_err();
    assert_eq!(missing.message, "GetExternalIPAddress: the router's answer has no GetExternalIPAddressResponse");
    assert_eq!(upnp::parse_response("nope", "AddPortMapping").unwrap_err().message, "AddPortMapping: the router sent something that is not SOAP");

    let fault = upnp::parse_fault(
        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><s:Fault><detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
         <errorCode>718</errorCode><errorDescription>ConflictInMappingEntry</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>",
        500,
        "AddPortMapping",
    );
    assert_eq!(fault.code, 718);
    assert_eq!(fault.message, "AddPortMapping refused by the router (error 718 ConflictInMappingEntry)");
    let not_xml = upnp::parse_fault("not xml", 500, "AddPortMapping");
    assert_eq!((not_xml.code, not_xml.message.as_str()), (0, "AddPortMapping refused by the router (HTTP 500)"));
    assert_eq!(upnp::parse_fault("<e><errorCode> 606 </errorCode></e>", 500, "X").message, "X refused by the router (error 606)");
}

/// Not in the C# tests as such: the client functions one by one against the router stand-in.
#[test]
fn the_client_finds_the_router_reads_it_and_calls_its_actions() {
    let router = FakeRouter::new();
    let found = upnp::search(Ipv4Addr::LOCALHOST, router.ssdp_addr(), 1500, &CancelToken::new()).unwrap();
    assert_eq!(found, [router.description_url()], "ten searches, one router, one location");
    assert_eq!(router.searches(), 10, "both rounds of the five search targets");

    let gateways = upnp::read_description(&found[0], 2000).unwrap();
    assert_eq!(gateways.len(), 1);
    let gateway: &UpnpGateway = &gateways[0];
    assert_eq!(gateway.service_type, common::FAKE_SERVICE_TYPE);
    assert_eq!(gateway.name, "FR-100");
    assert!(gateway.control_url.ends_with("/ctl/wan"), "{}", gateway.control_url);

    assert_eq!(upnp::get_external_address(gateway, 2000).unwrap(), Some(Ipv4Addr::new(203, 0, 113, 7)));
    upnp::add_port_mapping(gateway, 40123, Ipv4Addr::new(192, 168, 1, 3), 50331, "HdrCast", 3600, 2000).unwrap();
    assert_eq!(router.mapping(40123).as_deref(), Some("192.168.1.3:50331 3600"));
    upnp::delete_port_mapping(gateway, 40123, 2000).unwrap();
    assert!(router.mappings().is_empty());
    let gone = upnp::delete_port_mapping(gateway, 40123, 2000).unwrap_err();
    assert_eq!((gone.code, gone.message.as_str()), (714, "DeletePortMapping refused by the router (error 714 NoSuchEntryInArray)"));
    let unknown = upnp::call(gateway, "ForceTermination", &[], 2000).unwrap_err();
    assert_eq!(unknown.code, 401);

    // a description that is not there, and a router that is not there
    let missing = upnp::read_description(&found[0].replace("desc.xml", "nope.xml"), 2000).unwrap_err();
    assert_eq!(missing.message, "the router's description could not be read (ProtocolError)");
    let nobody = UpnpGateway { control_url: format!("http://127.0.0.1:{}/ctl/wan", common::free_tcp_port()), ..gateway.clone() };
    let refused = upnp::get_external_address(&nobody, 2000).unwrap_err();
    assert!(refused.message.starts_with("GetExternalIPAddress: the router did not answer ("), "{}", refused.message);
    assert_eq!(upnp::local_address_towards(Ipv4Addr::LOCALHOST), Some(Ipv4Addr::LOCALHOST));

    // a cancelled search returns at once with whatever it has
    let cancelled = CancelToken::new();
    cancelled.cancel();
    let started = Instant::now();
    assert!(upnp::search(Ipv4Addr::LOCALHOST, router.ssdp_addr(), 5000, &cancelled).unwrap().is_empty());
    assert!(started.elapsed() < Duration::from_secs(1));
}

// ---- RouterPortMappingTests -------------------------------------------------------------------------

fn external(ip: &str, port: u16) -> Option<SocketAddrV4> {
    Some(SocketAddrV4::new(ip.parse().unwrap(), port))
}

#[test]
fn forwards_the_port_reports_the_internet_address_and_removes_it_on_dispose() {
    let router = FakeRouter::new();
    let (log, lines) = collecting();
    let changes = Arc::new(AtomicUsize::new(0));
    let m = RouterPortMapping::new(50331, Some(router.options()), log);
    let counter = changes.clone();
    m.on_changed(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(m.state(), PortMappingState::Off);
    m.start();
    assert!(wait_for(|| m.state() == PortMappingState::Mapped, 8000), "{:?} {}", m.state(), m.problem());
    assert!(m.open());
    assert_eq!(m.external(), external("203.0.113.7", 50331));
    assert_eq!(m.router_name(), "FR-100");
    assert_eq!(m.port(), 50331);
    assert!(!m.behind_another_nat());
    assert!(changes.load(Ordering::SeqCst) >= 1);
    assert_eq!(router.mapping(50331).as_deref(), Some("127.0.0.1:50331 3600"));
    assert_eq!(
        lines.lock().unwrap().as_slice(),
        ["[p2p] router: UDP 50331 forwarded to this PC (FR-100, internet address 203.0.113.7:50331); renewed while live, removed when HdrCast stops"]
    );

    m.dispose();
    assert!(router.mappings().is_empty());
    assert_eq!(router.count("DeletePortMapping"), 1);
    assert_eq!(m.state(), PortMappingState::Off);
    assert_eq!(lines.lock().unwrap().last().map(String::as_str), Some("[p2p] router: forward of UDP 50331 removed"));
    m.dispose(); // once is enough
    assert_eq!(router.count("DeletePortMapping"), 1);
}

#[test]
fn the_lease_is_renewed_while_it_lives() {
    let router = FakeRouter::new();
    let m = RouterPortMapping::new(40000, Some(router.options_renewing(1)), quiet());
    m.start();
    assert!(wait_for(|| router.count("AddPortMapping") >= 3, 8000), "renewals: {}", router.count("AddPortMapping"));
    assert_eq!(m.state(), PortMappingState::Mapped);
    assert_eq!(router.searches(), 10, "renewing does not search again");
    assert!(router.calls().iter().filter(|c| c.starts_with("AddPortMapping ")).all(|c| c == "AddPortMapping 40000->127.0.0.1:40000 lease 3600"), "{:?}", router.calls());
}

#[test]
fn a_port_taken_by_another_device_is_forwarded_from_another_outside_port() {
    let router = FakeRouter::new();
    router.refuse_add(|args| (args["NewExternalPort"] == "50331").then(|| (718, "ConflictInMappingEntry".to_string())));
    let m = RouterPortMapping::new(50331, Some(router.options()), quiet());
    m.start();
    assert!(wait_for(|| m.state() == PortMappingState::Mapped, 8000), "{:?} {}", m.state(), m.problem());
    let outside = m.external().unwrap().port();
    assert_ne!(outside, 50331);
    assert!((20000..60000).contains(&outside), "{outside}");
    assert_eq!(router.mapping(outside).as_deref(), Some("127.0.0.1:50331 3600"));
}

#[test]
fn an_old_router_that_only_takes_permanent_forwards_gets_one() {
    let router = FakeRouter::new();
    router.refuse_add(|args| (args["NewLeaseDuration"] != "0").then(|| (725, "OnlyPermanentLeasesSupported".to_string())));
    {
        let m = RouterPortMapping::new(45000, Some(router.options()), quiet());
        m.start();
        assert!(wait_for(|| m.state() == PortMappingState::Mapped, 8000), "{:?} {}", m.state(), m.problem());
        assert_eq!(router.mapping(45000).as_deref(), Some("127.0.0.1:45000 0"));
    }
    assert!(router.mappings().is_empty(), "a permanent forward must not outlive us");
}

#[test]
fn a_refusal_is_reported_with_the_routers_reason() {
    let router = FakeRouter::new();
    router.refuse_add(|_| Some((606, "Action not authorized".to_string())));
    let (log, lines) = collecting();
    let m = RouterPortMapping::new(45001, Some(router.options()), log);
    m.start();
    assert!(wait_for(|| m.state() == PortMappingState::Refused, 8000), "{:?}", m.state());
    assert!(!m.open());
    assert_eq!(m.problem(), "AddPortMapping refused by the router (error 606 Action not authorized)");
    assert_eq!(m.external(), None);
    // it tries again (after a second here), and says the same thing only once
    assert!(wait_for(|| router.count("AddPortMapping") >= 2, 8000), "no retry");
    assert_eq!(
        lines.lock().unwrap().as_slice(),
        ["[p2p] router: could not forward UDP 45001 to this PC - AddPortMapping refused by the router (error 606 Action not authorized). \
          Friends whose routers change ports cannot reach this PC over IPv4 (docs/network.md §1.1)"]
    );
}

#[test]
fn a_router_behind_another_nat_is_mapped_but_not_open() {
    let router = FakeRouter::new();
    router.set_external_ip("100.64.1.2");
    let (log, lines) = collecting();
    let m = RouterPortMapping::new(45002, Some(router.options()), log);
    m.start();
    assert!(wait_for(|| m.state() == PortMappingState::Mapped, 8000), "{:?} {}", m.state(), m.problem());
    assert!(m.behind_another_nat());
    assert!(!m.open());
    assert_eq!(
        lines.lock().unwrap().as_slice(),
        ["[p2p] router: UDP 45002 is forwarded on FR-100, but that router's own internet address 100.64.1.2 is not public \
          (the provider or another router adds a second NAT), so it cannot make this PC reachable"]
    );
}

#[test]
fn no_router_and_a_public_address_on_this_pc_are_told_apart() {
    let silent = common::free_udp_port();
    let none = PortMappingOptions {
        ssdp_target: SocketAddrV4::new(Ipv4Addr::LOCALHOST, silent),
        bind: Some(Ipv4Addr::LOCALHOST),
        search_timeout_ms: 400,
        retry_seconds: 60,
        ..PortMappingOptions::default()
    };
    let m = RouterPortMapping::new(45003, Some(none), quiet());
    m.start();
    assert!(wait_for(|| m.state() == PortMappingState::NoRouter, 5000), "{:?}", m.state());
    assert_eq!(m.problem(), "no UPnP router answered (UPnP may be off in the router's settings)");
    assert!(!m.open());
    let started = Instant::now();
    drop(m); // it is waiting a minute for its retry: stopping must not
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());

    let (log, lines) = collecting();
    let direct = PortMappingOptions { bind: Some("198.51.100.20".parse().unwrap()), ..PortMappingOptions::default() };
    let m = RouterPortMapping::new(45004, Some(direct), log);
    m.start();
    assert!(wait_for(|| m.state() == PortMappingState::Direct, 5000), "{:?}", m.state());
    assert!(m.open());
    assert_eq!(m.external(), external("198.51.100.20", 45004));
    assert_eq!(lines.lock().unwrap().as_slice(), ["[p2p] router: this PC has the public IPv4 address 198.51.100.20 itself, no router to open a port on"]);
}

// ---- SymmetricNatTests ------------------------------------------------------------------------------
// Hole punching through simulated routers. The symmetric one gives every destination the next port, like the
// China Telecom router that advertised :20833 (what the echo saw) and reached the host from :20834.

fn echo_server() -> SocketAddr {
    // only an address inside the simulation: nothing is ever sent to it
    "101.43.23.94:18651".parse().unwrap()
}

struct Router {
    public: IpAddr,
    base_port: u16,
    symmetric: bool,
    /// Router forward + firewall rule: anything addressed to the base port gets in.
    open: bool,
    ports: HashMap<SocketAddr, u16>,
    pinholes: HashSet<(u16, SocketAddr)>,
    next: u16,
    dropped: usize,
}

impl Router {
    fn new(public_ip: &str, base_port: u16, symmetric: bool, open: bool) -> Router {
        let mut router =
            Router { public: public_ip.parse().unwrap(), base_port, symmetric, open, ports: HashMap::new(), pinholes: HashSet::new(), next: 1, dropped: 0 };
        router.send(echo_server()); // the echo saw the base port: that is what gets advertised
        router
    }

    fn advertised(&self) -> String {
        format!("{}:{}", self.public, self.base_port)
    }

    fn owns(&self, to: SocketAddr) -> bool {
        to.ip() == self.public
    }

    /// The public source of a packet this side sends to `to`; opens its pinhole.
    fn send(&mut self, to: SocketAddr) -> SocketAddr {
        let mut port = self.base_port;
        if self.symmetric {
            port = match self.ports.get(&to) {
                Some(port) => *port,
                None => {
                    let port = if self.ports.is_empty() {
                        self.base_port
                    } else {
                        let port = self.base_port + self.next;
                        self.next += 1;
                        port
                    };
                    self.ports.insert(to, port);
                    port
                }
            };
        }
        self.pinholes.insert((port, to));
        SocketAddr::new(self.public, port)
    }

    fn admits(&mut self, from: SocketAddr, to: SocketAddr) -> bool {
        let ok = self.pinholes.contains(&(to.port(), from)) || (self.open && to.port() == self.base_port);
        if !ok {
            self.dropped += 1;
        }
        ok
    }
}

fn nat_pair(host_router: &Router, viewer_router: &Router) -> (PeerSession, PeerSession) {
    let keys = vectors::keys();
    let sid = keys::new_id();
    let host = PeerSession::new(&keys, &sid, PeerRole::Host, 0).unwrap();
    let viewer = PeerSession::new(&keys, &sid, PeerRole::Viewer, 0).unwrap();
    host.add_remote_candidates(&["192.168.51.108:62909".to_string(), viewer_router.advertised()]);
    viewer.add_remote_candidates(&["192.168.1.3:50331".to_string(), host_router.advertised()]);
    (host, viewer)
}

fn nat_run(host: &PeerSession, host_router: &mut Router, viewer: &PeerSession, viewer_router: &mut Router, duration_us: i64) {
    let mut in_flight: Vec<([u8; control_packet::LENGTH], SocketAddr, SocketAddr)> = Vec::new();
    let mut now = 0;
    while now < duration_us {
        // the other network's LAN candidate never leaves (and gets no NAT mapping): the real router gave the
        // first packet to the host's public address the port right after the advertised one
        for o in host.tick(now) {
            if viewer_router.owns(o.to) {
                in_flight.push((o.data, host_router.send(o.to), o.to));
            }
        }
        for o in viewer.tick(now) {
            if host_router.owns(o.to) {
                in_flight.push((o.data, viewer_router.send(o.to), o.to));
            }
        }
        for (data, from, to) in std::mem::take(&mut in_flight) {
            let to_host = host_router.owns(to);
            let (router, session) = if to_host { (&mut *host_router, host) } else { (&mut *viewer_router, viewer) };
            if !router.admits(from, to) {
                continue;
            }
            for reply in session.on_control(&data, from, now) {
                in_flight.push((reply.data, router.send(reply.to), reply.to));
            }
        }
        if (host.is_over() && viewer.is_over()) || (host.state() == LinkState::Connected && viewer.state() == LinkState::Connected) {
            break;
        }
        now += 10_000;
    }
}

#[test]
fn a_viewer_behind_a_symmetric_nat_reaches_a_host_that_takes_ipv4_from_any_port() {
    let mut host_router = Router::new("203.0.113.65", 50331, false, true);
    let mut viewer_router = Router::new("198.51.100.3", 20833, true, false);
    let (host, viewer) = nat_pair(&host_router, &viewer_router);
    nat_run(&host, &mut host_router, &viewer, &mut viewer_router, 5_000_000);

    assert_eq!(viewer.state(), LinkState::Connected);
    assert_eq!(host.state(), LinkState::Connected); // on a PUNCH_ACK now, not only once SRT flows
    let learned: SocketAddr = "198.51.100.3:20834".parse().unwrap();
    assert_eq!(host.selected(), Some(learned));
    assert!(!host.is_advertised(learned));
    assert!(host.remote_candidates().contains(&learned));
}

#[test]
fn nobody_gets_through_when_neither_side_takes_unsolicited_ipv4() {
    let mut host_router = Router::new("203.0.113.65", 50331, false, false);
    let mut viewer_router = Router::new("198.51.100.3", 20833, true, false);
    let (host, viewer) = nat_pair(&host_router, &viewer_router);
    nat_run(&host, &mut host_router, &viewer, &mut viewer_router, PUNCH_DEADLINE_US + 500_000);

    assert_eq!(host.state(), LinkState::Failed);
    assert_eq!(viewer.state(), LinkState::Failed);
    assert!(host.reason().contains("no candidate answered"), "{}", host.reason()); // what the host's log said 7 times on 2026-09-25
    assert!(host_router.dropped > 0 && viewer_router.dropped > 0);
}

#[test]
fn a_host_behind_a_symmetric_nat_reaches_a_viewer_that_takes_ipv4_from_any_port() {
    // the other direction: the friend in China hosts, and the viewer's side is the open one
    let mut host_router = Router::new("198.51.100.3", 20884, true, false);
    let mut viewer_router = Router::new("203.0.113.65", 50331, false, true);
    let (host, viewer) = nat_pair(&host_router, &viewer_router);
    nat_run(&host, &mut host_router, &viewer, &mut viewer_router, 5_000_000);

    assert_eq!(host.state(), LinkState::Connected);
    assert_eq!(viewer.state(), LinkState::Connected); // needs the learned address: 20884 never answers
    let learned: SocketAddr = "198.51.100.3:20885".parse().unwrap();
    assert_eq!(viewer.selected(), Some(learned));
    assert!(!viewer.is_advertised(learned));
}

#[test]
fn two_cone_routers_still_punch_through_without_help() {
    let mut host_router = Router::new("203.0.113.65", 50331, false, false);
    let mut viewer_router = Router::new("198.51.100.13", 48626, false, false);
    let (host, viewer) = nat_pair(&host_router, &viewer_router);
    nat_run(&host, &mut host_router, &viewer, &mut viewer_router, 5_000_000);
    assert_eq!(viewer.state(), LinkState::Connected);
    assert_eq!(host.state(), LinkState::Connected);
    assert!(host.is_advertised(host.selected().unwrap()));
}

#[test]
fn learned_addresses_are_capped_and_need_an_authentic_packet() {
    let keys = vectors::keys();
    let sid = keys::new_id();
    let host = PeerSession::new(&keys, &sid, PeerRole::Host, 0).unwrap();
    host.add_remote_candidates(&["203.0.113.1:1000"]);
    let sid_bytes = keys::id_bytes_of(&sid).unwrap();
    let punch = control_packet::build(ControlType::Punch, PeerRole::Viewer, &sid_bytes, 5, 0, &keys.punch_key(&sid));
    for i in 0..40u16 {
        host.on_control(&punch, SocketAddr::new("198.51.100.9".parse().unwrap(), 30000 + i), 1000 + i64::from(i));
    }
    assert_eq!(host.remote_candidates().len(), MAX_REMOTE_CANDIDATES);

    // HostLink / ViewerLink only hand over packets whose MAC checks out; a forged one never gets this far
    let forged = control_packet::build(ControlType::Punch, PeerRole::Viewer, &sid_bytes, 5, 0, &keys.punch_key(&keys::new_id()));
    assert!(!control_packet::verify_mac(&forged, &keys.punch_key(&sid)));
}

// ---- OpenIpv4FlagTests (the P2P half) ---------------------------------------------------------------

#[test]
fn the_viewer_is_told_whose_side_kept_the_ipv4_attempt_out() {
    let host = ["[2400:aaaa:bbbb:cccc:1234:5678:9abc:def0]:57608", "192.168.1.3:57609", "203.0.113.65:57609"];
    let me = ["192.168.51.108:62909", "198.51.100.3:19522"];
    let closed = p2p_diagnostics::explain(false, &me, &host, Some(false));
    assert!(closed.contains("This PC has no IPv6"), "{closed}");
    assert!(closed.contains("click Allow next to Firewall"), "{closed}");
    let open = p2p_diagnostics::explain(false, &me, &host, Some(true));
    assert!(open.contains("something on this side drops UDP"), "{open}");
    let unknown = p2p_diagnostics::explain(false, &me, &host, None);
    assert!(!unknown.contains("Allow"), "{unknown}");

    // the sentences as users have seen them since 0.2.4, word for word
    assert_eq!(
        closed,
        "This PC has no IPv6 internet connection, and the IPv4 attempt did not get through: the host's PC only takes IPv4 from routers that keep ports, \
         so they should click Allow next to Firewall in HdrCast and turn UPnP on in their router. Turn on IPv6 on your router or ask your ISP (check at test-ipv6.com)"
    );
    assert_eq!(
        open,
        "This PC has no IPv6 internet connection, and the IPv4 attempt did not get through although the host takes IPv4 from any port: \
         something on this side drops UDP (a VPN, proxy or game accelerator?). Turn on IPv6 on your router or ask your ISP (check at test-ipv6.com)"
    );
    assert_eq!(unknown, "This PC has no IPv6 internet connection, and the IPv4 attempt did not get through. Turn on IPv6 on your router or ask your ISP (check at test-ipv6.com)");
}

#[test]
fn public_ipv4_addresses_are_recognised() {
    for (address, public) in [
        ("203.0.113.65", true),
        ("100.64.1.2", false),
        ("192.168.1.1", false),
        ("0.0.0.0", false),
        ("239.255.255.250", false),
        ("2400::1", false),
        ("10.1.2.3", false),
        ("172.16.0.1", false),
        ("172.32.0.1", true),
        ("127.0.0.1", false),
        ("169.254.7.7", false),
        ("100.128.0.1", true),
    ] {
        assert_eq!(local_addresses::is_public_ipv4(&address.parse().unwrap()), public, "{address}");
    }
}

/// The helper tests above bind sockets; make sure nothing in this file can have reached a real router: every
/// mapping was created with explicit options whose search target is on loopback (or never searched at all).
#[test]
fn the_default_options_are_the_real_multicast_group_which_no_test_uses() {
    let defaults = PortMappingOptions::default();
    assert_eq!(defaults.ssdp_target, "239.255.255.250:1900".parse().unwrap());
    assert_eq!((defaults.search_timeout_ms, defaults.call_timeout_ms), (2500, 3000));
    assert_eq!((defaults.lease_seconds, defaults.renew_seconds, defaults.retry_seconds), (3600, 1200, 60));
    assert_eq!(defaults.description, "HdrCast");
    assert_eq!(defaults.bind, None);
    let router = FakeRouter::new();
    assert_eq!(*router.options().ssdp_target.ip(), Ipv4Addr::LOCALHOST);
    assert_eq!(router.options().bind, Some(Ipv4Addr::LOCALHOST));
    let _ = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
}
