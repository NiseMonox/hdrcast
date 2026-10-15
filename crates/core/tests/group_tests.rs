//! Port of tests/GroupTests.cs: GroupKeyTests, InviteTests, SignedEnvelopeTests, GroupConfigTests.
//! Reads tests/vectors/group-v1.json (made by make_group_v1.py, the reference implementation), the same file
//! the C# tests and the signaling server's Python tests read.

use std::path::PathBuf;

use hdrcast_core::base64url;
use hdrcast_core::group::keys::{self, GroupKeys};
use hdrcast_core::invite::{self, Invite, InviteError};
use hdrcast_core::json::{self, Value};
use hdrcast_core::messages::{self, request_auth, JoinAnswer, JoinRequest, LiveRecord, Payload, SignedEnvelope};
use hdrcast_core::{srt_url, GroupConfig};

mod vectors {
    use super::*;

    const TEXT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/vectors/group-v1.json"));

    pub fn root() -> Value {
        serde_json::from_str(TEXT).expect("group-v1.json is JSON")
    }

    /// A top-level string.
    pub fn s(key: &str) -> String {
        json::get_str(&root(), key).unwrap_or_else(|| panic!("no {key} in the vectors")).to_string()
    }

    pub fn get(path: &[&str]) -> Value {
        let mut at = root();
        for key in path {
            at = json::get(&at, key).unwrap_or_else(|| panic!("no {path:?} in the vectors")).clone();
        }
        at
    }

    pub fn payload(kind: &str) -> Payload {
        match get(&[kind, "payload"]) {
            Value::Object(o) => o,
            other => panic!("{kind}: {other}"),
        }
    }

    pub fn key() -> Vec<u8> {
        base64url::decode(&s("groupKey")).unwrap()
    }

    pub fn keys() -> GroupKeys {
        GroupKeys::new(&key()).unwrap()
    }
}

// ---- GroupKeyTests ------------------------------------------------------------------------------

#[test]
fn derived_keys_match_the_reference() {
    let k = vectors::keys();
    assert_eq!(base64url::encode(k.server_key()), vectors::s("serverKey"));
    assert_eq!(base64url::encode(k.msg_key()), vectors::s("msgKey"));
    assert_eq!(k.srt_passphrase(), vectors::s("srtPassphrase"));
    assert_eq!(base64url::encode(&k.punch_key(&vectors::s("sid"))), vectors::s("punchKey"));
    assert!(srt_url::is_valid_passphrase(k.srt_passphrase()));
    assert_eq!(k.srt_passphrase().len(), 32);
}

#[test]
fn ids_are_22_base64url_characters() {
    let id = keys::new_id();
    assert_eq!(id.len(), 22);
    assert!(keys::is_valid_id(&id));
    assert!(keys::is_valid_id(&vectors::s("gid")));
    assert!(!keys::is_valid_id("short"));
    assert!(!keys::is_valid_id("oKGio6SlpqeoqaqrrK2ur+"));
    assert!(!keys::is_valid_id(""));
}

#[test]
fn base64url_round_trips_and_accepts_padding() {
    let data = [0xfb, 0xff, 0x00, 0x10];
    let s = base64url::encode(&data);
    assert_eq!(s, "-_8AEA");
    assert_eq!(base64url::decode(&s).as_deref(), Some(&data[..]));
    assert_eq!(base64url::decode(&format!("{s}==")).as_deref(), Some(&data[..]));
    assert_eq!(base64url::decode("a"), None);
    assert_eq!(base64url::decode("ab+c"), None);
}

// ---- InviteTests --------------------------------------------------------------------------------

fn vector_invite() -> Invite {
    Invite::new("http://101.43.23.94:18650", &vectors::s("gid"), &vectors::key(), "测试组").unwrap()
}

