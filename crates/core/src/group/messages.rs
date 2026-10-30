//! The signed messages of protocol v1 (docs/design.md §8.4, §8.5): the envelope, the request authentication
//! header, and the three payloads (live record, join request, join answer).
//!
//! Payloads are read field by field with the forgiving accessors of [`json`], exactly as 0.2.4 reads them:
//! unknown fields are ignored, a missing or mistyped field takes its default, and a number may arrive as an
//! integer, a float or a string. They are written in the fixed order of the reference vectors.

use serde_json::Value;

use super::config::clean_display_name;
use super::device;
use super::keys::{self, GroupKeys};
use crate::common::{base64url, endpoint, json};

/// A payload object; keys keep the order they were inserted in.
pub type Payload = serde_json::Map<String, Value>;

pub const KIND_LIVE: &str = "live";
pub const KIND_JOIN: &str = "join";
pub const KIND_ANSWER: &str = "answer";

/// `cands` carries at most this many addresses; the rest are dropped by the reader.
pub const MAX_CANDIDATES: usize = 8;

/// A `p` longer than this is not even checked (the signaling server caps a whole body at 8192 bytes).
const MAX_PAYLOAD_TEXT: usize = 16384;

/// {"p": b64u(payload JSON), "m": b64u(HMAC-SHA256(msgKey, kind\ngid\nkey\np))} (docs/design.md §8.4).
/// The MAC covers the exact base64url text, so nobody has to agree on JSON formatting; binding kind, group and
/// key (host id or session id) stops a signed message from being replayed somewhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedEnvelope {
    pub p: String,
    pub m: String,
}

impl SignedEnvelope {
    pub fn new(p: impl Into<String>, m: impl Into<String>) -> SignedEnvelope {
        SignedEnvelope { p: p.into(), m: m.into() }
    }

    pub fn seal(keys: &GroupKeys, kind: &str, gid: &str, key: &str, payload: &Payload) -> SignedEnvelope {
        let json = serde_json::to_string(payload).expect("a JSON object always serializes");
        let p = base64url::encode(json.as_bytes());
        let m = base64url::encode(&mac(keys, kind, gid, key, &p));
        SignedEnvelope { p, m }
    }

    /// Verifies the MAC, then decodes the payload object; `None` on any mismatch.
    pub fn try_open(&self, keys: &GroupKeys, kind: &str, gid: &str, key: &str) -> Option<Payload> {
        if self.p.is_empty() || self.p.len() > MAX_PAYLOAD_TEXT {
            return None;
        }
        let claimed = base64url::decode(&self.m)?;
        if !keys::fixed_time_eq(&claimed, &mac(keys, kind, gid, key, &self.p)) {
            return None;
        }
        // Only now is the payload looked at: nothing unauthenticated reaches the JSON parser.
        let raw = base64url::decode(&self.p)?;
        match json::parse(&String::from_utf8_lossy(&raw)) {
            Ok(Value::Object(payload)) => Some(payload),
            _ => None,
        }
    }

    pub fn to_json_object(&self) -> Payload {
        let mut o = Payload::new();
        o.insert("p".to_string(), Value::from(self.p.as_str()));
        o.insert("m".to_string(), Value::from(self.m.as_str()));
        o
    }

    /// `{"p":"…","m":"…"}`, the body of a signaling request.
    pub fn to_json(&self) -> String {
        Value::Object(self.to_json_object()).to_string()
    }

    /// Reads `p` and `m` out of an object (which may carry more: "host", "age_ms", "seq"); `None` when either is
    /// missing or not a string.
    pub fn from_json_object(obj: &Value) -> Option<SignedEnvelope> {
        Some(SignedEnvelope::new(json::get_str(obj, "p")?, json::get_str(obj, "m")?))
    }
}

fn mac(keys: &GroupKeys, kind: &str, gid: &str, key: &str, p: &str) -> [u8; 32] {
    keys::hmac_sha256(keys.msg_key(), format!("{kind}\n{gid}\n{key}\n{p}").as_bytes())
}

/// `X-Hdrcast-Auth: <ts ms>.<nonce>.<mac>` (docs/design.md §8.5).
pub mod request_auth {
    use super::{base64url, keys};

    pub const HEADER_NAME: &str = "X-Hdrcast-Auth";

    /// `path_and_query` is the request target exactly as it goes on the request line.
    pub fn header(server_key: &[u8], method: &str, path_and_query: &str, body: &[u8], ts_ms: i64, nonce: &str) -> String {
        let body_hash = base64url::encode(&keys::sha256(body));
        let msg = format!("{method}\n{path_and_query}\n{ts_ms}\n{nonce}\n{body_hash}");
        let mac = base64url::encode(&keys::hmac_sha256(server_key, msg.as_bytes()));
        format!("{ts_ms}.{nonce}.{mac}")
    }

