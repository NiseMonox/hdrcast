//! This crate against the C# it was ported from, on some 1350 inputs chosen to be awkward: every parser,
//! validator and formatter is asked the question that HdrCast 0.2.4 was asked on .NET Framework 4.8, and has to
//! give the recorded answer (tests/data/csharp-0.2.4-answers.txt). Friends keep running 0.2.4; what it accepts
//! and produces is the protocol.
//!
//! Where the recorded line carries a second answer ("!="), this crate differs on purpose. Those lines are few
//! and of these kinds only:
//!
//! - JSON that no HdrCast, Python or mpv writes: MiniJson took numbers like `01` and `1.`, raw control characters
//!   and lone surrogate escapes inside strings, 200 nested brackets, and a bare number or `true` followed by
//!   junk; serde_json refuses them, so such a payload, invite or mpv line is "not valid" here.
//! - Numbers beyond the 64-bit range: the C# cast turned them into i64::MIN, here they saturate (so a version
//!   of 2^63 reads as "newer", not "invalid"). Not-a-number and negative zero differ in bits nobody looks at.
//! - A display name cut at 24 UTF-16 units: C# could keep half a surrogate pair, here the whole character goes.
//! - IPv6 addresses with an embedded IPv4 part (`::1.2.3.4`, ISATAP) are spelled in hex groups, not dotted,
//!   and a leading zero in the dotted part of `::ffff:01.2.3.4` is refused.
//! - `srt_url::parse` (only used to look at URLs in tests) refuses ports above 65535 and keeps a raw
//!   non-BMP character that the C# decoder mangled.
//! - A signaling URL with a control character in its path, or junk right after `[v6]`, is refused.
//! - group.json: where the C# serializer threw the whole file away for one bad field (a duplicate key, `1.5`
//!   for an integer, `"yes"` for the flag), the good fields are kept here. The other way round, it took a file
//!   whose closing brace is missing and the JSON oddities of the first item; those read as "no group" here.

use hdrcast_core::group::keys::{self, GroupKeys};
use hdrcast_core::json::{self, Value};
use hdrcast_core::messages::{request_auth, JoinAnswer, JoinRequest, LiveRecord, SignedEnvelope};
use hdrcast_core::srt_url::{self, SrtRole, SrtTopology, SrtUrlOptions};
use hdrcast_core::{
    arg_quote, base64url, config, endpoint, firewall, invite, secret_redactor, srt_buffer_sizing, GroupConfig, Invite,
};

const ANSWERS: &str = include_str!("data/csharp-0.2.4-answers.txt");
/// The peer id the group.json cases carry.
const PEER: &str = "wMHCw8TFxsfIycrLzM3Ozw";

/// `%uXXXX` → that UTF-16 code unit; everything else is itself.
fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut units: Vec<u16> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            assert_eq!(bytes[i + 1], b'u', "a bare % in the fixture: {s}");
            units.push(u16::from_str_radix(&s[i + 2..i + 6], 16).unwrap());
            i += 6;
        } else {
            units.push(u16::from(bytes[i]));
            i += 1;
        }
    }
    String::from_utf16_lossy(&units)
}

fn escape(s: &str) -> String {
    let mut out = String::new();
    for unit in s.encode_utf16() {
        if (0x20..=0x7e).contains(&unit) && unit != u16::from(b'%') {
            out.push(unit as u8 as char);
        } else {
            out.push_str(&format!("%u{unit:04x}"));
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(h: &str) -> Vec<u8> {
    (0..h.len() / 2).map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap()).collect()
}

fn bit(v: bool) -> &'static str {
    if v {
        "1"
    } else {
        "0"
    }
}

fn opt_bit(v: Option<bool>) -> &'static str {
    v.map_or("-", bit)
}