#[test]
fn encoding_matches_the_reference_byte_for_byte() {
    assert_eq!(invite::encode(&vector_invite()), vectors::s("inviteCode"));
    // and so does the JSON inside it
    let code = vectors::s("inviteCode");
    let payload = code.strip_prefix("hdrcast:").unwrap().split('.').next().unwrap();
    assert_eq!(String::from_utf8(base64url::decode(payload).unwrap()).unwrap(), vectors::s("inviteJson"));
}

#[test]
fn the_reference_code_decodes() {
    let inv = invite::try_extract(&vectors::s("inviteCode")).unwrap();
    assert_eq!(inv.signal_url(), "http://101.43.23.94:18650");
    assert_eq!(inv.group_id(), vectors::s("gid"));
    assert_eq!(inv.group_key().as_slice(), vectors::key());
    assert_eq!(inv.group_name(), "测试组");
    assert_eq!(inv, vector_invite());
}

#[test]
fn a_new_group_round_trips() {
    let created = Invite::create("宇晨的朋友们", None).unwrap();
    let back = invite::try_extract(&invite::encode(&created)).unwrap();
    assert_eq!(created.group_id(), back.group_id());
    assert_eq!(created.group_key(), back.group_key());
    assert_eq!(back.signal_url(), invite::DEFAULT_SIGNAL_URL);
    assert!(invite::encode(&created).len() < 260);
}

#[test]
fn survives_what_chat_apps_do_to_text() {
    let code = vectors::s("inviteCode");
    // wrapped every 40 characters with CRLF, zero-width spaces sprinkled in, text around it
    let mut wrapped = String::new();
    for (i, c) in code.chars().enumerate() {
        if i > 0 && i % 40 == 0 {
            wrapped.push_str("\r\n");
        }
        if i % 17 == 5 {
            wrapped.push('\u{200B}');
        }
        wrapped.push(c);
    }
    let message = format!("宇晨：来看我直播！\n{wrapped}\n复制整段就行。");
    let inv = invite::try_extract(&message).unwrap();
    assert_eq!(inv.group_id(), vectors::s("gid"));

    // full-width prefix typed by an IME, and upper-case
    let full_width = format!("ｈｄｒｃａｓｔ：{}", &code["hdrcast:".len()..]);
    assert!(invite::try_extract(&full_width).is_ok());
    assert!(invite::try_extract(&format!("HDRCAST:{}，谢谢", &code[8..])).is_ok());
}

#[test]
fn text_directly_after_the_checksum_is_ignored() {
    let code = vectors::s("inviteCode");
    assert!(invite::try_extract(&format!("{code}ok.bye")).is_ok());
    assert!(invite::try_extract(&format!("{code}\nsee you.")).is_ok());
}

#[test]
fn truncated_damaged_newer_and_missing_codes_are_told_apart() {
    let code = vectors::s("inviteCode");
    assert_eq!(invite::try_extract("hello"), Err(InviteError::NotFound));

    assert_eq!(invite::try_extract(&code[..code.len() - 20]), Err(InviteError::Incomplete));
    assert_eq!(invite::try_extract(&code[..code.len() - 3]), Err(InviteError::Incomplete));

    let mut flipped = code.clone().into_bytes();
    flipped[20] = if flipped[20] == b'A' { b'B' } else { b'A' };
    assert_eq!(invite::try_extract(&String::from_utf8(flipped).unwrap()), Err(InviteError::Damaged));

    assert_eq!(invite::try_extract(&craft("{\"v\":2,\"s\":\"http://x:1\"}")), Err(InviteError::NewerVersion));

    let bad_key = format!(
        "{{\"v\":1,\"s\":\"http://101.43.23.94:18650\",\"g\":\"{}\",\"k\":\"AAAA\",\"n\":\"x\"}}",
        vectors::s("gid")
    );
    assert_eq!(invite::try_extract(&craft(&bad_key)), Err(InviteError::Invalid));

    for e in InviteError::ALL {
        assert!(!e.describe().trim().is_empty());
        assert_eq!(e.describe(), e.to_string());
    }
}

