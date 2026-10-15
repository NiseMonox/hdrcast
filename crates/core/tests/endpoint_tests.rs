//! Port of tests/EndpointTests.cs: EndpointTests, SrtUrlIpv6Tests, InviteRedactionTests, ScriptHygieneTests.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use hdrcast_core::endpoint::{self, EndpointKind};
use hdrcast_core::secret_redactor;
use hdrcast_core::srt_url::{self, SrtRole, SrtTopology, SrtUrlOptions, SrtUrlParseError};

// ---- EndpointTests ------------------------------------------------------------------------------

#[test]
fn accepts_what_people_type() {
    use EndpointKind::{Hostname, IPv4, IPv6};
    let cases: [(&str, &str, Option<u16>, EndpointKind); 19] = [
        ("relay.example.net", "relay.example.net", None, Hostname),
        ("relay.example.net:9000", "relay.example.net", Some(9000), Hostname),
        ("192.168.1.201", "192.168.1.201", None, IPv4),
        ("192.168.1.201:8890", "192.168.1.201", Some(8890), IPv4),
        ("2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566", "2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566", None, IPv6),
        ("[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]", "2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566", None, IPv6),
        ("[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:8890", "2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566", Some(8890), IPv6),
        ("2001:DB8:0:0:0:0:0:1", "2001:db8::1", None, IPv6), // canonical compressed form
        ("::1", "::1", None, IPv6),
        ("[::1]:5000", "::1", Some(5000), IPv6),
        ("::ffff:192.0.2.7", "192.0.2.7", None, IPv4), // IPv4-mapped → IPv4
        ("  \"[2001:db8::1]:8890\"  ", "2001:db8::1", Some(8890), IPv6), // quotes and spaces
        ("「2001:db8::1」", "2001:db8::1", None, IPv6),
        ("srt://[2001:db8::1]:8890?streamid=read:screen", "2001:db8::1", Some(8890), IPv6),
        ("srt://relay.example.net:8890?latency=1500000", "relay.example.net", Some(8890), Hostname),
        ("１９２．１６８．１．２０１：８８９０", "192.168.1.201", Some(8890), IPv4), // typed with a CJK IME
        ("192。168。1。201", "192.168.1.201", None, IPv4),
        ("［2001:db8::1］：８８９０", "2001:db8::1", Some(8890), IPv6),
        ("relay.example.net.", "relay.example.net", None, Hostname),
    ];
    for (input, host, port, kind) in cases {
        let hp = endpoint::parse(input).unwrap_or_else(|e| panic!("{input:?}: {e}"));
        assert_eq!(hp.host(), host, "{input:?}");
        assert_eq!(hp.port(), port, "{input:?}");
        assert_eq!(hp.kind(), kind, "{input:?}");
    }
}

#[test]
fn rejects_what_cannot_work_with_a_readable_reason() {
    let cases = [
        ("", "empty"),
        ("   ", "empty"),
        ("fe80::1", "link-local"),
        ("fe80::1%12", "zone"),
        ("[fe80::1%eth0]:8890", "zone"),
        ("ff02::1", "multicast"),
        ("::", "connect"),
        ("0.0.0.0", "connect"),
        ("[2001:db8::1", "closing"),
        ("[2001:db8::1]8890", "port"),
        ("[192.168.1.1]:8890", "brackets"),
        ("192.168.1", "IPv4"),
        ("192.168.1.256", "IPv4"),
        ("192.168.01.1", "IPv4"),
        ("relay:0", "port"),
        ("relay:70000", "port"),
        ("relay:abc", "port"),
        ("under_score.example", "host name"),
        ("2001:db8::g", "IPv6"),
    ];
    for (input, reason_fragment) in cases {
        let error = endpoint::parse(input).expect_err(input).to_string();
        assert!(
            error.to_lowercase().contains(&reason_fragment.to_lowercase()),
            "{input:?}: {error:?} does not mention {reason_fragment:?}"
        );
    }
}

