//! This computer's device key (docs/design.md §8.10).
//!
//! Everyone in a group holds the group key, so whatever is signed with it (a peer id, a name) could have been
//! written by any member. What a host grants one particular computer (connecting without asking, remote control
//! as soon as it connects) is therefore tied to a key only that computer has: an Ed25519 key pair made on first
//! use and kept in `device.json`. Joins and answers carry the public key and a signature over the session. The
//! key remote control seals its datagrams with comes from an X25519 exchange inside those signed messages, so
//! nobody else in the group can inject input into a session a host trusts, or read what a viewer types.
//!
//! People compare devices by their [`code`](DeviceKey::code): twelve characters of the key's SHA-256 in
//! Crockford base32, "7KQ2-M9XD-4TPA".

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde_json::Value;
use x25519_dalek::{x25519, X25519_BASEPOINT_BYTES};
use zeroize::Zeroize;

use super::keys;
use super::messages::{self, DeviceProof, JoinAnswer, JoinRequest};
use crate::common::{app_paths, base64url, json};

pub const FILE_NAME: &str = "device.json";
pub const KEY_LENGTH: usize = 32;
pub const SIGNATURE_LENGTH: usize = 64;

/// Crockford's base32: no I, L, O or U, so a code read aloud or retyped has nothing to confuse.
const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// This computer's signing key.
pub struct DeviceKey {
    signing: SigningKey,
    /// The public key, base64url (what goes into messages as "dk").
    public: String,
}

impl DeviceKey {
    pub fn generate() -> DeviceKey {
        let mut seed = [0u8; KEY_LENGTH];
        keys::fill_random(&mut seed);
        let key = DeviceKey::from_seed(&seed);
        seed.zeroize();
        key
    }

    pub fn from_seed(seed: &[u8; KEY_LENGTH]) -> DeviceKey {
        let signing = SigningKey::from_bytes(seed);
        let public = base64url::encode(signing.verifying_key().as_bytes());
        DeviceKey { signing, public }
    }

    /// The public key as messages carry it ("dk").
    pub fn public_key(&self) -> &str {
        &self.public
    }

    /// What people compare: "7KQ2-M9XD-4TPA".
    pub fn code(&self) -> String {
        code_of_bytes(self.signing.verifying_key().as_bytes())
    }

    /// The signature of `message`, base64url (what goes into messages as "ds").
    pub fn sign(&self, message: &[u8]) -> String {
        base64url::encode(&self.signing.sign(message).to_bytes())
    }

    pub fn default_path() -> PathBuf {
        app_paths::settings_file(FILE_NAME)
    }

    /// The key kept in `path`, or a new one, saved there, when the file is missing or unusable. A new key that
    /// could not be saved is still returned, with the reason: it works until the app ends, but hosts that trusted
    /// it will not recognise this computer after the next start.
    pub fn load_or_create(path: &Path) -> (DeviceKey, Option<io::Error>) {
        if let Some(key) = DeviceKey::load(path) {
            return (key, None);
        }
        let key = DeviceKey::generate();
        let error = key.save(path).err();
        (key, error)
    }

    /// `None` when the file is missing or does not hold a key.
    pub fn load(path: &Path) -> Option<DeviceKey> {
        let value = json::load_file_value(path)?;
        let mut raw = base64url::decode(json::get_str(&value, "key")?)?;
        let key = <[u8; KEY_LENGTH]>::try_from(raw.as_slice()).ok().map(|seed| DeviceKey::from_seed(&seed));
        raw.zeroize();
        key
    }

    /// Writes the private key to `path` (atomically). The file is as secret as `group.json`.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut secret = base64url::encode(self.signing.as_bytes());
        let mut text = serde_json::to_string_pretty(&serde_json::json!({
            "v": 1,
            "note": "This computer's private key for HdrCast. Keep it to yourself: whoever has it can connect as this computer.",
            "key": secret,
            "public": self.public,
        }))
        .map_err(io::Error::other)?;
        secret.zeroize();
        let result = json::write_file_atomic(path, text.as_bytes());
        text.zeroize();
        result
    }
}

/// `{:?}` never shows the private key.
impl fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceKey({})", self.code())
    }
}