#[test]
fn the_first_valid_code_wins_over_a_broken_one_before_it() {
    let code = vectors::s("inviteCode");
    let text = format!("old: {}\nnew: {code}", &code[..30]);
    let inv = invite::try_extract(&text).unwrap();
    assert_eq!(inv.group_id(), vectors::s("gid"));
}

fn craft(json: &str) -> String {
    let b = json.as_bytes();
    format!("hdrcast:{}.{}", base64url::encode(b), &base64url::encode(&keys::sha256(b))[..8])
}

// ---- SignedEnvelopeTests ------------------------------------------------------------------------

#[test]
fn sealing_matches_the_reference_and_opens_again() {
    for (kind, key_field) in [("live", "host"), ("join", "host"), ("answer", "sid")] {
        let keys = vectors::keys();
        let gid = vectors::s("gid");
        let key = vectors::s(key_field);
        let env = SignedEnvelope::seal(&keys, kind, &gid, &key, &vectors::payload(kind));
        let reference = vectors::get(&[kind, "envelope"]);
        assert_eq!(Some(env.p.as_str()), json::get_str(&reference, "p"), "{kind}");
        assert_eq!(Some(env.m.as_str()), json::get_str(&reference, "m"), "{kind}");

        let opened = env.try_open(&keys, kind, &gid, &key).expect(kind);
        assert_eq!(opened.get("v").and_then(json::as_i64), Some(1));
    }
}

#[test]
fn the_typed_payloads_write_the_reference_bytes_too() {
    // Reading the reference payload into its model and writing it out again gives the very same envelope:
    // field order, number formatting and escaping all agree with the Python reference.
    let keys = vectors::keys();
    let (gid, host, sid) = (vectors::s("gid"), vectors::s("host"), vectors::s("sid"));
    let p_of = |kind: &str| json::get_str(&vectors::get(&[kind, "envelope"]), "p").unwrap().to_string();

    let live = LiveRecord::from_payload(&vectors::payload("live"), &host).unwrap();
    assert_eq!(SignedEnvelope::seal(&keys, messages::KIND_LIVE, &gid, &host, &live.to_payload()).p, p_of("live"));
    let join = JoinRequest::from_payload(&vectors::payload("join")).unwrap();
    assert_eq!(SignedEnvelope::seal(&keys, messages::KIND_JOIN, &gid, &host, &join.to_payload()).p, p_of("join"));
    let answer = JoinAnswer::from_payload(&vectors::payload("answer"), &sid).unwrap();
    assert_eq!(SignedEnvelope::seal(&keys, messages::KIND_ANSWER, &gid, &sid, &answer.to_payload()).p, p_of("answer"));
}

