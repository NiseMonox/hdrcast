//! The signaling HTTP API (docs/design.md §8.5). Every call authenticates with the group's server key; every
//! envelope that comes back is verified with the message key before anyone looks inside, so a compromised or
//! impersonated server can withhold information but cannot point a viewer somewhere else.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use hdrcast_core::config::NotConfiguredError;
use hdrcast_core::group::keys;
use hdrcast_core::json::{self, Value};
use hdrcast_core::messages::{self, request_auth};
use hdrcast_core::{base64url, GroupConfig, GroupKeys, LiveRecord, SignedEnvelope};

use crate::cancel::CancelToken;
use crate::http::{HttpClient, HttpFailure, HttpResponse};

/// Why a signaling call did not give an answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignalingError {
    /// The caller's token was cancelled. Nothing is wrong with the server.
    #[error("the signaling call was cancelled")]
    Cancelled,
    /// The call failed. `status` is the HTTP status the server answered with, or 0; `unreachable` = no HTTP
    /// answer at all (network, DNS, timeout).
    #[error("{message}")]
    Failed { message: String, status: u16, unreachable: bool },
}

impl SignalingError {
    fn failed(message: impl Into<String>, status: u16, unreachable: bool) -> SignalingError {
        SignalingError::Failed { message: message.into(), status, unreachable }
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, SignalingError::Cancelled)
    }

    /// The HTTP status of the server's refusal; 0 when there was none.
    pub fn status(&self) -> u16 {
        match self {
            SignalingError::Failed { status, .. } => *status,
            SignalingError::Cancelled => 0,
        }
    }

    /// No HTTP answer at all (network, DNS, timeout).
    pub fn is_unreachable(&self) -> bool {
        matches!(self, SignalingError::Failed { unreachable: true, .. })
    }
}

/// A verified entry of the live list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveEntry {
    pub record: LiveRecord,
    /// How long ago the server stored the record (its heartbeat age).
    pub age_ms: i64,
}

impl LiveEntry {
    /// This host speaks the side channel and offers remote control (docs/design.md §8.9).
    pub fn remote_control(&self) -> bool {
        self.record.remote_control == Some(true)
    }
}

pub const DEFAULT_TIMEOUT_MS: u64 = 8000;
pub const MAX_WAIT_SECONDS: u32 = 25;

/// Largest body we accept: 32 live hosts or 32 queued joins at up to 8 KB each stay well below it; anything
/// bigger is not our server.
const MAX_BODY_BYTES: u64 = 512 * 1024;

/// May be shared between threads: a long-poll, a heartbeat and a live-list refresh run side by side.
pub struct SignalingClient {
    base: String,
    group_id: String,
    keys: GroupKeys,
    timeout_ms: AtomicU64,
    http: Arc<HttpClient>,
}

struct Response {
    status: u16,
    body: String,
}

impl SignalingClient {
    pub fn new(base_url: &str, group_id: &str, keys: &GroupKeys, timeout_ms: u64) -> SignalingClient {
        SignalingClient {
            base: base_url.trim_end_matches('/').to_string(),
            group_id: group_id.to_string(),
            keys: keys.clone(),
            timeout_ms: AtomicU64::new(timeout_ms),
            http: Arc::new(HttpClient::new("HdrCast/1", Some("application/json"), true)),
        }
    }

    /// The client for `group`'s signaling server ([`DEFAULT_TIMEOUT_MS`]).
    pub fn for_group(group: &GroupConfig) -> Result<SignalingClient, NotConfiguredError> {
        Ok(SignalingClient::new(&group.signal_url, &group.group_id, &group.keys()?, DEFAULT_TIMEOUT_MS))
    }

    pub fn group_id(&self) -> &str {
        &self.group_id
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// How long a call may take (a long-poll: this plus its wait).
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms.load(Ordering::Relaxed)
    }

    pub fn set_timeout_ms(&self, timeout_ms: u64) {
        self.timeout_ms.store(timeout_ms, Ordering::Relaxed);
    }

    /// Host name or address of the signaling server (the UDP echo lives there too); "" when the address is
    /// not a URL.
    pub fn host(&self) -> String {
        crate::http::HttpUrl::parse(&self.base).map(|url| url.host().to_string()).unwrap_or_default()
    }

    /// True when the server answers its health check. False also when cancelled.
    pub fn health(&self, ct: &CancelToken) -> bool {
        self.send("GET", "/v1/health", None, false, self.timeout_ms(), ct).is_ok()
    }