    /// The header for a request made now, with a fresh nonce (8 random bytes).
    pub fn header_now(server_key: &[u8], method: &str, path_and_query: &str, body: &[u8]) -> String {
        let nonce = base64url::encode(&keys::random_bytes(8));
        header(server_key, method, path_and_query, body, crate::group::now_ms(), &nonce)
    }
}

/// What a live host advertises every 10 s (kind "live").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRecord {
    pub host_id: String,
    pub name: String,
    pub time_ms: i64,
    pub since_ms: i64,
    pub candidates: Vec<String>,
    pub codec: String,
    pub mbps: i32,
    pub max_viewers: i32,
    pub viewers: i32,
    /// The host's IPv4 port takes packets from any address and port (router forward + firewall rule), so viewers
    /// whose routers change ports can reach it. `None` = not known yet, or a host older than 0.2.4.
    pub open_ipv4: Option<bool>,
    /// This host speaks the side channel and offers remote control (docs/design.md §8.9). Written only when
    /// `Some(true)`; absent reads as `None` and means no: nothing may be sent on the side channel to such a host
    /// (0.2.4 would hand the datagrams to SRT).
    pub remote_control: Option<bool>,
    /// The host's device key ("dk", base64url, docs/design.md §8.10), written by hosts from 0.3.1 on. A viewer
    /// that read it accepts only answers signed with it. `None` when absent or not a usable key.
    pub device_key: Option<String>,
    /// "od": the host is available on request instead of broadcasting. It starts capturing when someone it lets
    /// in connects (docs/design.md §8.10). Written only when true; absent means a broadcast (as every older host).
    pub on_demand: bool,
    /// "kf": the host takes keyframe requests on the side channel and answers them (docs/design.md §8.12): its
    /// encoder runs inside HdrCast. Written only when true, last; absent means no, and then nothing of the kind may
    /// be sent to it (a host older than 0.3.3 would hand the datagram to SRT).
    pub keyframes: bool,
    /// "ll": the host offers control mode, the low-latency stream for remote control (docs/design.md §11): a viewer
    /// may join with "ll" and gets the picture in side datagrams instead of through SRT. Written only when true,
    /// last; absent means no (hosts older than 0.3.7, or one whose encoder cannot do it).
    pub low_latency: bool,
}

impl Default for LiveRecord {
    fn default() -> Self {
        LiveRecord {
            host_id: String::new(),
            name: String::new(),
            time_ms: 0,
            since_ms: 0,
            candidates: Vec::new(),
            codec: "hevc".to_string(),
            mbps: 0,
            max_viewers: 0,
            viewers: 0,
            open_ipv4: None,
            remote_control: None,
            device_key: None,
            on_demand: false,
            keyframes: false,
            low_latency: false,
        }
    }
}

impl LiveRecord {
    pub fn to_payload(&self) -> Payload {
        let mut p = Payload::new();
        p.insert("v".to_string(), Value::from(1));
        p.insert("t".to_string(), Value::from(self.time_ms));
        p.insert("host".to_string(), Value::from(self.host_id.as_str()));
        p.insert("name".to_string(), Value::from(self.name.as_str()));
        p.insert("since".to_string(), Value::from(self.since_ms));
        p.insert("cands".to_string(), Value::from(self.candidates.clone()));
        p.insert("codec".to_string(), Value::from(self.codec.as_str()));
        p.insert("mbps".to_string(), Value::from(self.mbps));
        p.insert("max".to_string(), Value::from(self.max_viewers));
        p.insert("viewers".to_string(), Value::from(self.viewers));
        // Written only when known, so a record without it is byte-identical to a 0.2.3 host's.
        if let Some(open) = self.open_ipv4 {
            p.insert("open4".to_string(), Value::from(open));
        }
        // Only ever written as true: a host that does not offer it sends what 0.2.4 sends.
        if self.remote_control == Some(true) {
            p.insert("rc".to_string(), Value::from(true));
        }
        if let Some(key) = &self.device_key {
            p.insert("dk".to_string(), Value::from(key.as_str()));
        }
        if self.on_demand {
            p.insert("od".to_string(), Value::from(true));
        }
        if self.keyframes {
            p.insert("kf".to_string(), Value::from(true));
        }
        if self.low_latency {
            p.insert("ll".to_string(), Value::from(true));
        }
        p
    }