/// A key pair for one session's X25519 exchange. The private half never leaves memory and is wiped on drop.
pub struct Exchange {
    secret: [u8; KEY_LENGTH],
    public: String,
}

impl Exchange {
    pub fn new() -> Exchange {
        let mut secret = [0u8; KEY_LENGTH];
        keys::fill_random(&mut secret);
        let public = base64url::encode(&x25519(secret, X25519_BASEPOINT_BYTES));
        Exchange { secret, public }
    }

    /// The public half as messages carry it ("ek").
    pub fn public_key(&self) -> &str {
        &self.public
    }

    /// The secret both sides now share; `None` when `peer` is not a usable key (one that would make the result
    /// predictable: all zeros).
    pub fn agree(&self, peer: &[u8; KEY_LENGTH]) -> Option<[u8; KEY_LENGTH]> {
        let shared = x25519(self.secret, *peer);
        (!keys::fixed_time_eq(&shared, &[0u8; KEY_LENGTH])).then_some(shared)
    }
}

impl Default for Exchange {
    fn default() -> Self {
        Exchange::new()
    }
}

impl Drop for Exchange {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

impl fmt::Debug for Exchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Exchange({})", self.public)
    }
}

/// The remote-control key of a session whose join and answer carried exchange keys (docs/design.md §8.10).
/// Replaces the group-derived `GroupKeys::control_key` for such a session.
pub fn session_control_key(shared: &[u8; KEY_LENGTH], sid: &str, viewer_exchange: &str, host_exchange: &str) -> [u8; 32] {
    keys::hmac_sha256(shared, format!("hdrcast/v1/ctl-dh\n{sid}\n{viewer_exchange}\n{host_exchange}").as_bytes())
}

/// The 32 bytes of a device key in its one canonical base64url spelling, when they are a usable Ed25519 key.
pub fn parse_public_key(text: &str) -> Option<[u8; KEY_LENGTH]> {
    let bytes = canonical_bytes::<KEY_LENGTH>(text)?;
    let key = VerifyingKey::from_bytes(&bytes).ok()?;
    (!key.is_weak()).then_some(bytes)
}

/// The 32 bytes of an exchange key in its one canonical base64url spelling.
pub fn parse_exchange_key(text: &str) -> Option<[u8; KEY_LENGTH]> {
    canonical_bytes::<KEY_LENGTH>(text)
}

fn canonical_bytes<const N: usize>(text: &str) -> Option<[u8; N]> {
    let bytes: [u8; N] = base64url::decode(text)?.try_into().ok()?;
    // One spelling per key: base64url's spare bits and padding would otherwise let the same key be written
    // several ways, and the rules a host keeps compare the text.
    (base64url::encode(&bytes) == text).then_some(bytes)
}

/// The code people compare for the device key `text` ("" when it is not a key).
pub fn code_of(text: &str) -> String {
    parse_public_key(text).map(|bytes| code_of_bytes(&bytes)).unwrap_or_default()
}

fn code_of_bytes(public: &[u8; KEY_LENGTH]) -> String {
    let digest = keys::sha256(public);
    let bits = u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"));
    let mut code = String::with_capacity(14);
    for i in 0..12 {
        if i > 0 && i % 4 == 0 {
            code.push('-');
        }
        code.push(CODE_ALPHABET[((bits >> (59 - 5 * i)) & 31) as usize] as char);
    }
    code
}

fn verify(public: &str, message: &str, signature: &str) -> bool {
    let (Some(key), Some(signature)) = (parse_public_key(public), canonical_bytes::<SIGNATURE_LENGTH>(signature)) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&key) else { return false };
    key.verify_strict(message.as_bytes(), &Signature::from_bytes(&signature)).is_ok()
}

/// The candidates as JSON, exactly as a reader of the payload ends up with them.
fn candidates_text(candidates: &[String]) -> String {
    Value::from(candidates.to_vec()).to_string()
}

/// What a viewer signs in its join: the device and exchange keys, bound to this group, host, session, moment and
/// the addresses the host is to punch towards.
pub fn join_statement(gid: &str, host: &str, join: &JoinRequest, proof: &DeviceProof) -> String {
    format!(
        "hdrcast/v1/join-device\n{gid}\n{host}\n{}\n{}\n{}\n{}\n{}\n{}",
        join.session_id,
        join.viewer_id,
        join.time_ms,
        candidates_text(&join.candidates),
        proof.key,
        proof.exchange
    )
}