    /// Registers the group (first come); 200/201 are both success, 409 means the id is taken.
    pub fn register_group(&self, ct: &CancelToken) -> Result<(), SignalingError> {
        let mut body = json::Map::new();
        body.insert("serverKey".to_string(), Value::from(base64url::encode(self.keys.server_key())));
        let path = format!("/v1/g/{}", self.group_id);
        self.send("PUT", &path, Some(json::to_string(&Value::Object(body))), true, self.timeout_ms(), ct).map(drop)
    }

    pub fn put_live(&self, host_id: &str, envelope: &SignedEnvelope, ct: &CancelToken) -> Result<(), SignalingError> {
        let path = format!("/v1/g/{}/live/{host_id}", self.group_id);
        self.send("PUT", &path, Some(envelope.to_json()), true, self.timeout_ms(), ct).map(drop)
    }

    pub fn delete_live(&self, host_id: &str, ct: &CancelToken) -> Result<(), SignalingError> {
        let path = format!("/v1/g/{}/live/{host_id}", self.group_id);
        self.send("DELETE", &path, None, true, self.timeout_ms(), ct).map(drop)
    }

    /// The live list, keeping only records with a valid signature.
    pub fn get_live(&self, ct: &CancelToken) -> Result<Vec<LiveEntry>, SignalingError> {
        let path = format!("/v1/g/{}/live", self.group_id);
        let root = parse(&self.send("GET", &path, None, true, self.timeout_ms(), ct)?.body)?;
        let mut result = Vec::new();
        let Some(Value::Array(hosts)) = json::get(&root, "hosts") else { return Ok(result) };
        for host in hosts {
            let (Some(id), Some(envelope)) = (json::get_str(host, "host"), SignedEnvelope::from_json_object(host)) else { continue };
            if !keys::is_valid_id(id) {
                continue;
            }
            let Some(payload) = envelope.try_open(&self.keys, messages::KIND_LIVE, &self.group_id, id) else { continue };
            if let Some(record) = LiveRecord::from_payload(&payload, id) {
                result.push(LiveEntry { record, age_ms: json::get(host, "age_ms").and_then(json::as_i64).unwrap_or(0) });
            }
        }
        Ok(result)
    }

    /// Drops a join request in the host's mailbox and returns its sequence number; fails with status 404 when
    /// the host is not live.
    pub fn post_join(&self, host_id: &str, envelope: &SignedEnvelope, ct: &CancelToken) -> Result<i64, SignalingError> {
        let path = format!("/v1/g/{}/live/{host_id}/join", self.group_id);
        let root = parse(&self.send("POST", &path, Some(envelope.to_json()), true, self.timeout_ms(), ct)?.body)?;
        Ok(json::get(&root, "seq").and_then(json::as_i64).unwrap_or(0))
    }

    /// Long-polls the host's mailbox for joins after `after`; returns (seq, envelope) pairs and the next cursor.
    pub fn poll_joins(&self, host_id: &str, after: i64, wait_seconds: u32, ct: &CancelToken) -> Result<(Vec<(i64, SignedEnvelope)>, i64), SignalingError> {
        let wait = wait_seconds.min(MAX_WAIT_SECONDS);
        let path = format!("/v1/g/{}/live/{host_id}/joins?after={after}&wait={wait}", self.group_id);
        let root = parse(&self.send("GET", &path, None, true, self.timeout_ms() + u64::from(wait) * 1000, ct)?.body)?;
        let mut next = json::get(&root, "next").and_then(json::as_i64).unwrap_or(after);
        let mut list = Vec::new();
        let Some(Value::Array(joins)) = json::get(&root, "joins") else { return Ok((list, next)) };
        for join in joins {
            let seq = json::get(join, "seq").and_then(json::as_i64).unwrap_or(0);
            if let Some(envelope) = SignedEnvelope::from_json_object(join) {
                list.push((seq, envelope));
            }
            next = next.max(seq);
        }
        Ok((list, next))
    }

    pub fn put_answer(&self, sid: &str, envelope: &SignedEnvelope, ct: &CancelToken) -> Result<(), SignalingError> {
        let path = format!("/v1/g/{}/answer/{sid}", self.group_id);
        self.send("PUT", &path, Some(envelope.to_json()), true, self.timeout_ms(), ct).map(drop)
    }