    /// `None` unless the payload's own "host" is the host it was stored and signed under.
    pub fn from_payload(p: &Payload, expected_host: &str) -> Option<LiveRecord> {
        let r = LiveRecord {
            host_id: text(p, "host").unwrap_or("").to_string(),
            name: clean_display_name(text(p, "name").unwrap_or("")),
            time_ms: integer(p, "t").unwrap_or(0),
            since_ms: integer(p, "since").unwrap_or(0),
            candidates: candidates(field(p, "cands")),
            codec: text(p, "codec").unwrap_or("hevc").to_string(),
            mbps: int32(p, "mbps"),
            max_viewers: int32(p, "max"),
            viewers: int32(p, "viewers"),
            open_ipv4: field(p, "open4").and_then(json::as_bool),
            remote_control: field(p, "rc").and_then(json::as_bool),
            device_key: text(p, "dk").filter(|k| device::parse_public_key(k).is_some()).map(str::to_string),
            on_demand: field(p, "od").and_then(json::as_bool).unwrap_or(false),
            keyframes: field(p, "kf").and_then(json::as_bool).unwrap_or(false),
            low_latency: field(p, "ll").and_then(json::as_bool).unwrap_or(false),
        };
        (r.host_id == expected_host).then_some(r)
    }
}

/// Who signed a join or an answer (docs/design.md §8.10): the device key ("dk"), the key for this session's
/// exchange ("ek") and the signature ("ds"), all base64url as sent. Checked by [`device`], not here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceProof {
    pub key: String,
    /// Empty in an answer that refuses: there is no session to agree on a key for.
    pub exchange: String,
    pub signature: String,
}

impl DeviceProof {
    fn write(&self, p: &mut Payload) {
        p.insert("dk".to_string(), Value::from(self.key.as_str()));
        if !self.exchange.is_empty() {
            p.insert("ek".to_string(), Value::from(self.exchange.as_str()));
        }
        p.insert("ds".to_string(), Value::from(self.signature.as_str()));
    }

    /// `None` unless "dk" and "ds" are strings, and "ek" too when `exchange_required`.
    fn read(p: &Payload, exchange_required: bool) -> Option<DeviceProof> {
        let exchange = text(p, "ek");
        if exchange_required && exchange.is_none() {
            return None;
        }
        Some(DeviceProof {
            key: text(p, "dk")?.to_string(),
            exchange: exchange.unwrap_or("").to_string(),
            signature: text(p, "ds")?.to_string(),
        })
    }
}

/// A viewer asking a host for a session (kind "join").
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JoinRequest {
    pub session_id: String,
    pub viewer_id: String,
    pub name: String,
    pub time_ms: i64,
    pub candidates: Vec<String>,
    /// SRT latency the viewer wants (its region preset), ms.
    pub latency_ms: i32,
    /// oheadbw the viewer wants (its region preset), percent.
    pub overhead_percent: i32,
    /// This viewer speaks the side channel and asks for remote control (docs/design.md §8.9). Written (as
    /// `"rc":true`) only when true; absent means no.
    pub remote_control: bool,
    /// The viewer's device and its signature (0.3.1 on); `None` from older viewers.
    pub device: Option<DeviceProof>,
    /// "kf": the viewer asks for a keyframe when it loses part of the stream, and understands the host's answers
    /// on the side channel (docs/design.md §8.12). Written only when true, last; absent means no.
    pub keyframes: bool,
    /// "ll": control mode (docs/design.md §11): the viewer wants the low-latency stream in side datagrams instead of
    /// SRT. Only sent to a host whose live record says "ll". Written only when true, last; absent means no.
    pub low_latency: bool,
}

impl JoinRequest {
    pub fn to_payload(&self) -> Payload {
        let mut p = Payload::new();
        p.insert("v".to_string(), Value::from(1));
        p.insert("t".to_string(), Value::from(self.time_ms));
        p.insert("sid".to_string(), Value::from(self.session_id.as_str()));
        p.insert("viewer".to_string(), Value::from(self.viewer_id.as_str()));
        p.insert("name".to_string(), Value::from(self.name.as_str()));
        p.insert("cands".to_string(), Value::from(self.candidates.clone()));
        p.insert("latency".to_string(), Value::from(self.latency_ms));
        p.insert("overhead".to_string(), Value::from(self.overhead_percent));
        if self.remote_control {
            p.insert("rc".to_string(), Value::from(true));
        }
        if let Some(device) = &self.device {
            device.write(&mut p);
        }
        if self.keyframes {
            p.insert("kf".to_string(), Value::from(true));
        }
        if self.low_latency {
            p.insert("ll".to_string(), Value::from(true));
        }
        p
    }