/// A JSON value in a notation that shows what the reader made of it: n, t, f, i<integer>, d<bits of a double>,
/// s(text), [items], {k(key)=value}. Integers beyond i64 are doubles, as they were in MiniJson.
fn dump(v: &Value) -> String {
    match v {
        Value::Null => "n".to_string(),
        Value::Bool(true) => "t".to_string(),
        Value::Bool(false) => "f".to_string(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => format!("i{i}"),
            None => format!("d{:016x}", n.as_f64().unwrap().to_bits()),
        },
        Value::String(s) => format!("s({s})"),
        Value::Array(items) => format!("[{}]", items.iter().map(dump).collect::<Vec<_>>().join(",")),
        Value::Object(o) => {
            format!("{{{}}}", o.iter().map(|(k, v)| format!("k({k})={}", dump(v))).collect::<Vec<_>>().join(","))
        }
    }
}

fn invite_result(text: &str) -> String {
    match invite::try_extract(text) {
        Ok(inv) => format!(
            "ok|{}|{}|{}|{}|{}",
            inv.signal_url(),
            inv.group_id(),
            hex(inv.group_key()),
            inv.group_name(),
            inv.fingerprint()
        ),
        Err(e) => format!("err|{e:?}"),
    }
}

fn object(text: &str) -> Result<json::Map<String, Value>, String> {
    match json::parse(text) {
        Err(_) => Err("parseerr".to_string()),
        Ok(Value::Object(o)) => Ok(o),
        Ok(_) => Err("notobject".to_string()),
    }
}

