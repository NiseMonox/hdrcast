//! Port of MiniJsonTests from tests/ReceiverTests.cs, against the `json` module that replaces MiniJson
//! (the rest of that file tests the receiver, which lives in another crate).

use hdrcast_core::json::{self, Value};

#[test]
fn parses_the_shapes_mpv_sends() {
    let v = json::parse(r#"{"event":"property-change","id":3,"name":"demuxer-cache-duration","data":2.5}"#).unwrap();
    assert_eq!(json::get_str(&v, "event"), Some("property-change"));
    assert_eq!(json::get(&v, "id").and_then(json::as_i64), Some(3));
    assert_eq!(json::get(&v, "data").and_then(json::as_f64), Some(2.5));
}

#[test]
fn handles_nested_objects_arrays_and_escapes() {
    let v = json::parse(r#"{"a":[1,2,{"b":"x\ny"}],"c":true,"d":null}"#).unwrap();
    let arr = json::get(&v, "a").and_then(Value::as_array).expect("an array");
    assert_eq!(arr.len(), 3);
    assert_eq!(json::get_str(&arr[2], "b"), Some("x\ny"));
    assert_eq!(json::get(&v, "c").and_then(json::as_bool), Some(true));
    assert!(json::get(&v, "d").is_none());
}

#[test]
fn round_trips_through_serialize() {
    let obj = serde_json::json!({"command": ["set_property", "fullscreen", true]});
    let back = json::parse(&json::to_string(&obj)).unwrap();
    let cmd = json::get(&back, "command").and_then(Value::as_array).unwrap();
    assert_eq!(cmd[0], "set_property");
    assert_eq!(cmd[2], true);
    assert_eq!(json::to_string(&obj), r#"{"command":["set_property","fullscreen",true]}"#);
}

#[test]
fn bad_input_is_an_error_not_a_panic() {
    assert!(json::try_parse("{oops").is_none());
    assert!(json::try_parse("").is_none());
    assert!(json::parse("{oops").unwrap_err().to_string().starts_with("JSON: "));
}

#[test]
fn escapes_control_characters_when_writing() {
    let json = json::to_string(&serde_json::json!({"k": "a\"b\\c\nd"}));
    assert!(json.contains("\\\""));
    assert!(json.contains("\\\\"));
    assert!(json.contains("\\n"));
    assert_eq!(json, r#"{"k":"a\"b\\c\nd"}"#);
}