#[test]
fn unspecified_addresses_are_allowed_only_when_asked() {
    let a = endpoint::parse_with("::", true).unwrap();
    assert_eq!(a.host(), "::");
    assert!(endpoint::parse_with("0.0.0.0", true).is_ok());
}

#[test]
fn a_bare_ipv6_address_never_loses_its_last_group_to_a_port() {
    // "2400::1:8890" is the IPv6 address 2400::1:8890, not host 2400::1 port 8890; a port needs brackets.
    let hp = endpoint::parse("2400::1:8890").unwrap();
    assert_eq!(hp.host(), "2400::1:8890");
    assert_eq!(hp.port(), None);
}

#[test]
fn formatting_brackets_ipv6_only() {
    assert_eq!(endpoint::format_host("2001:db8::1"), "[2001:db8::1]");
    assert_eq!(endpoint::format_host("[2001:db8::1]"), "[2001:db8::1]");
    assert_eq!(endpoint::format_host("1.2.3.4"), "1.2.3.4");
    assert_eq!(endpoint::parse("2001:db8::1").unwrap().to_authority(8890), "[2001:db8::1]:8890");
    assert_eq!(endpoint::parse("relay:9000").unwrap().to_authority(8890), "relay:9000");
    let v6: SocketAddr = "[2001:db8::1]:8890".parse().unwrap();
    assert_eq!(endpoint::format(&v6), "[2001:db8::1]:8890");
    let mapped: SocketAddr = "[::ffff:192.0.2.7]:40".parse().unwrap();
    assert_eq!(endpoint::format(&mapped), "192.0.2.7:40");
}

// ---- SrtUrlIpv6Tests ----------------------------------------------------------------------------

fn caller(host: &str, port: u16) -> SrtUrlOptions {
    SrtUrlOptions {
        topology: SrtTopology::Caller,
        role: SrtRole::Read,
        host: host.to_string(),
        port,
        latency_ms: 1500,
        ..SrtUrlOptions::default()
    }
}

#[test]
fn every_way_of_typing_the_host_gives_a_valid_url() {
    let cases = [
        ("2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566", "srt://[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:8890?"),
        ("[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]", "srt://[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:8890?"),
        ("[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:8890", "srt://[2400:aaaa:bbbb:cccc:1122:33ff:fe44:5566]:8890?"),
        ("192.168.1.201:8890", "srt://192.168.1.201:8890?"),
        ("relay.example.net:8890", "srt://relay.example.net:8890?"),
    ];
    for (host, expected_start) in cases {
        let url = srt_url::build(&caller(host, 8890)).unwrap();
        assert!(url.starts_with(expected_start), "{host}: {url}");
        assert_eq!(srt_url::parse(&url).unwrap().port, 8890);
    }
}

#[test]
fn a_port_in_the_host_that_disagrees_with_the_port_box_is_an_error() {
    assert!(srt_url::build(&caller("[2001:db8::1]:9000", 8890)).is_err());
    let error = srt_url::caller_host_for("[2001:db8::1]:9000", 8890).unwrap_err();
    assert!(error.to_string().contains("9000"), "{error}");
}

#[test]
fn listener_binds_ipv4_by_default_and_a_concrete_ipv6_address_when_given() {
    let mut o =
        SrtUrlOptions { topology: SrtTopology::Listener, port: 9000, latency_ms: 1500, ..SrtUrlOptions::default() };
    assert!(srt_url::build(&o).unwrap().starts_with("srt://0.0.0.0:9000?"));
    o.listen_address = Some("2001:db8::5".to_string());
    assert!(srt_url::build(&o).unwrap().starts_with("srt://[2001:db8::5]:9000?"));
    o.listen_address = Some("127.0.0.1".to_string());
    assert!(srt_url::build(&o).unwrap().starts_with("srt://127.0.0.1:9000?"));
}