/// The answer of this crate to one case, in the notation of the fixture.
fn answer(kind: &str, f: &[String]) -> String {
    let key: Vec<u8> = (0u8..32).collect();
    let keys = GroupKeys::new(&key).unwrap();
    match kind {
        "ep" => match endpoint::parse_with(&f[0], f[1] == "1") {
            Ok(hp) => {
                format!("ok|{}|{}|{:?}", hp.host(), hp.port().map_or("-".to_string(), |p| p.to_string()), hp.kind())
            }
            Err(e) => format!("err|{e}"),
        },
        "norm" => endpoint::normalize(&f[0]),
        "fmt" => endpoint::format_host(&f[0]),
        "quote" => arg_quote::quote(&f[0]),
        "redact" => secret_redactor::redact(&f[0]),
        "b64d" => base64url::decode(&f[0]).map_or("fail".to_string(), |d| format!("ok|{}", hex(&d))),
        "b64e" => base64url::encode(&unhex(&f[0])),
        "id" => bit(keys::is_valid_id(&f[0])).to_string(),
        "srtenc" => srt_url::encode(&f[0]),
        "srtdec" => srt_url::decode(&f[0]),
        "srtparse" => match srt_url::parse(&f[0]) {
            Ok(p) => {
                let query: Vec<String> = p.query.iter().map(|(k, v)| format!("{k}\u{1e}{v}")).collect();
                format!("ok|{}|{}|{}", p.host, p.port, query.join("\u{1f}"))
            }
            Err(e) => format!("err|{e}"),
        },
        "listen" => match srt_url::listen_host_for(Some(&f[0])) {
            Ok(host) => format!("ok|{host}"),
            Err(e) => format!("err|{e}"),
        },
        "caller" => match srt_url::caller_host_for(&f[0], f[1].parse().unwrap()) {
            Ok(host) => format!("ok|{host}"),
            Err(e) => format!("err|{e}"),
        },
        "srtbuild" => {
            let options = SrtUrlOptions {
                topology: if f[0] == "L" { SrtTopology::Listener } else { SrtTopology::Caller },
                role: if f[1] == "P" { SrtRole::Publish } else { SrtRole::Read },
                host: f[2].clone(),
                port: f[3].parse().unwrap(),
                listen_address: (!f[4].is_empty()).then(|| f[4].clone()),
                passphrase: (!f[5].is_empty()).then(|| f[5].clone()),
                pb_key_len: f[6].parse().unwrap(),
                latency_ms: f[7].parse().unwrap(),
                bitrate_bps: f[8].parse().unwrap(),
                pkt_size: f[9].parse().unwrap(),
                live_trans_type: f[10] == "1",
                rate_control: f[11] == "1",
                overhead_percent: f[12].parse().unwrap(),
                connect_timeout_ms: f[13].parse().unwrap(),
                include_recv_socket_buffer: f[14] == "1",
                recv_socket_buffer_bytes: f[15].parse().unwrap(),
            };
            srt_url::build(&options).unwrap_or_else(|e| format!("err|{e}"))
        }
        "sizing" => match srt_buffer_sizing::compute(f[0].parse().unwrap(), f[1].parse().unwrap()) {
            Ok(plan) => format!("{}|{}|{}", plan.packets, plan.flow_window, plan.buffer_bytes),
            Err(e) => format!("err|{e}"),
        },
        "problem" => match Invite::new(&f[0], &f[1], &vec![0u8; f[2].parse().unwrap()], &f[3]) {
            Ok(inv) => format!("ok|{}|{}", inv.signal_url(), inv.group_name()),
            Err(problem) => format!("err|{problem}"),
        },
        "invite" => {
            // the field is the JSON inside the code; wrap it the way an invite is wrapped
            let raw = f[0].as_bytes();
            let code = format!("hdrcast:{}.{}", base64url::encode(raw), &base64url::encode(&keys::sha256(raw))[..8]);
            invite_result(&code)
        }
        "extract" => invite_result(&f[0]),
        "live" => match object(&f[0]) {
            Err(why) => why,
            Ok(payload) => match LiveRecord::from_payload(&payload, &f[1]) {
                None => "null".to_string(),
                Some(r) => format!(
                    "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
                    r.host_id,
                    user_or(r.name),
                    r.time_ms,
                    r.since_ms,
                    r.candidates.join(","),
                    r.codec,
                    r.mbps,
                    r.max_viewers,
                    r.viewers,
                    opt_bit(r.open_ipv4)
                ),
            },
        },
        "join" => match object(&f[0]) {
            Err(why) => why,
            Ok(payload) => match JoinRequest::from_payload(&payload) {
                None => "null".to_string(),
                Some(j) => format!(
                    "{}|{}|{}|{}|{}|{}|{}",
                    j.session_id,
                    j.viewer_id,
                    user_or(j.name),
                    j.time_ms,
                    j.candidates.join(","),
                    j.latency_ms,
                    j.overhead_percent
                ),
            },
        },
        "answer" => match object(&f[0]) {
            Err(why) => why,
            Ok(payload) => match JoinAnswer::from_payload(&payload, &f[1]) {
                None => "null".to_string(),
                Some(a) => {
                    format!("{}|{}|{}|{}|{}", a.session_id, a.time_ms, bit(a.ok), a.reason, a.candidates.join(","))
                }
            },
        },
        "seal" => match object(&f[3]) {
            Err(why) => why,
            Ok(payload) => {
                let env = SignedEnvelope::seal(&keys, &f[0], &f[1], &f[2], &payload);
                format!("{}|{}|{}", env.p, env.m, env.to_json())
            }
        },
        "open" => match SignedEnvelope::new(f[3].clone(), f[4].clone()).try_open(&keys, &f[0], &f[1], &f[2]) {
            Some(payload) => format!("ok|{}", dump(&Value::Object(payload))),
            None => "fail".to_string(),
        },
        "auth" => request_auth::header(keys.server_key(), &f[0], &f[1], f[2].as_bytes(), f[3].parse().unwrap(), &f[4]),
        "keys" => match GroupKeys::new(&unhex(&f[0])) {
            Ok(k) => format!(
                "{}|{}|{}|{}",
                base64url::encode(k.server_key()),
                base64url::encode(k.msg_key()),
                k.srt_passphrase(),
                base64url::encode(&k.punch_key(&f[1]))
            ),
            Err(e) => format!("err|{e}"),
        },
        "fingerprint" => Invite::new(&f[0], &f[1], &unhex(&f[2]), &f[3]).unwrap().fingerprint(),
        "encode" => invite::encode(&Invite::new(&f[0], &f[1], &unhex(&f[2]), &f[3]).unwrap()),
        "clean" => user_or(config::clean_display_name(&f[0])),
        "same" => bit(firewall::same_program(&f[0], &f[1])).to_string(),
        "aslong" | "asdouble" | "asbool" => match json::parse(&f[0]) {
            Err(_) => "parseerr".to_string(),
            Ok(v) => match kind {
                "aslong" => json::as_i64(&v).map_or("null".to_string(), |l| l.to_string()),
                "asdouble" => json::as_f64(&v).map_or("null".to_string(), |d| format!("{:016x}", d.to_bits())),
                _ => opt_bit(json::as_bool(&v)).to_string(),
            },
        },
        "json" => json::parse(&f[0]).map_or("err".to_string(), |v| dump(&v)),
        "cfg" => {
            // through a real file, like the apps: the byte order mark handling is part of the answer
            let file = std::env::temp_dir()
                .join(format!("hdrcast-answers-{}-{:?}.json", std::process::id(), std::thread::current().id()));
            std::fs::write(&file, f[0].as_bytes()).unwrap();
            let c = GroupConfig::load(&file);
            std::fs::remove_file(&file).unwrap();
            let peer = if c.peer_id == PEER {
                "peer-kept"
            } else if keys::is_valid_id(&c.peer_id) {
                "peer-new"
            } else {
                "peer-bad"
            };
            format!(
                "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
                c.version,
                c.signal_url,
                c.group_id,
                c.group_key,
                c.group_name,
                user_or(c.display_name.clone()),
                peer,
                c.imported_at_ms,
                bit(c.detect_clipboard_invites),
                c.dismissed_fingerprint,
                bit(c.is_configured())
            )
        }
        other => panic!("the fixture has a kind this test does not know: {other}"),
    }
}