/// What a host signs in its answer: the verdict and its exchange key, bound to the viewer's exchange key.
pub fn answer_statement(gid: &str, viewer_exchange: &str, answer: &JoinAnswer, proof: &DeviceProof) -> String {
    format!(
        "hdrcast/v1/answer-device\n{gid}\n{}\n{}\n{}\n{}\n{}\n{viewer_exchange}\n{}\n{}",
        answer.session_id,
        answer.time_ms,
        answer.ok,
        answer.reason,
        candidates_text(&answer.candidates),
        proof.key,
        proof.exchange
    )
}

/// Signs `join` for the host `host`: sets its "dk", "ek" and "ds". The candidates are first tidied the way the
/// host will read them, so both sides sign and check the same text.
pub fn sign_join(device: &DeviceKey, exchange: &Exchange, gid: &str, host: &str, join: &mut JoinRequest) {
    join.candidates = messages::candidates(Some(&Value::from(join.candidates.clone())));
    let mut proof = DeviceProof { key: device.public_key().to_string(), exchange: exchange.public_key().to_string(), signature: String::new() };
    proof.signature = device.sign(join_statement(gid, host, join, &proof).as_bytes());
    join.device = Some(proof);
}

/// A join whose device signature holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDevice {
    /// The device key, base64url (canonical).
    pub key: String,
    /// The viewer's exchange key for this session, base64url.
    pub exchange: String,
}

/// The device behind `join` when its signature is valid for this group and host; `None` for a join without
/// one (an older viewer) and for one whose signature does not hold.
pub fn verify_join(gid: &str, host: &str, join: &JoinRequest) -> Option<VerifiedDevice> {
    let proof = join.device.as_ref()?;
    parse_exchange_key(&proof.exchange)?;
    verify(&proof.key, &join_statement(gid, host, join, proof), &proof.signature)
        .then(|| VerifiedDevice { key: proof.key.clone(), exchange: proof.exchange.clone() })
}

/// Signs `answer` to a join whose exchange key was `viewer_exchange` ("" when the join had none): sets "dk",
/// "ek" (when `exchange` is given) and "ds".
pub fn sign_answer(device: &DeviceKey, exchange: Option<&Exchange>, gid: &str, viewer_exchange: &str, answer: &mut JoinAnswer) {
    answer.candidates = messages::candidates(Some(&Value::from(answer.candidates.clone())));
    let mut proof = DeviceProof {
        key: device.public_key().to_string(),
        exchange: exchange.map(|e| e.public_key().to_string()).unwrap_or_default(),
        signature: String::new(),
    };
    proof.signature = device.sign(answer_statement(gid, viewer_exchange, answer, &proof).as_bytes());
    answer.device = Some(proof);
}