    /// Long-polls for the host's answer; `None` when none arrived within `wait_seconds` (or what arrived is
    /// not an envelope).
    pub fn wait_answer(&self, sid: &str, wait_seconds: u32, ct: &CancelToken) -> Result<Option<SignedEnvelope>, SignalingError> {
        let wait = wait_seconds.min(MAX_WAIT_SECONDS);
        let path = format!("/v1/g/{}/answer/{sid}?wait={wait}", self.group_id);
        let response = self.send("GET", &path, None, true, self.timeout_ms() + u64::from(wait) * 1000, ct)?;
        if response.status == 204 {
            return Ok(None);
        }
        Ok(SignedEnvelope::from_json_object(&parse(&response.body)?))
    }

    /// One call. The request itself runs on a thread of its own, because nothing can interrupt a blocking HTTP
    /// call and a stop must not sit out a 20 s long-poll: when `ct` is cancelled this returns at once and the
    /// request is left to finish (or time out) by itself, its answer dropped.
    fn send(&self, method: &'static str, path_and_query: &str, body: Option<String>, authenticate: bool, timeout_ms: u64, ct: &CancelToken) -> Result<Response, SignalingError> {
        if ct.is_cancelled() {
            return Err(SignalingError::Cancelled);
        }
        if crate::http::HttpUrl::parse(&self.base).is_none() {
            return Err(SignalingError::failed(format!("bad signaling address: {}", self.base), 0, true));
        }
        let url = format!("{}{path_and_query}", self.base);
        let has_body = matches!(method, "PUT" | "POST");
        let bytes = body.unwrap_or_default().into_bytes();
        let auth = authenticate.then(|| request_auth::header_now(self.keys.server_key(), method, path_and_query, &bytes));
        let timeout = Duration::from_millis(timeout_ms);

        enum Event {
            Done(Result<HttpResponse, HttpFailure>),
            Cancelled,
        }
        let (tx, rx) = mpsc::channel();
        let _on_cancel = {
            let tx = tx.clone();
            ct.on_cancel(move || {
                let _ = tx.send(Event::Cancelled);
            })
        };
        let http = self.http.clone();
        let spawned = thread::Builder::new().name("signal-http".to_string()).spawn(move || {
            let mut headers: Vec<(&str, &str)> = Vec::with_capacity(2);
            if let Some(auth) = &auth {
                headers.push((request_auth::HEADER_NAME, auth));
            }
            if has_body {
                headers.push(("Content-Type", "application/json"));
            }
            let result = http.send(method, &url, &headers, has_body.then_some(bytes.as_slice()), timeout, MAX_BODY_BYTES);
            let _ = tx.send(Event::Done(result)); // nobody listens any more when the call was given up
        });
        if spawned.is_err() {
            return Err(SignalingError::failed("signaling connection failed", 0, true));
        }

        // A little longer than the request's own limit, so that it is the request that reports its timeout.
        let response = match rx.recv_timeout(timeout + Duration::from_millis(500)) {
            Ok(Event::Cancelled) => return Err(SignalingError::Cancelled),
            Ok(Event::Done(Ok(response))) => response,
            Ok(Event::Done(Err(failure))) => {
                if ct.is_cancelled() {
                    return Err(SignalingError::Cancelled);
                }
                return Err(match failure {
                    HttpFailure::BadUrl => SignalingError::failed(format!("bad signaling address: {}", self.base), 0, true),
                    HttpFailure::Timeout => SignalingError::failed("signaling server did not answer in time", 0, true),
                    HttpFailure::NoAnswer(why) => SignalingError::failed(format!("signaling server unreachable ({why})"), 0, true),
                    HttpFailure::Broken => SignalingError::failed("signaling connection failed", 0, true),
                });
            }
            Err(_) => return Err(SignalingError::failed("signaling server did not answer in time", 0, true)),
        };

        if !response.is_success() {
            let error = json::try_parse(&response.body)
                .as_ref()
                .and_then(|root| json::get_str(root, "error"))
                .map_or_else(|| response.reason.to_string(), str::to_string);
            return Err(SignalingError::failed(format!("signaling {}: {error}", response.status), response.status, false));
        }
        Ok(Response { status: response.status, body: response.body })
    }
}

impl std::fmt::Debug for SignalingClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalingClient").field("base", &self.base).field("group_id", &self.group_id).finish()
    }
}

fn parse(text: &str) -> Result<Value, SignalingError> {
    json::parse(text).map_err(|_| SignalingError::failed("signaling sent something that is not JSON", 0, false))
}