/// A missing display name falls back to this machine's user name (also for a peer's record without a name, as
/// in the C# apps); the fixture writes it as `<user>`.
fn user_or(name: String) -> String {
    if name == config::clean_display_name("") {
        "<user>".to_string()
    } else {
        name
    }
}

struct Case<'a> {
    line: &'a str,
    kind: &'a str,
    fields: Vec<String>,
    csharp: &'a str,
    /// Set where this crate differs on purpose.
    rust: Option<&'a str>,
}

fn cases() -> Vec<Case<'static>> {
    ANSWERS
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (case, answers) = line.split_once("\t=>\t").expect("a case line");
            let (csharp, rust) = match answers.split_once("\t!=\t") {
                Some((csharp, rust)) => (csharp, Some(rust)),
                None => (answers, None),
            };
            let mut parts = case.split('\t');
            let kind = parts.next().unwrap();
            Case { line, kind, fields: parts.map(unescape).collect(), csharp, rust }
        })
        .collect()
}

#[test]
fn this_crate_answers_like_hdrcast_0_2_4() {
    let cases = cases();
    assert!(cases.len() > 1300, "the fixture lost cases: {}", cases.len());
    let mut wrong = Vec::new();
    let mut checked = 0;
    for case in &cases {
        // Windows Firewall rule paths follow Windows path rules; elsewhere they are compared as written.
        if case.kind == "same" && !cfg!(windows) {
            continue;
        }
        let expected = case.rust.unwrap_or(case.csharp);
        let got = escape(&answer(case.kind, &case.fields));
        checked += 1;
        if got != expected {
            wrong.push(format!("{}\n    answered: {got}", case.line));
        }
    }
    assert!(wrong.is_empty(), "{} of {checked} answers changed:\n{}", wrong.len(), wrong.join("\n"));
}

#[test]
fn the_deliberate_differences_stay_few_and_where_they_are_known() {
    let cases = cases();
    let differing: Vec<&Case> = cases.iter().filter(|c| c.rust.is_some()).collect();
    assert!(differing.iter().all(|c| c.rust != Some(c.csharp)), "a difference that is none");
    assert_eq!(differing.len(), 88, "a new difference from the C# apps needs a reason (see the top of this file)");

    // Nothing differs in what goes over the wire or into a log, whatever the input:
    for kind in [
        "seal", "auth", "keys", "encode", "fingerprint", "extract", "b64d", "b64e", "id", "join", "answer", "srtbuild",
        "srtenc", "listen", "caller", "sizing", "norm", "fmt", "quote", "redact", "same",
    ] {
        assert!(cases.iter().any(|c| c.kind == kind), "no {kind} cases");
        assert!(differing.iter().all(|c| c.kind != kind), "{kind} must answer exactly like the C# apps");
    }
}
