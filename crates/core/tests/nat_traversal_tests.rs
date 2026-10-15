//! Port of the parts of tests/NatTraversalTests.cs that only need this crate: WindowsFirewallTests and the
//! live-record half of OpenIpv4FlagTests.

use std::path::Path;

use hdrcast_core::firewall::{self, FirewallRule, FirewallVerdict, PROFILE_ALL, PROTOCOL_ANY, PROTOCOL_UDP};
use hdrcast_core::group::keys;
use hdrcast_core::LiveRecord;

// ---- WindowsFirewallTests -----------------------------------------------------------------------

const EXE: &str = r"D:\HdrCast-0.2.4\HdrCastSender.exe";
const PRIVATE: i32 = 2;
const PUBLIC: i32 = 4;

/// An enabled inbound UDP allow rule for [`EXE`] on every network, any port, any address.
fn rule() -> FirewallRule {
    FirewallRule {
        name: "r".to_string(),
        application: EXE.to_string(),
        allow: true,
        protocol: PROTOCOL_UDP,
        profiles: PROFILE_ALL,
        enabled: true,
        inbound: true,
        local_ports: "*".to_string(),
        remote_addresses: "*".to_string(),
    }
}

fn eval(rules: &[FirewallRule]) -> FirewallVerdict {
    firewall::evaluate(EXE, rules, PRIVATE, 7, 0)
}

#[test]
fn only_an_enabled_inbound_udp_allow_rule_for_this_program_on_every_active_network_counts() {
    assert_eq!(eval(&[]), FirewallVerdict::NotAllowed);
    assert_eq!(eval(&[rule()]), FirewallVerdict::Allowed);
    assert_eq!(eval(&[FirewallRule { protocol: PROTOCOL_ANY, ..rule() }]), FirewallVerdict::Allowed);
    assert_eq!(
        eval(&[FirewallRule { application: r"d:\hdrcast-0.2.4\HDRCASTSENDER.EXE".into(), ..rule() }]),
        FirewallVerdict::Allowed
    );
    assert_eq!(eval(&[FirewallRule { protocol: 6, ..rule() }]), FirewallVerdict::NotAllowed); // TCP only
    assert_eq!(
        eval(&[FirewallRule { application: r"D:\HdrCast-0.2.2\HdrCastSender.exe".into(), ..rule() }]),
        FirewallVerdict::NotAllowed // an older copy
    );
    assert_eq!(eval(&[FirewallRule { enabled: false, ..rule() }]), FirewallVerdict::NotAllowed);
    assert_eq!(eval(&[FirewallRule { inbound: false, ..rule() }]), FirewallVerdict::NotAllowed);
    assert_eq!(eval(&[FirewallRule { local_ports: "5000".into(), ..rule() }]), FirewallVerdict::NotAllowed); // our port is random
    assert_eq!(
        eval(&[FirewallRule { remote_addresses: "LocalSubnet".into(), ..rule() }]),
        FirewallVerdict::NotAllowed // friends are not local
    );
    assert_eq!(eval(&[FirewallRule { application: String::new(), ..rule() }]), FirewallVerdict::NotAllowed); // not about this program
    assert_eq!(
        firewall::evaluate(EXE, &[FirewallRule { profiles: PRIVATE, ..rule() }], PRIVATE | PUBLIC, 7, 0),
        FirewallVerdict::NotAllowed
    );
    assert_eq!(
        firewall::evaluate(
            EXE,
            &[FirewallRule { profiles: PRIVATE, ..rule() }, FirewallRule { profiles: PUBLIC, ..rule() }],
            PRIVATE | PUBLIC,
            7,
            0
        ),
        FirewallVerdict::Allowed
    );
}