#[test]
fn listener_refuses_the_ipv6_wildcard_with_an_explanation() {
    // Verified with ffmpeg 7.1.5 + libsrt 1.5.4: "Operation not supported: Bad parameters".
    for address in ["::", "[::]"] {
        let error = srt_url::listen_host_for(Some(address)).unwrap_err();
        assert!(error.to_string().contains("::"), "{error}");
        let o = SrtUrlOptions {
            topology: SrtTopology::Listener,
            port: 9000,
            latency_ms: 1500,
            listen_address: Some(address.to_string()),
            ..SrtUrlOptions::default()
        };
        assert!(srt_url::build(&o).is_err());
    }
}

#[test]
fn listener_address_must_be_a_literal_without_port() {
    assert!(srt_url::listen_host_for(Some("my-pc.local")).is_err());
    assert!(srt_url::listen_host_for(Some("[2001:db8::5]:9000")).is_err());
}

#[test]
fn parse_insists_on_brackets_for_ipv6() {
    assert_eq!(srt_url::parse("srt://2001:db8::1:8890?mode=caller"), Err(SrtUrlParseError::Ipv6NeedsBrackets));
    let p = srt_url::parse("srt://[2001:db8::1]:8890?mode=caller").unwrap();
    assert_eq!(p.host, "2001:db8::1");
    assert_eq!(p.port, 8890);
}

// ---- InviteRedactionTests -----------------------------------------------------------------------

#[test]
fn invite_codes_never_reach_a_log() {
    const CODE: &str = "hdrcast:eyJ2IjoxLCJzIjoiaHR0cDovLzEwMS40My4yMy45NDoxODY1MCJ9.TaKdIhZQ";
    let line = format!("[app] imported {CODE} from the clipboard");
    let safe = secret_redactor::redact(&line);
    assert!(!safe.contains("eyJ2"));
    assert!(safe.contains(&format!("hdrcast:{} from the clipboard", secret_redactor::MASK)));
    assert_eq!(secret_redactor::redact("HDRCAST:abc.def"), format!("HDRCAST:{}", secret_redactor::MASK));
    assert_eq!(secret_redactor::redact("see hdrcast: docs"), "see hdrcast: docs");
}

// ---- ScriptHygieneTests -------------------------------------------------------------------------

/// The repository root (where build.sh is), or `None` when the crate is built from somewhere else.
fn repo_root() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().find(|dir| dir.join("build.sh").is_file()).map(Path::to_path_buf)
}

fn windows_scripts(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root.join("scripts")) else { return Vec::new() };
    entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ["cmd", "bat", "ps1"].iter().any(|known| ext.eq_ignore_ascii_case(known)))
        })
        .collect()
}

/// cmd.exe seeks to :labels; in an LF file a label straddling a 512-byte block can be missed.
#[test]
fn windows_scripts_use_crlf_only() {
    let Some(root) = repo_root() else { return };
    for file in windows_scripts(&root) {
        let bytes = std::fs::read(&file).unwrap();
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                assert!(i > 0 && bytes[i - 1] == b'\r', "{} has a bare LF at byte {i}", file.display());
            }
        }
    }
}

/// Windows PowerShell 5.1 reads a BOM-less script in the ANSI code page: a UTF-8 em dash becomes a quote.
#[test]
fn windows_scripts_are_pure_ascii() {
    let Some(root) = repo_root() else { return };
    for file in windows_scripts(&root) {
        let bytes = std::fs::read(&file).unwrap();
        if let Some(at) = bytes.iter().position(|&b| b >= 0x80) {
            panic!("{} has a non-ASCII byte at {at}", file.display());
        }
    }
}

#[test]
fn gitattributes_pins_crlf_for_windows_scripts() {
    let Some(root) = repo_root() else { return };
    let text = std::fs::read_to_string(root.join(".gitattributes")).unwrap();
    for pattern in ["*.cmd", "*.ps1"] {
        // a line "<pattern><whitespace>… eol=crlf"
        let pinned = text.lines().any(|line| {
            line.strip_prefix(pattern)
                .is_some_and(|rest| rest.starts_with(char::is_whitespace) && rest.contains("eol=crlf"))
        });
        assert!(pinned, ".gitattributes does not pin {pattern} to CRLF");
    }
}