/// The optional "rc" flag (docs/design.md §8.9). Left out, the payloads are the reference bytes (the test
/// above); set, it is one more field at the end, which a reader that only asks for the fields it knows — the
/// 0.2.4 apps, read here through the same field-by-field accessors — never notices.
#[test]
fn the_remote_control_flag_is_an_extra_field_older_readers_skip() {
    let keys = vectors::keys();
    let (gid, host) = (vectors::s("gid"), vectors::s("host"));
    let json_of = |env: &SignedEnvelope| String::from_utf8(base64url::decode(&env.p).unwrap()).unwrap();
    let reference_json = |kind: &str| {
        json_of(&SignedEnvelope::from_json_object(&vectors::get(&[kind, "envelope"])).unwrap())
    };

    let reference = LiveRecord::from_payload(&vectors::payload("live"), &host).unwrap();
    assert_eq!(reference.remote_control, None);
    let offering = LiveRecord { remote_control: Some(true), ..reference.clone() };
    let env = SignedEnvelope::seal(&keys, messages::KIND_LIVE, &gid, &host, &offering.to_payload());
    let live_json = reference_json("live");
    assert_eq!(json_of(&env), format!("{},\"rc\":true}}", &live_json[..live_json.len() - 1]));
    let opened = env.try_open(&keys, messages::KIND_LIVE, &gid, &host).unwrap();
    let read = LiveRecord::from_payload(&opened, &host).unwrap();
    assert_eq!(read.remote_control, Some(true));
    assert_eq!(LiveRecord { remote_control: None, ..read }, reference, "every 0.2.4 field reads as before");

    let reference = JoinRequest::from_payload(&vectors::payload("join")).unwrap();
    assert!(!reference.remote_control);
    let asking = JoinRequest { remote_control: true, ..reference.clone() };
    let env = SignedEnvelope::seal(&keys, messages::KIND_JOIN, &gid, &host, &asking.to_payload());
    let join_json = reference_json("join");
    assert_eq!(json_of(&env), format!("{},\"rc\":true}}", &join_json[..join_json.len() - 1]));
    let opened = env.try_open(&keys, messages::KIND_JOIN, &gid, &host).unwrap();
    let read = JoinRequest::from_payload(&opened).unwrap();
    assert!(read.remote_control);
    assert_eq!(JoinRequest { remote_control: false, ..read }, reference, "every 0.2.4 field reads as before");

    // and the reference envelopes themselves (no "rc") still verify and read as "no"
    for kind in ["live", "join"] {
        let env = SignedEnvelope::from_json_object(&vectors::get(&[kind, "envelope"])).unwrap();
        let payload = env.try_open(&keys, kind, &gid, &host).expect(kind);
        assert!(!payload.contains_key("rc"), "{kind}");
    }
}

#[test]
fn any_change_breaks_the_seal() {
    let keys = vectors::keys();
    let gid = vectors::s("gid");
    let host = vectors::s("host");
    let env = SignedEnvelope::from_json_object(&vectors::get(&["live", "envelope"])).unwrap();
    assert!(env.try_open(&keys, "live", &gid, &host).is_some());

    assert!(env.try_open(&keys, "join", &gid, &host).is_none()); // other kind
    assert!(env.try_open(&keys, "live", &keys::new_id(), &host).is_none()); // other group
    assert!(env.try_open(&keys, "live", &gid, &keys::new_id()).is_none()); // other host
    let strangers = GroupKeys::new(&keys::random_bytes(32)).unwrap();
    assert!(env.try_open(&strangers, "live", &gid, &host).is_none());
    let p = format!("{}{}", &env.p[..env.p.len() - 1], if env.p.ends_with('A') { "B" } else { "A" });
    assert!(SignedEnvelope::new(p, env.m.clone()).try_open(&keys, "live", &gid, &host).is_none());
    assert!(SignedEnvelope::new(env.p.clone(), "garbage").try_open(&keys, "live", &gid, &host).is_none());
}

#[test]
fn payload_models_read_what_the_reference_wrote() {
    let live = LiveRecord::from_payload(&vectors::payload("live"), &vectors::s("host")).unwrap();
    assert_eq!(live.name, "小明");
    assert_eq!(live.candidates.len(), 2);
    assert_eq!(live.mbps, 40);
    assert_eq!(LiveRecord::from_payload(&vectors::payload("live"), &keys::new_id()), None);

    let join = JoinRequest::from_payload(&vectors::payload("join")).unwrap();
    assert_eq!(join.session_id, vectors::s("sid"));
    assert_eq!(join.latency_ms, 3500);
    assert_eq!(join.overhead_percent, 50);

    let answer = JoinAnswer::from_payload(&vectors::payload("answer"), &vectors::s("sid")).unwrap();
    assert!(answer.ok);
    assert_eq!(answer.candidates.len(), 1);
}