/// True when `answer` was signed by the device `host_key` for the viewer whose exchange key was
/// `viewer_exchange`.
pub fn verify_answer(gid: &str, host_key: &str, viewer_exchange: &str, answer: &JoinAnswer) -> bool {
    let Some(proof) = answer.device.as_ref() else { return false };
    proof.key == host_key
        && (proof.exchange.is_empty() || parse_exchange_key(&proof.exchange).is_some())
        && verify(&proof.key, &answer_statement(gid, viewer_exchange, answer, proof), &proof.signature)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(viewer: &str) -> JoinRequest {
        JoinRequest {
            session_id: keys::new_id(),
            viewer_id: viewer.to_string(),
            name: "laptop".to_string(),
            time_ms: 1_760_000_000_000,
            candidates: vec!["[2400::1]:5000".to_string(), "192.168.1.9:5001".to_string()],
            latency_ms: 300,
            overhead_percent: 25,
            remote_control: true,
            device: None,
        }
    }

    #[test]
    fn a_signed_join_verifies_only_for_its_group_host_and_content() {
        let device = DeviceKey::generate();
        let exchange = Exchange::new();
        let (gid, host, viewer) = (keys::new_id(), keys::new_id(), keys::new_id());
        let mut signed = join(&viewer);
        sign_join(&device, &exchange, &gid, &host, &mut signed);

        // through the wire format and back
        let read = JoinRequest::from_payload(&signed.to_payload()).unwrap();
        assert_eq!(read, signed);
        let verified = verify_join(&gid, &host, &read).unwrap();
        assert_eq!(verified.key, device.public_key());
        assert_eq!(verified.exchange, exchange.public_key());

        assert_eq!(verify_join(&keys::new_id(), &host, &read), None, "another group");
        assert_eq!(verify_join(&gid, &keys::new_id(), &read), None, "another host");
        for tampered in [
            JoinRequest { session_id: keys::new_id(), ..read.clone() },
            JoinRequest { viewer_id: keys::new_id(), ..read.clone() },
            JoinRequest { time_ms: read.time_ms + 1, ..read.clone() },
            JoinRequest { candidates: vec!["203.0.113.5:9".to_string()], ..read.clone() },
        ] {
            assert_eq!(verify_join(&gid, &host, &tampered), None);
        }
        // someone else's key with this device's signature, and this key with someone else's exchange key
        let other = DeviceKey::generate();
        let mut swapped = read.clone();
        swapped.device.as_mut().unwrap().key = other.public_key().to_string();
        assert_eq!(verify_join(&gid, &host, &swapped), None);
        let mut swapped = read.clone();
        swapped.device.as_mut().unwrap().exchange = Exchange::new().public_key().to_string();
        assert_eq!(verify_join(&gid, &host, &swapped), None);

        // what the name, the latency and the remote-control flag say is not part of who the device is
        let relaxed = JoinRequest { name: "renamed".into(), latency_ms: 3500, remote_control: false, ..read.clone() };
        assert!(verify_join(&gid, &host, &relaxed).is_some());
        // an older viewer's join has no device at all
        assert_eq!(verify_join(&gid, &host, &join(&viewer)), None);
    }

    #[test]
    fn a_join_without_an_exchange_key_is_not_a_device_join() {
        let device = DeviceKey::generate();
        let (gid, host) = (keys::new_id(), keys::new_id());
        let mut signed = join(&keys::new_id());
        sign_join(&device, &Exchange::new(), &gid, &host, &mut signed);
        let mut payload = signed.to_payload();
        payload.remove("ek");
        let read = JoinRequest::from_payload(&payload).unwrap();
        assert_eq!(read.device, None, "dk and ds alone do not make a device join");
        assert_eq!(verify_join(&gid, &host, &read), None);
    }

    #[test]
    fn answers_are_signed_for_one_viewer_and_both_sides_agree_on_the_control_key() {
        let (host_device, gid) = (DeviceKey::generate(), keys::new_id());
        let (viewer_exchange, host_exchange) = (Exchange::new(), Exchange::new());
        let sid = keys::new_id();
        let mut answer = JoinAnswer {
            session_id: sid.clone(),
            time_ms: 5,
            ok: true,
            reason: String::new(),
            candidates: vec!["[2400::2]:6000".to_string()],
            device: None,
        };
        sign_answer(&host_device, Some(&host_exchange), &gid, viewer_exchange.public_key(), &mut answer);
        let read = JoinAnswer::from_payload(&answer.to_payload(), &sid).unwrap();
        assert!(verify_answer(&gid, host_device.public_key(), viewer_exchange.public_key(), &read));
        assert!(!verify_answer(&gid, DeviceKey::generate().public_key(), viewer_exchange.public_key(), &read), "signed by another device");
        assert!(!verify_answer(&gid, host_device.public_key(), Exchange::new().public_key(), &read), "meant for another viewer");
        assert!(!verify_answer(&keys::new_id(), host_device.public_key(), viewer_exchange.public_key(), &read));
        let redirected = JoinAnswer { candidates: vec!["198.51.100.7:1".to_string()], ..read.clone() };
        assert!(!verify_answer(&gid, host_device.public_key(), viewer_exchange.public_key(), &redirected));
        let unsigned = JoinAnswer { device: None, ..read.clone() };
        assert!(!verify_answer(&gid, host_device.public_key(), viewer_exchange.public_key(), &unsigned));

        let host_ek = parse_exchange_key(&read.device.as_ref().unwrap().exchange).unwrap();
        let viewer_ek = parse_exchange_key(viewer_exchange.public_key()).unwrap();
        let viewer_shared = viewer_exchange.agree(&host_ek).unwrap();
        let host_shared = host_exchange.agree(&viewer_ek).unwrap();
        assert_eq!(viewer_shared, host_shared);
        let (a, b) = (viewer_exchange.public_key(), host_exchange.public_key());
        assert_eq!(session_control_key(&viewer_shared, &sid, a, b), session_control_key(&host_shared, &sid, a, b));
        assert_ne!(session_control_key(&viewer_shared, &sid, a, b), session_control_key(&viewer_shared, &keys::new_id(), a, b));
        assert_eq!(viewer_exchange.agree(&[0u8; 32]), None, "a key that would make the secret predictable");
    }

    #[test]
    fn refusals_are_signed_without_an_exchange_key() {
        let (host_device, gid, sid) = (DeviceKey::generate(), keys::new_id(), keys::new_id());
        let viewer_exchange = Exchange::new();
        let mut answer = JoinAnswer { session_id: sid.clone(), time_ms: 9, ok: false, reason: JoinAnswer::REASON_DENIED.into(), ..JoinAnswer::default() };
        sign_answer(&host_device, None, &gid, viewer_exchange.public_key(), &mut answer);
        let text = Value::Object(answer.to_payload()).to_string();
        assert!(!text.contains(r#""ek""#), "{text}");
        let read = JoinAnswer::from_payload(&answer.to_payload(), &sid).unwrap();
        assert!(verify_answer(&gid, host_device.public_key(), viewer_exchange.public_key(), &read));
        let changed = JoinAnswer { ok: true, ..read.clone() };
        assert!(!verify_answer(&gid, host_device.public_key(), viewer_exchange.public_key(), &changed));
    }

    #[test]
    fn keys_have_one_spelling_and_a_stable_code() {
        let device = DeviceKey::from_seed(&[7u8; 32]);
        let public = device.public_key().to_string();
        assert_eq!(parse_public_key(&public).map(|b| base64url::encode(&b)), Some(public.clone()));
        assert_eq!(public.len(), 43);
        // the same 32 bytes with a spare bit set, or with padding: refused
        const ALPHABET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let last = ALPHABET.find(public.chars().last().unwrap()).unwrap();
        let respelled = format!("{}{}", &public[..42], ALPHABET.as_bytes()[last ^ 1] as char);
        assert_eq!(base64url::decode(&respelled), base64url::decode(&public));
        assert_eq!(parse_public_key(&respelled), None);
        assert_eq!(parse_public_key(&format!("{public}=")), None);
        assert_eq!(parse_public_key("short"), None);
        assert_eq!(parse_public_key(&base64url::encode(&[0u8; 32])).is_some(), false, "the identity point is a weak key");

        let code = device.code();
        assert_eq!(code.len(), 14);
        assert_eq!(code, code_of(&public));
        assert_eq!(code.matches('-').count(), 2);
        assert!(code.chars().all(|c| c == '-' || CODE_ALPHABET.contains(&(c as u8))));
        assert_ne!(code, DeviceKey::from_seed(&[8u8; 32]).code());
        assert_eq!(code_of("not a key"), "");
        assert!(!format!("{device:?}").contains(&base64url::encode(&[7u8; 32])));
    }

    #[test]
    fn the_key_is_kept_across_starts_and_a_broken_file_gets_a_new_one() {
        let dir = std::env::temp_dir().join(format!("hdrcast-device-{}", keys::new_id()));
        let path = dir.join(FILE_NAME);
        let (first, error) = DeviceKey::load_or_create(&path);
        assert!(error.is_none());
        let (again, _) = DeviceKey::load_or_create(&path);
        assert_eq!(first.public_key(), again.public_key());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(first.public_key()));

        std::fs::write(&path, "{\"v\":1,\"key\":\"AAAA\"}").unwrap();
        let (fresh, _) = DeviceKey::load_or_create(&path);
        assert_ne!(fresh.public_key(), first.public_key());
        assert_eq!(DeviceKey::load(&path).unwrap().public_key(), fresh.public_key());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
