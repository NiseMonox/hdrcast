//! JSON as the C# apps read and wrote it: `serde_json` underneath, with the forgiving accessors of the old
//! MiniJson on top (mpv JSON IPC, invites, signaling) and the atomic file save of the old `Json` helper.
//!
//! What protocol v1 relies on, and what is kept:
//! - objects keep their key order ([`Map`] is insertion-ordered: this crate turns on serde_json's
//!   `preserve_order`), so a payload is written in the order the reference vectors pin;
//! - the compact writer escapes exactly what MiniJson escaped (`"`, `\`, control characters; `/` and non-ASCII
//!   are written raw), so the bytes are the same;
//! - a duplicate key keeps its last value; unknown keys are simply never asked for;
//! - a number may arrive as an integer, a float or a string and still be read ([`as_i64`] and friends), because
//!   that is what 0.2.4 accepts from its peers.
//!
//! What is stricter than MiniJson, on input no HdrCast, Python or mpv ever produces: raw control characters
//! inside strings, lone surrogate escapes, numbers with leading zeros or a trailing '.', nesting deeper than
//! 128 levels, and a bare number or literal followed by junk are parse errors here. An integer beyond the
//! 64-bit range saturates instead of wrapping. tests/csharp_answers_tests.rs has the cases.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;

pub use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("JSON: {0}")]
pub struct JsonError(String);

/// Parses the first JSON value in `text`. Tolerant of surrounding whitespace and, as MiniJson was, of trailing
/// garbage after an object, array or string.
pub fn parse(text: &str) -> Result<Value, JsonError> {
    match serde_json::Deserializer::from_str(text).into_iter::<Value>().next() {
        Some(Ok(value)) => Ok(value),
        Some(Err(e)) => Err(JsonError(e.to_string())),
        None => Err(JsonError("unexpected end".to_string())),
    }
}

pub fn try_parse(text: &str) -> Option<Value> {
    parse(text).ok()
}

/// Compact JSON (no spaces), keys in the map's own order.
pub fn to_string(value: &Value) -> String {
    value.to_string()
}

// ---- typed accessors ----------------------------------------------------------------------

/// The member `key` of an object; `None` when `obj` is not an object, the key is missing, or its value is null
/// (MiniJson could not tell those apart, and nothing needs to).
pub fn get<'a>(obj: &'a Value, key: &str) -> Option<&'a Value> {
    obj.as_object()?.get(key).filter(|v| !v.is_null())
}

/// A string member; numbers and booleans are not turned into strings.
pub fn get_str<'a>(obj: &'a Value, key: &str) -> Option<&'a str> {
    get(obj, key)?.as_str()
}

/// A number, or a string holding one ("2.5"): mpv reports some properties as text.
pub fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => parse_f64(s),
        _ => None,
    }
}

/// An integer; a float is rounded half-to-even (as `Math.Round` did), a string holding an integer is parsed.
/// Values beyond the i64 range saturate.
pub fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => match n.as_i64() {
            Some(i) => Some(i),
            None => n.as_f64().filter(|d| d.is_finite()).map(|d| d.round_ties_even() as i64),
        },
        Value::String(s) => parse_i64(s),
        _ => None,
    }
}

/// true / false, or the strings mpv uses for flags: "yes" / "no" / "true" / "false".
pub fn as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.as_str() {
            "yes" | "true" => Some(true),
            "no" | "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// `long.TryParse(s, NumberStyles.Integer)`: optional surrounding ASCII whitespace, optional sign, digits.
fn parse_i64(s: &str) -> Option<i64> {
    trim_number(s).parse().ok()
}

/// `double.TryParse(s, NumberStyles.Float)`: optional surrounding whitespace, sign, digits, '.', exponent.
fn parse_f64(s: &str) -> Option<f64> {
    let t = trim_number(s);
    match t {
        "NaN" => return Some(f64::NAN),
        "Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    // Rust's parser also takes "inf", "nan" and "infinity" in any case; .NET's did not.
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-')) {
        return None;
    }
    t.parse::<f64>().ok().filter(|d| d.is_finite())
}

fn trim_number(s: &str) -> &str {
    s.trim_matches(|c: char| matches!(c, '\t'..='\r' | ' '))
}

// ---- files --------------------------------------------------------------------------------

/// Writes `value` as indented JSON, atomically (unique temp file + replace), creating the directory when needed.
/// The temp name is unique because several HdrCast processes (this app, and a C# 0.2.4 Sender or Receiver next
/// to it) may save the shared group.json at the same moment.
pub fn save_file<T: Serialize + ?Sized>(path: &Path, value: &T) -> io::Result<()> {
    let text = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
    write_file_atomic(path, text.as_bytes())
}

/// `None` when the file is missing, unreadable or not what `T` expects (callers fall back to defaults).
pub fn load_file<T: DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_str(&load_file_text(path)?).ok()
}