#[test]
fn candidates_are_filtered_to_parseable_literals_with_ports() {
    let mut p = Payload::new();
    p.insert("sid".into(), Value::from(keys::new_id()));
    p.insert("viewer".into(), Value::from(keys::new_id()));
    p.insert(
        "cands".into(),
        serde_json::json!(["[2001:db8::1]:4000", "example.com:4000", "1.2.3.4", "fe80::1", 5, "10.0.0.2:5000"]),
    );
    let j = JoinRequest::from_payload(&p).unwrap();
    assert_eq!(j.candidates, ["[2001:db8::1]:4000", "10.0.0.2:5000"]);
}

#[test]
fn request_auth_header_matches_the_reference() {
    let a = vectors::get(&["auth"]);
    let text = |key: &str| json::get_str(&a, key).unwrap();
    let header = request_auth::header(
        vectors::keys().server_key(),
        text("method"),
        text("path"),
        text("body").as_bytes(),
        json::get(&a, "ts").and_then(json::as_i64).unwrap(),
        text("nonce"),
    );
    assert_eq!(header, text("header"));
    assert_eq!(base64url::encode(&keys::sha256(b"")), text("emptyBodyHash"));
    assert_eq!(base64url::encode(&keys::sha256(text("body").as_bytes())), text("bodyHash"));
    // the body the reference signed is the join envelope exactly as this crate serializes it
    let join = SignedEnvelope::from_json_object(&vectors::get(&["join", "envelope"])).unwrap();
    assert_eq!(join.to_json(), text("body"));
}

// ---- GroupConfigTests ---------------------------------------------------------------------------

#[test]
fn adopting_an_invite_keeps_this_installs_peer_id() {
    let cfg = GroupConfig::default().normalize();
    let peer = cfg.peer_id.clone();
    assert!(keys::is_valid_id(&peer));
    assert!(!cfg.is_configured());

    let joined = cfg.adopt(&invite::try_extract(&vectors::s("inviteCode")).unwrap(), "  小红\n ");
    assert!(joined.is_configured());
    assert_eq!(joined.peer_id, peer);
    assert_eq!(joined.display_name, "小红");
    assert_eq!(base64url::encode(joined.keys().unwrap().server_key()), vectors::s("serverKey"));
}

#[test]
fn saves_and_loads_and_survives_concurrent_writers() {
    let dir = std::env::temp_dir().join(format!("hdrcast-group-{}", keys::new_id()));
    let path: PathBuf = dir.join(hdrcast_core::config::FILE_NAME);
    let cfg = GroupConfig::default().normalize().adopt(&invite::try_extract(&vectors::s("inviteCode")).unwrap(), "me");
    cfg.save(&path).unwrap();
    let back = GroupConfig::load(&path);
    assert_eq!(back.group_id, cfg.group_id);
    assert_eq!(back.peer_id, cfg.peer_id);
    assert!(back.detect_clipboard_invites);
    assert_eq!(back, cfg);

    // Several programs saving at the same moment: a save may lose the race (an I/O error the caller sees), but
    // the file is always one writer's complete version and no temp file stays behind.
    let failed = std::thread::scope(|scope| {
        let writers: Vec<_> = (0..16).map(|_| scope.spawn(|| cfg.save(&path).is_err())).collect();
        // readers in the middle of it never see a half-written file
        for _ in 0..50 {
            let seen = GroupConfig::load(&path);
            assert!(seen.is_configured() || !path.exists(), "a reader saw a broken file");
        }
        writers.into_iter().map(|w| w.join().unwrap()).filter(|failed| *failed).count()
    });
    assert!(failed < 16, "every concurrent save failed");
    assert_eq!(GroupConfig::load(&path), cfg);
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn display_names_are_cleaned_and_bounded() {
    use hdrcast_core::config::clean_display_name;
    assert_eq!(clean_display_name(" ab\tc "), "abc");
    assert_eq!(clean_display_name(&"x".repeat(100)).len(), 24);
    assert!(!clean_display_name("   ").is_empty());
}