#[test]
fn a_block_rule_wins_and_the_firewall_being_off_or_blocking_everything_is_told_apart() {
    assert_eq!(eval(&[rule(), FirewallRule { allow: false, ..rule() }]), FirewallVerdict::Blocked);
    assert_eq!(
        eval(&[rule(), FirewallRule { allow: false, profiles: PUBLIC, ..rule() }]),
        FirewallVerdict::Allowed // a block rule for another network
    );
    assert_eq!(firewall::evaluate(EXE, &[], PRIVATE, PUBLIC, 0), FirewallVerdict::Off);
    assert_eq!(firewall::evaluate(EXE, &[rule()], PRIVATE, 7, PRIVATE), FirewallVerdict::BlockAll);
}

#[test]
fn rule_paths_with_environment_variables_match() {
    let Some(system) = std::env::var_os("SystemRoot").filter(|_| cfg!(windows)) else { return };
    let svchost = Path::new(&system).join("system32").join("svchost.exe");
    assert!(firewall::same_program(r"%SystemRoot%\system32\svchost.exe", &svchost.to_string_lossy()));
    assert!(!firewall::same_program("", EXE));
}

#[test]
fn the_live_settings_can_be_read_on_windows() {
    // read-only: never adds a rule (that needs the Allow button and Windows' UAC prompt)
    if !cfg!(windows) {
        return;
    }
    let v = firewall::check(&std::env::temp_dir().join("hdrcast-no-such-program.exe"));
    assert_ne!(v, FirewallVerdict::Unknown);
    assert_ne!(v, FirewallVerdict::Allowed);
    assert_ne!(v, FirewallVerdict::NotApplicable);

    // The rule list really is walked: every Windows has rules for svchost ("Core Networking" and others),
    // spelled %SystemRoot%\system32\svchost.exe. A first version read the COM enumerator wrongly and saw none,
    // which looked exactly like "no rule for HdrCast".
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let svchost = Path::new(&root).join("system32").join("svchost.exe");
    let rules = firewall::rules_for(&svchost).expect("the rule list is readable");
    assert!(!rules.is_empty());
    // and what was read makes sense: every rule names svchost and has a name; inbound and outbound both occur
    assert!(rules.iter().all(|r| firewall::same_program(&r.application, &svchost.to_string_lossy())));
    assert!(rules.iter().all(|r| !r.name.is_empty()));
    assert!(rules.iter().any(|r| r.inbound) && rules.iter().any(|r| !r.inbound));
    assert!(rules.iter().any(|r| r.protocol == PROTOCOL_UDP));
}

#[test]
fn the_check_works_from_any_kind_of_thread() {
    // The UI thread of a desktop app is a single-threaded COM apartment; a worker may have none or the
    // multi-threaded one. The check must not care, and must not leave COM in a different state behind.
    if !cfg!(windows) {
        return;
    }
    let exe = std::env::temp_dir().join("hdrcast-no-such-program.exe");
    let here = firewall::check(&exe);
    let handles: Vec<_> = (0..4).map(|_| {
        let exe = exe.clone();
        std::thread::spawn(move || firewall::check(&exe))
    }).collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), here);
    }
}

// ---- OpenIpv4FlagTests --------------------------------------------------------------------------

#[test]
fn the_live_record_carries_open4_only_when_known_and_old_records_read_as_unknown() {
    let mut rec = LiveRecord {
        host_id: keys::new_id(),
        name: "NiseMono".to_string(),
        candidates: vec!["203.0.113.65:50331".to_string()],
        ..LiveRecord::default()
    };
    assert!(!rec.to_payload().contains_key("open4")); // byte-identical to a 0.2.3 host's record
    assert_eq!(LiveRecord::from_payload(&rec.to_payload(), &rec.host_id).unwrap().open_ipv4, None);
    rec.open_ipv4 = Some(true);
    assert_eq!(LiveRecord::from_payload(&rec.to_payload(), &rec.host_id).unwrap().open_ipv4, Some(true));
    rec.open_ipv4 = Some(false);
    assert_eq!(LiveRecord::from_payload(&rec.to_payload(), &rec.host_id).unwrap().open_ipv4, Some(false));
    // and the rest of the record makes the trip unchanged
    assert_eq!(LiveRecord::from_payload(&rec.to_payload(), &rec.host_id).unwrap(), rec);
}