/// The file's JSON as a [`Value`]; `None` when missing, unreadable or not JSON.
pub fn load_file_value(path: &Path) -> Option<Value> {
    serde_json::from_str(&load_file_text(path)?).ok()
}

/// The file as text: UTF-8, a byte order mark dropped (Notepad adds one), invalid bytes replaced.
/// `None` when the file is missing or unreadable.
pub fn load_file_text(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Replaces `path` with `bytes` in one step: a reader sees the old file or the new one, never half of either,
/// and a crash mid-write leaves the old file in place.
pub fn write_file_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let tmp = temp_path(path)?;
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        // The rename is only as durable as the data behind it; a failed flush is not worth failing the save for.
        let _ = file.sync_all();
        drop(file);
        replace(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// `<path>.<8 hex digits>.tmp`, next to the target so the rename never crosses a volume.
fn temp_path(path: &Path) -> io::Result<PathBuf> {
    let mut random = [0u8; 4];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{:08x}.tmp", u32::from_be_bytes(random)));
    Ok(PathBuf::from(name))
}

/// Renames `tmp` over `path`. On Windows the rename is refused for as long as somebody holds the target open
/// without sharing deletion (the C# apps reading it, another writer replacing it, an antivirus scan), which
/// lasts milliseconds: wait it out instead of losing the save.
fn replace(tmp: &Path, path: &Path) -> io::Result<()> {
    const ATTEMPTS: u32 = 10;
    let mut attempt = 1;
    loop {
        match fs::rename(tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) if cfg!(windows) && attempt < ATTEMPTS && is_sharing_conflict(&e) => {
                std::thread::sleep(Duration::from_millis(5 * u64::from(attempt)));
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

fn is_sharing_conflict(e: &io::Error) -> bool {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    e.kind() == io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(ERROR_SHARING_VIOLATION)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn objects_keep_the_order_they_were_written_in() {
        // Sealing a payload relies on this (serde_json's preserve_order feature); sorted keys would still verify,
        // but would no longer match the reference vectors byte for byte.
        let v = parse(r#"{"v":1,"t":2,"host":"h","a":0}"#).unwrap();
        assert_eq!(to_string(&v), r#"{"v":1,"t":2,"host":"h","a":0}"#);
        let mut m = Map::new();
        m.insert("z".into(), Value::from(1));
        m.insert("a".into(), Value::from(2));
        assert_eq!(to_string(&Value::Object(m)), r#"{"z":1,"a":2}"#);
    }

    #[test]
    fn the_writer_escapes_what_minijson_escaped_and_nothing_else() {
        let v = Value::from("a\"b\\c\nd\r\t\u{8}\u{c}\u{1}/é\u{7f}😀");
        assert_eq!(to_string(&v), "\"a\\\"b\\\\c\\nd\\r\\t\\b\\f\\u0001/é\u{7f}😀\"");
        assert_eq!(to_string(&serde_json::json!({"n": 1758500000000i64, "b": true, "x": null, "a": ["s", 2]})), r#"{"n":1758500000000,"b":true,"x":null,"a":["s",2]}"#);
    }

    #[test]
    fn trailing_text_after_the_value_is_ignored() {
        assert_eq!(parse(" {\"a\":1} trailing"), Ok(serde_json::json!({"a": 1})));
        assert_eq!(parse("[1,2]]"), Ok(serde_json::json!([1, 2])));
        assert_eq!(parse("\t\r\n \"s\"x"), Ok(Value::from("s")));
        assert!(parse("").is_err());
        assert!(parse("   ").is_err());
        assert!(parse("{\"a\":1,}").is_err());
        assert!(parse("\u{feff}{}").is_err()); // a byte order mark is not whitespace
    }

    #[test]
    fn a_repeated_key_keeps_its_last_value() {
        let v = parse(r#"{"v":1,"n":"first","v":2}"#).unwrap();
        assert_eq!(as_i64(get(&v, "v").unwrap()), Some(2));
        assert_eq!(get_str(&v, "n"), Some("first"));
    }

    #[test]
    fn numbers_are_read_however_they_arrive() {
        let v = parse(r#"{"i":3,"f":2.5,"g":3.5,"h":-2.5,"e":1e3,"s":" 42 ","neg":"-7","plus":"+7","bad":"4x","empty":"",
                          "big":18446744073709551615,"huge":1e300,"t":true,"z":-0,"fs":"2.5","inf":"inf","dot":".5"}"#)
            .unwrap();
        let int = |k: &str| as_i64(get(&v, k).unwrap());
        assert_eq!(int("i"), Some(3));
        assert_eq!(int("f"), Some(2)); // half to even, like Math.Round
        assert_eq!(int("g"), Some(4));
        assert_eq!(int("h"), Some(-2));
        assert_eq!(int("e"), Some(1000));
        assert_eq!(int("s"), Some(42));
        assert_eq!(int("neg"), Some(-7));
        assert_eq!(int("plus"), Some(7));
        assert_eq!(int("bad"), None);
        assert_eq!(int("empty"), None);
        assert_eq!(int("fs"), None); // "2.5" is not an integer
        assert_eq!(int("big"), Some(i64::MAX));
        assert_eq!(int("huge"), Some(i64::MAX));
        assert_eq!(int("t"), None);
        assert_eq!(int("z"), Some(0));

        let float = |k: &str| as_f64(get(&v, k).unwrap());
        assert_eq!(float("i"), Some(3.0));
        assert_eq!(float("f"), Some(2.5));
        assert_eq!(float("fs"), Some(2.5));
        assert_eq!(float("dot"), Some(0.5));
        assert_eq!(float("s"), Some(42.0));
        assert_eq!(float("inf"), None);
        assert_eq!(float("bad"), None);
        assert_eq!(float("t"), None);
    }

    #[test]
    fn flags_are_booleans_or_the_words_mpv_uses() {
        let v = parse(r#"{"a":true,"b":"yes","c":"no","d":"false","e":"Yes","f":1,"g":null}"#).unwrap();
        let flag = |k: &str| get(&v, k).and_then(as_bool);
        assert_eq!(flag("a"), Some(true));
        assert_eq!(flag("b"), Some(true));
        assert_eq!(flag("c"), Some(false));
        assert_eq!(flag("d"), Some(false));
        assert_eq!(flag("e"), None);
        assert_eq!(flag("f"), None);
        assert_eq!(flag("g"), None);
        assert!(get(&v, "g").is_none(), "null reads as missing");
        assert!(get(&Value::from(1), "a").is_none());
    }

    #[test]
    fn a_saved_file_loads_again_and_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("hdrcast-json-{}", std::process::id()));
        let path = dir.join("nested").join("settings.json");
        save_file(&path, &serde_json::json!({"name": "小明", "n": 1})).unwrap();
        save_file(&path, &serde_json::json!({"name": "小红", "n": 2})).unwrap(); // replaces an existing file
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\n  \"name\": \"小红\""), "indented, non-ASCII written raw: {text}");
        assert_eq!(load_file_value(&path), Some(serde_json::json!({"name": "小红", "n": 2})));
        assert_eq!(load_file::<std::collections::BTreeMap<String, Value>>(&path).unwrap().len(), 2);
        let left: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("settings.json")]);

        // a byte order mark (Notepad) is not part of the JSON
        fs::write(&path, b"\xEF\xBB\xBF{\"n\":3}").unwrap();
        assert_eq!(load_file_value(&path), Some(serde_json::json!({"n": 3})));
        fs::write(&path, b"{oops").unwrap();
        assert_eq!(load_file_value(&path), None);
        assert_eq!(load_file_value(&dir.join("missing.json")), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}