    /// `None` when the session or viewer id is not an id. Latency and overhead outside what SRT can use fall back
    /// to the defaults (1500 ms, 25 %) instead of failing the join.
    pub fn from_payload(p: &Payload) -> Option<JoinRequest> {
        let mut j = JoinRequest {
            session_id: text(p, "sid").unwrap_or("").to_string(),
            viewer_id: text(p, "viewer").unwrap_or("").to_string(),
            name: clean_display_name(text(p, "name").unwrap_or("")),
            time_ms: integer(p, "t").unwrap_or(0),
            candidates: candidates(field(p, "cands")),
            latency_ms: int32(p, "latency"),
            overhead_percent: int32(p, "overhead"),
            remote_control: field(p, "rc").and_then(json::as_bool).unwrap_or(false),
            device: DeviceProof::read(p, true),
            keyframes: field(p, "kf").and_then(json::as_bool).unwrap_or(false),
            low_latency: field(p, "ll").and_then(json::as_bool).unwrap_or(false),
        };
        if !keys::is_valid_id(&j.session_id) || !keys::is_valid_id(&j.viewer_id) {
            return None;
        }
        if j.latency_ms < 20 || j.latency_ms > 30000 {
            j.latency_ms = 1500;
        }
        if j.overhead_percent < 5 || j.overhead_percent > 100 {
            j.overhead_percent = 25;
        }
        Some(j)
    }
}

/// The host's reply to a join (kind "answer").
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JoinAnswer {
    pub session_id: String,
    pub time_ms: i64,
    pub ok: bool,
    pub reason: String,
    pub candidates: Vec<String>,
    /// The host's device, its exchange key and its signature (0.3.1 on, docs/design.md §8.10).
    pub device: Option<DeviceProof>,
}

impl JoinAnswer {
    pub const REASON_FULL: &'static str = "full";
    pub const REASON_STOPPED: &'static str = "stopped";
    pub const REASON_BUSY: &'static str = "busy";
    /// The host's rules or its user said no (0.3.1 on). Older viewers read it like "busy" and try again later.
    pub const REASON_DENIED: &'static str = "denied";
    /// Nobody at the host answered in time (0.3.1 on).
    pub const REASON_TIMEOUT: &'static str = "timeout";

    pub fn to_payload(&self) -> Payload {
        let mut p = Payload::new();
        p.insert("v".to_string(), Value::from(1));
        p.insert("t".to_string(), Value::from(self.time_ms));
        p.insert("sid".to_string(), Value::from(self.session_id.as_str()));
        p.insert("ok".to_string(), Value::from(self.ok));
        p.insert("reason".to_string(), Value::from(self.reason.as_str()));
        p.insert("cands".to_string(), Value::from(self.candidates.clone()));
        if let Some(device) = &self.device {
            device.write(&mut p);
        }
        p
    }

    /// `None` unless the payload's own "sid" is the session it was stored and signed under.
    pub fn from_payload(p: &Payload, expected_sid: &str) -> Option<JoinAnswer> {
        let a = JoinAnswer {
            session_id: text(p, "sid").unwrap_or("").to_string(),
            time_ms: integer(p, "t").unwrap_or(0),
            ok: field(p, "ok").and_then(json::as_bool).unwrap_or(false),
            reason: text(p, "reason").unwrap_or("").to_string(),
            candidates: candidates(field(p, "cands")),
            device: DeviceProof::read(p, false),
        };
        (a.session_id == expected_sid).then_some(a)
    }
}

/// A JSON array of strings, keeping only entries [`endpoint::parse`] accepts as a literal address with a port
/// (at most [`MAX_CANDIDATES`]). The entries are kept as sent, not rewritten.
pub fn candidates(value: Option<&Value>) -> Vec<String> {
    let mut list = Vec::new();
    let Some(Value::Array(items)) = value else { return list };
    for item in items {
        if list.len() >= MAX_CANDIDATES {
            break;
        }
        let Value::String(s) = item else { continue };
        if endpoint::parse(s).is_ok_and(|hp| hp.port().is_some() && hp.is_literal()) {
            list.push(s.clone());
        }
    }
    list
}

fn field<'a>(p: &'a Payload, key: &str) -> Option<&'a Value> {
    p.get(key).filter(|v| !v.is_null())
}

fn text<'a>(p: &'a Payload, key: &str) -> Option<&'a str> {
    field(p, key)?.as_str()
}

fn integer(p: &Payload, key: &str) -> Option<i64> {
    field(p, key).and_then(json::as_i64)
}

/// An int field: missing reads as 0, and a value outside the int range wraps exactly as the C# cast did
/// (nothing reasonable comes out of such a record either way; what matters is that both sides agree).
fn int32(p: &Payload, key: &str) -> i32 {
    integer(p, key).unwrap_or(0) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> GroupKeys {
        GroupKeys::new(&[3u8; 32]).unwrap()
    }

    fn object(json: &str) -> Payload {
        match json::parse(json).unwrap() {
            Value::Object(o) => o,
            other => panic!("not an object: {other}"),
        }
    }

    #[test]
    fn payloads_are_written_in_the_reference_order() {
        let live = LiveRecord { host_id: "H".into(), name: "n".into(), time_ms: 2, since_ms: 1, candidates: vec!["1.2.3.4:5".into()], mbps: 40, max_viewers: 4, viewers: 1, ..LiveRecord::default() };
        assert_eq!(Value::Object(live.to_payload()).to_string(), r#"{"v":1,"t":2,"host":"H","name":"n","since":1,"cands":["1.2.3.4:5"],"codec":"hevc","mbps":40,"max":4,"viewers":1}"#);
        let open = LiveRecord { open_ipv4: Some(false), ..live };
        assert!(Value::Object(open.to_payload()).to_string().ends_with(r#","viewers":1,"open4":false}"#));

        let join = JoinRequest { session_id: "S".into(), viewer_id: "V".into(), name: "n".into(), time_ms: 3, candidates: vec![], latency_ms: 3500, overhead_percent: 50, remote_control: false, device: None, keyframes: false, low_latency: false };
        assert_eq!(Value::Object(join.to_payload()).to_string(), r#"{"v":1,"t":3,"sid":"S","viewer":"V","name":"n","cands":[],"latency":3500,"overhead":50}"#);

        let answer = JoinAnswer { session_id: "S".into(), time_ms: 4, ok: false, reason: JoinAnswer::REASON_FULL.into(), candidates: vec![], device: None };
        assert_eq!(Value::Object(answer.to_payload()).to_string(), r#"{"v":1,"t":4,"sid":"S","ok":false,"reason":"full","cands":[]}"#);
    }

    #[test]
    fn the_remote_control_flag_is_written_only_when_set_and_absent_reads_as_no() {
        let host = keys::new_id();
        let live = LiveRecord { host_id: host.clone(), name: "n".into(), time_ms: 2, since_ms: 1, mbps: 40, max_viewers: 4, viewers: 1, ..LiveRecord::default() };
        let plain = Value::Object(live.to_payload()).to_string();
        assert!(!plain.contains(r#""rc""#), "{plain}");
        assert_eq!(LiveRecord::from_payload(&live.to_payload(), &host).unwrap().remote_control, None);
        // "no" is not written either: such a record is byte-identical to a 0.2.4 host's
        let no = LiveRecord { remote_control: Some(false), ..live.clone() };
        assert_eq!(Value::Object(no.to_payload()).to_string(), plain);
        let yes = LiveRecord { remote_control: Some(true), ..live.clone() };
        assert!(Value::Object(yes.to_payload()).to_string().ends_with(r#","viewers":1,"rc":true}"#));
        assert_eq!(LiveRecord::from_payload(&yes.to_payload(), &host).unwrap().remote_control, Some(true));
        // after open4, so a record with both still starts with the bytes 0.2.4 writes
        let both = LiveRecord { open_ipv4: Some(true), remote_control: Some(true), ..live };
        assert!(Value::Object(both.to_payload()).to_string().ends_with(r#","viewers":1,"open4":true,"rc":true}"#));
        let read = LiveRecord::from_payload(&both.to_payload(), &host).unwrap();
        assert_eq!((read.open_ipv4, read.remote_control), (Some(true), Some(true)));
        // an explicit false reads as no; anything that is not a flag reads as absent
        let explicit = object(&format!(r#"{{"host":"{host}","rc":false}}"#));
        assert_eq!(LiveRecord::from_payload(&explicit, &host).unwrap().remote_control, Some(false));
        let odd = object(&format!(r#"{{"host":"{host}","rc":7}}"#));
        assert_eq!(LiveRecord::from_payload(&odd, &host).unwrap().remote_control, None);

        let (sid, viewer) = (keys::new_id(), keys::new_id());
        let join = JoinRequest { session_id: sid, viewer_id: viewer, name: "n".into(), time_ms: 3, latency_ms: 3500, overhead_percent: 50, ..JoinRequest::default() };
        let plain = Value::Object(join.to_payload()).to_string();
        assert!(plain.ends_with(r#","latency":3500,"overhead":50}"#), "{plain}");
        assert!(!JoinRequest::from_payload(&join.to_payload()).unwrap().remote_control);
        let asking = JoinRequest { remote_control: true, ..join };
        assert!(Value::Object(asking.to_payload()).to_string().ends_with(r#","latency":3500,"overhead":50,"rc":true}"#));
        assert_eq!(JoinRequest::from_payload(&asking.to_payload()), Some(asking));
    }

    #[test]
    fn the_device_key_and_on_demand_flag_come_last_and_only_when_set() {
        let host = keys::new_id();
        let live = LiveRecord { host_id: host.clone(), name: "n".into(), time_ms: 2, since_ms: 1, mbps: 40, max_viewers: 4, viewers: 1, ..LiveRecord::default() };
        let plain = Value::Object(live.to_payload()).to_string();
        assert!(!plain.contains(r#""dk""#) && !plain.contains(r#""od""#), "{plain}");
        let read = LiveRecord::from_payload(&live.to_payload(), &host).unwrap();
        assert_eq!((read.device_key, read.on_demand), (None, false));

        let dk = device::DeviceKey::generate().public_key().to_string();
        let full = LiveRecord { open_ipv4: Some(true), remote_control: Some(true), device_key: Some(dk.clone()), on_demand: true, ..live.clone() };
        let text = Value::Object(full.to_payload()).to_string();
        assert!(text.ends_with(&format!(r#","viewers":1,"open4":true,"rc":true,"dk":"{dk}","od":true}}"#)), "{text}");
        assert_eq!(LiveRecord::from_payload(&full.to_payload(), &host).unwrap(), full);

        // a "dk" that is not a usable key reads as none; "od" false is not written
        let odd = object(&format!(r#"{{"host":"{host}","dk":"AAAA","od":false}}"#));
        let read = LiveRecord::from_payload(&odd, &host).unwrap();
        assert_eq!((read.device_key, read.on_demand), (None, false));
    }

    #[test]
    fn the_keyframe_flag_comes_last_only_when_set_so_older_peers_see_their_own_bytes() {
        // live: without it the text is what 0.3.2 wrote, with it "kf" follows everything else
        let host = keys::new_id();
        let dk = device::DeviceKey::generate().public_key().to_string();
        let live = LiveRecord { host_id: host.clone(), name: "n".into(), time_ms: 2, since_ms: 1, mbps: 40, max_viewers: 4, viewers: 1, remote_control: Some(true), device_key: Some(dk.clone()), on_demand: true, ..LiveRecord::default() };
        let plain = Value::Object(live.to_payload()).to_string();
        assert!(plain.ends_with(&format!(r#","viewers":1,"rc":true,"dk":"{dk}","od":true}}"#)), "{plain}");
        assert!(!plain.contains(r#""kf""#), "{plain}");
        assert!(!LiveRecord::from_payload(&live.to_payload(), &host).unwrap().keyframes);
        let offering = LiveRecord { keyframes: true, ..live.clone() };
        let text = Value::Object(offering.to_payload()).to_string();
        assert_eq!(text, format!(r#"{}{}"#, &plain[..plain.len() - 1], r#","kf":true}"#));
        assert_eq!(LiveRecord::from_payload(&offering.to_payload(), &host).unwrap(), offering);
        // false is not written; what is not a flag reads as no (strings as the other flags read them)
        for value in ["false", "7", "\"no\"", "null"] {
            let odd = object(&format!(r#"{{"host":"{host}","kf":{value}}}"#));
            assert!(!LiveRecord::from_payload(&odd, &host).unwrap().keyframes, "{value}");
        }

        // join: after the device proof, so a signed join's first bytes and its signature stay what they were
        let (sid, viewer) = (keys::new_id(), keys::new_id());
        let proof = DeviceProof { key: dk.clone(), exchange: "E".into(), signature: "S".into() };
        let join = JoinRequest { session_id: sid, viewer_id: viewer, name: "n".into(), time_ms: 3, latency_ms: 3500, overhead_percent: 50, remote_control: true, device: Some(proof), ..JoinRequest::default() };
        let plain = Value::Object(join.to_payload()).to_string();
        assert!(plain.ends_with(&format!(r#","overhead":50,"rc":true,"dk":"{dk}","ek":"E","ds":"S"}}"#)), "{plain}");
        assert!(!JoinRequest::from_payload(&join.to_payload()).unwrap().keyframes);
        let asking = JoinRequest { keyframes: true, ..join.clone() };
        let text = Value::Object(asking.to_payload()).to_string();
        assert_eq!(text, format!(r#"{}{}"#, &plain[..plain.len() - 1], r#","kf":true}"#));
        assert_eq!(JoinRequest::from_payload(&asking.to_payload()), Some(asking));
        // and without anything else optional it still comes last
        let bare = JoinRequest { remote_control: false, device: None, keyframes: true, ..join };
        assert!(Value::Object(bare.to_payload()).to_string().ends_with(r#","latency":3500,"overhead":50,"kf":true}"#));
    }

    #[test]
    fn the_control_mode_flag_comes_after_everything_only_when_set() {
        let host = keys::new_id();
        let dk = device::DeviceKey::generate().public_key().to_string();
        let live = LiveRecord { host_id: host.clone(), name: "n".into(), mbps: 40, max_viewers: 4, device_key: Some(dk.clone()), on_demand: true, keyframes: true, ..LiveRecord::default() };
        let plain = Value::Object(live.to_payload()).to_string();
        assert!(plain.ends_with(r#","od":true,"kf":true}"#), "{plain}");
        assert!(!plain.contains(r#""ll""#));
        assert!(!LiveRecord::from_payload(&live.to_payload(), &host).unwrap().low_latency);
        let offering = LiveRecord { low_latency: true, ..live.clone() };
        let text = Value::Object(offering.to_payload()).to_string();
        assert_eq!(text, format!(r#"{}{}"#, &plain[..plain.len() - 1], r#","ll":true}"#));
        assert_eq!(LiveRecord::from_payload(&offering.to_payload(), &host).unwrap(), offering);
        for value in ["false", "1", "\"no\"", "null"] {
            let odd = object(&format!(r#"{{"host":"{host}","ll":{value}}}"#));
            assert!(!LiveRecord::from_payload(&odd, &host).unwrap().low_latency, "{value}");
        }

        let (sid, viewer) = (keys::new_id(), keys::new_id());
        let proof = DeviceProof { key: dk.clone(), exchange: "E".into(), signature: "S".into() };
        let join = JoinRequest { session_id: sid, viewer_id: viewer, name: "n".into(), latency_ms: 300, overhead_percent: 25, remote_control: true, device: Some(proof), keyframes: true, ..JoinRequest::default() };
        let plain = Value::Object(join.to_payload()).to_string();
        assert!(plain.ends_with(r#","ds":"S","kf":true}"#), "{plain}");
        let asking = JoinRequest { low_latency: true, ..join.clone() };
        let text = Value::Object(asking.to_payload()).to_string();
        assert_eq!(text, format!(r#"{}{}"#, &plain[..plain.len() - 1], r#","ll":true}"#));
        assert_eq!(JoinRequest::from_payload(&asking.to_payload()), Some(asking));
        assert!(!JoinRequest::from_payload(&join.to_payload()).unwrap().low_latency);
    }

    #[test]
    fn mistyped_and_missing_fields_take_their_defaults() {
        let host = keys::new_id();
        let p = object(&format!(r#"{{"host":"{host}","t":"1758500000000","since":12.5,"mbps":"40","max":4.0,"viewers":null,"name":7,"codec":null,"open4":"yes","cands":"nope","extra":{{"a":1}}}}"#));
        let r = LiveRecord::from_payload(&p, &host).unwrap();
        assert_eq!((r.time_ms, r.since_ms, r.mbps, r.max_viewers, r.viewers), (1758500000000, 12, 40, 4, 0));
        assert_eq!(r.codec, "hevc");
        assert_eq!(r.open_ipv4, Some(true));
        assert!(r.candidates.is_empty());
        assert!(!r.name.is_empty(), "a missing name falls back like the C# apps");

        let empty = LiveRecord::from_payload(&Payload::new(), "").unwrap();
        assert_eq!((empty.time_ms, empty.mbps, empty.open_ipv4), (0, 0, None));
        assert_eq!(LiveRecord::from_payload(&Payload::new(), &host), None);
        // an int field outside the int range wraps the way the C# cast did
        let wrap = object(&format!(r#"{{"host":"{host}","mbps":4294967336}}"#));
        assert_eq!(LiveRecord::from_payload(&wrap, &host).unwrap().mbps, 40);
    }

    #[test]
    fn a_join_needs_real_ids_and_gets_sane_latency_and_overhead() {
        let (sid, viewer) = (keys::new_id(), keys::new_id());
        let join = |extra: &str| JoinRequest::from_payload(&object(&format!(r#"{{"sid":"{sid}","viewer":"{viewer}"{extra}}}"#)));
        let j = join("").unwrap();
        assert_eq!((j.latency_ms, j.overhead_percent, j.time_ms), (1500, 25, 0));
        assert_eq!(join(r#","latency":19,"overhead":4"#).map(|j| (j.latency_ms, j.overhead_percent)), Some((1500, 25)));
        assert_eq!(join(r#","latency":20,"overhead":5"#).map(|j| (j.latency_ms, j.overhead_percent)), Some((20, 5)));
        assert_eq!(join(r#","latency":30000,"overhead":100"#).map(|j| (j.latency_ms, j.overhead_percent)), Some((30000, 100)));
        assert_eq!(join(r#","latency":30001,"overhead":101"#).map(|j| (j.latency_ms, j.overhead_percent)), Some((1500, 25)));
        assert_eq!(JoinRequest::from_payload(&object(&format!(r#"{{"sid":"{sid}","viewer":"short"}}"#))), None);
        assert_eq!(JoinRequest::from_payload(&object(&format!(r#"{{"viewer":"{viewer}"}}"#))), None);
    }

    #[test]
    fn an_answer_must_name_its_session_and_ok_defaults_to_no() {
        let sid = keys::new_id();
        let a = JoinAnswer::from_payload(&object(&format!(r#"{{"sid":"{sid}","reason":"busy"}}"#)), &sid).unwrap();
        assert!(!a.ok);
        assert_eq!(a.reason, JoinAnswer::REASON_BUSY);
        assert!(JoinAnswer::from_payload(&object(&format!(r#"{{"sid":"{sid}","ok":"true"}}"#)), &sid).unwrap().ok);
        assert_eq!(JoinAnswer::from_payload(&object(&format!(r#"{{"sid":"{sid}","ok":true}}"#)), &keys::new_id()), None);
    }

    #[test]
    fn only_the_first_eight_usable_candidates_are_kept() {
        let mut items: Vec<Value> = vec![Value::from("bad"), Value::from(1), Value::Null];
        items.extend((0..12).map(|i| Value::from(format!("10.0.0.{i}:{}", 5000 + i))));
        let kept = candidates(Some(&Value::Array(items)));
        assert_eq!(kept.len(), MAX_CANDIDATES);
        assert_eq!(kept[0], "10.0.0.0:5000");
        assert_eq!(kept[7], "10.0.0.7:5007");
        assert!(candidates(None).is_empty());
        assert!(candidates(Some(&Value::from("1.2.3.4:5"))).is_empty());
        // kept as sent, even when the reader had to tidy it up to understand it
        assert_eq!(candidates(Some(&serde_json::json!([" １.2.3.4：５ "]))), vec![" １.2.3.4：５ ".to_string()]);
    }

    #[test]
    fn an_envelope_survives_json_and_rejects_oversize_or_unsigned_payloads() {
        let keys = keys();
        let (gid, sid) = (keys::new_id(), keys::new_id());
        let answer = JoinAnswer { session_id: sid.clone(), time_ms: 9, ok: true, ..JoinAnswer::default() };
        let env = SignedEnvelope::seal(&keys, KIND_ANSWER, &gid, &sid, &answer.to_payload());
        assert_eq!(env.to_json(), format!(r#"{{"p":"{}","m":"{}"}}"#, env.p, env.m));

        let listed = json::parse(&format!(r#"{{"seq":4,"p":"{}","m":"{}","age_ms":12}}"#, env.p, env.m)).unwrap();
        let back = SignedEnvelope::from_json_object(&listed).unwrap();
        assert_eq!(back, env);
        assert_eq!(JoinAnswer::from_payload(&back.try_open(&keys, KIND_ANSWER, &gid, &sid).unwrap(), &sid), Some(answer));

        assert_eq!(SignedEnvelope::from_json_object(&json::parse(r#"{"p":"x"}"#).unwrap()), None);
        assert_eq!(SignedEnvelope::from_json_object(&json::parse(r#"{"p":"x","m":5}"#).unwrap()), None);
        assert_eq!(SignedEnvelope::new("", env.m.clone()).try_open(&keys, KIND_ANSWER, &gid, &sid), None);

        // correctly signed, but not an object / not JSON / too long: still refused
        for body in ["[1,2]", "not json", &format!("{{\"pad\":\"{}\"}}", "x".repeat(13000))] {
            let p = base64url::encode(body.as_bytes());
            let m = base64url::encode(&mac(&keys, KIND_ANSWER, &gid, &sid, &p));
            assert_eq!(SignedEnvelope::new(p, m).try_open(&keys, KIND_ANSWER, &gid, &sid), None, "{}", &body[..8.min(body.len())]);
        }
    }

    #[test]
    fn the_auth_header_has_three_dot_separated_parts_and_a_fresh_nonce() {
        let a = request_auth::header_now(keys().server_key(), "GET", "/v1/g/x/live", b"");
        let b = request_auth::header_now(keys().server_key(), "GET", "/v1/g/x/live", b"");
        let parts: Vec<&str> = a.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].parse::<i64>().unwrap() > 1_700_000_000_000);
        assert_eq!(parts[1].len(), 11);
        assert_eq!(parts[2].len(), 43);
        assert_ne!(a.split('.').nth(1), b.split('.').nth(1));
    }
}
