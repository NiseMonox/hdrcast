//! mpv's JSON IPC protocol (DOCS/man/ipc.rst): the lines mpv sends, parsed tolerantly; the command lines we
//! send; the properties we observe; and the live status built from their changes.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use hdrcast_core::common::json::{self, Map, Value};
use serde::{Serialize, Serializer};

use crate::numfmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MpvIpcMessageKind {
    Event = 0,
    Reply = 1,
    Unknown = 2,
}

/// One line of mpv's JSON IPC protocol.
#[derive(Debug, Clone, PartialEq)]
pub struct MpvIpcMessage {
    pub kind: MpvIpcMessageKind,
    /// Event name ("property-change", "file-loaded", "end-file", "shutdown", ...).
    pub event: Option<String>,
    /// Property name for property-change events.
    pub name: Option<String>,
    /// Observer id for property-change events.
    pub id: Option<i64>,
    /// Property value / reply payload as mpv sent it; `Value::Null` when mpv reports the property as
    /// unavailable (or sent no data at all).
    pub data: Value,
    pub request_id: Option<i64>,
    /// Reply status ("success" or an error text).
    pub error: Option<String>,
    /// end-file reason ("eof", "stop", "quit", "error", "redirect", "unknown").
    pub reason: Option<String>,
    /// The whole message, for fields that have no accessor here.
    pub raw: Map<String, Value>,
}

impl MpvIpcMessage {
    pub fn is_property_change(&self) -> bool {
        self.kind == MpvIpcMessageKind::Event && self.event.as_deref() == Some("property-change")
    }

    pub fn is_success(&self) -> bool {
        self.kind == MpvIpcMessageKind::Reply
            && matches!(self.error.as_deref(), None | Some("success"))
    }

    /// `None` for blank or non-JSON lines (mpv may interleave nothing else, but stay safe).
    pub fn try_parse_line(line: &str) -> Option<MpvIpcMessage> {
        if line.trim().is_empty() {
            return None;
        }
        let root = json::try_parse(line)?;
        if !root.is_object() {
            return None;
        }

        let string = |key: &str| json::get_str(&root, key).map(str::to_string);
        let integer = |key: &str| json::get(&root, key).and_then(json::as_i64);
        let data = || root.get("data").cloned().unwrap_or(Value::Null);

        let mut m = MpvIpcMessage {
            kind: MpvIpcMessageKind::Unknown,
            event: None,
            name: None,
            id: None,
            data: Value::Null,
            request_id: None,
            error: None,
            reason: None,
            raw: Map::new(),
        };
        if let Some(event) = root.get("event").and_then(Value::as_str) {
            m.kind = MpvIpcMessageKind::Event;
            m.event = Some(event.to_string());
            m.name = string("name");
            m.id = integer("id");
            m.reason = string("reason");
            m.data = data();
        } else if root.get("error").is_some() || root.get("request_id").is_some() {
            m.kind = MpvIpcMessageKind::Reply;
            m.error = string("error");
            m.request_id = integer("request_id");
            m.data = data();
        }
        if let Value::Object(raw) = root {
            m.raw = raw;
        }
        Some(m)
    }
}

// ---- commands -------------------------------------------------------------------------------

/// One JSON command line for mpv (without the line feed): `{"command":[…]}`, plus `"request_id"` when a reply
/// should be matched to it. `command` is the command name followed by its arguments.
pub fn command(request_id: Option<i64>, command: &[Value]) -> String {
    let mut line = Map::new();
    line.insert("command".to_string(), Value::Array(command.to_vec()));
    if let Some(request_id) = request_id {
        line.insert("request_id".to_string(), Value::from(request_id));
    }
    json::to_string(&Value::Object(line))
}

pub fn observe_property(id: i64, name: &str) -> String {
    command(None, &["observe_property".into(), id.into(), name.into()])
}

pub fn get_property(name: &str, request_id: i64) -> String {
    command(Some(request_id), &["get_property".into(), name.into()])
}

pub fn set_property(name: &str, value: impl Into<Value>, request_id: Option<i64>) -> String {
    command(
        request_id,
        &["set_property".into(), name.into(), value.into()],
    )
}

pub fn cycle_property(name: &str, request_id: Option<i64>) -> String {
    command(request_id, &["cycle".into(), name.into()])
}

pub fn quit(request_id: Option<i64>) -> String {
    command(request_id, &["quit".into()])
}

/// Properties the receiver observes (the ids are echoed back in property-change events).
pub mod observed {
    pub const DEMUXER_CACHE_DURATION: &str = "demuxer-cache-duration";
    pub const PAUSED_FOR_CACHE: &str = "paused-for-cache";
    pub const FRAME_DROP_COUNT: &str = "frame-drop-count";
    pub const DECODER_FRAME_DROP_COUNT: &str = "decoder-frame-drop-count";
    pub const ESTIMATED_VF_FPS: &str = "estimated-vf-fps";
    pub const HWDEC_CURRENT: &str = "hwdec-current";
    pub const VIDEO_PARAMS: &str = "video-params";
    pub const VIDEO_TARGET_PARAMS: &str = "video-target-params";
    pub const VIDEO_BITRATE: &str = "video-bitrate";
    pub const CACHE_BUFFERING_STATE: &str = "cache-buffering-state";
    pub const SPEED: &str = "speed";
    pub const FULLSCREEN: &str = "fullscreen";
    /// Remote control: where the pointer is over the mpv window (`{"x":…,"y":…,"hover":…}`). The app reads it
    /// from [`MpvIpcMessage::data`](super::MpvIpcMessage); the status model does not interpret it.
    pub const MOUSE_POS: &str = "mouse-pos";
    /// Remote control: the window's size and the margins around the picture (`{"w","h","ml","mr","mt","mb",…}`),
    /// needed to turn a pointer position into a position on the host's screen. Read from the raw message too.
    pub const OSD_DIMENSIONS: &str = "osd-dimensions";

    pub const ALL: [(i64, &str); 14] = [
        (1, DEMUXER_CACHE_DURATION),
        (2, PAUSED_FOR_CACHE),
        (3, FRAME_DROP_COUNT),
        (4, DECODER_FRAME_DROP_COUNT),
        (5, ESTIMATED_VF_FPS),
        (6, HWDEC_CURRENT),
        (7, VIDEO_PARAMS),
        (8, VIDEO_TARGET_PARAMS),
        (9, VIDEO_BITRATE),
        (10, CACHE_BUFFERING_STATE),
        (11, SPEED),
        (12, FULLSCREEN),
        (13, MOUSE_POS),
        (14, OSD_DIMENSIONS),
    ];

    /// One `observe_property` command line per entry of [`ALL`].
    pub fn observe_commands() -> impl Iterator<Item = String> {
        ALL.iter()
            .map(|&(id, name)| super::observe_property(id, name))
    }
}

// ---- status ---------------------------------------------------------------------------------

/// Subset of mpv's video-params / video-target-params node we display.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MpvVideoParams {
    pub pixel_format: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub color_matrix: Option<String>,
    pub primaries: Option<String>,
    pub gamma: Option<String>,
    pub color_levels: Option<String>,
    pub max_luma: Option<f64>,
    pub min_luma: Option<f64>,
    pub sig_peak: Option<f64>,
}

impl MpvVideoParams {
    /// `None` when `node` is not an object (mpv sends null while there is no video).
    pub fn from_node(node: &Value) -> Option<MpvVideoParams> {
        if !node.is_object() {
            return None;
        }
        let string = |key: &str| json::get_str(node, key).map(str::to_string);
        let integer = |key: &str| json::get(node, key).and_then(json::as_i64);
        let number = |key: &str| json::get(node, key).and_then(json::as_f64);
        Some(MpvVideoParams {
            pixel_format: string("pixelformat"),
            width: integer("w"),
            height: integer("h"),
            color_matrix: string("colormatrix"),
            primaries: string("primaries"),
            gamma: string("gamma"),
            color_levels: string("colorlevels"),
            max_luma: number("max-luma"),
            min_luma: number("min-luma"),
            sig_peak: number("sig-peak"),
        })
    }

    /// True when the transfer is PQ or HLG (HDR content / HDR output).
    pub fn is_hdr(&self) -> bool {
        matches!(self.gamma.as_deref(), Some("pq" | "hlg"))
    }

    /// "bt.2020 / pq / bt.2020-ncl / p010 / 3840x2160 / peak 1000 nit"
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        parts.extend(self.primaries.clone());
        parts.extend(self.gamma.clone());
        parts.extend(self.color_matrix.clone());
        parts.extend(self.pixel_format.clone());
        if let (Some(width), Some(height)) = (self.width, self.height) {
            parts.push(format!("{width}x{height}"));
        }
        if let Some(max_luma) = self.max_luma {
            parts.push(format!("peak {} nit", numfmt::fixed(max_luma, 0, 0)));
        } else if let Some(sig_peak) = self.sig_peak {
            parts.push(format!("sig-peak {}", numfmt::fixed(sig_peak, 0, 2)));
        }
        if parts.is_empty() {
            "-".to_string()
        } else {
            parts.join(" / ")
        }
    }
}

impl fmt::Display for MpvVideoParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// Aggregated live state built from property-change events.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MpvStatusModel {
    pub cache_seconds: Option<f64>,
    pub paused_for_cache: bool,
    pub buffering_percent: Option<i64>,
    pub frame_drops: i64,
    pub decoder_frame_drops: i64,
    pub fps: Option<f64>,
    pub hw_dec: Option<String>,
    pub source: Option<MpvVideoParams>,
    pub target: Option<MpvVideoParams>,
    /// Bits per second as reported by mpv's video-bitrate.
    pub video_bitrate_bps: Option<f64>,
    pub speed: f64,
    pub fullscreen: bool,
    /// When a property was last applied. Serialised as milliseconds since the Unix epoch.
    #[serde(serialize_with = "unix_millis")]
    pub last_update_utc: Option<SystemTime>,
}

fn unix_millis<S: Serializer>(time: &Option<SystemTime>, serializer: S) -> Result<S::Ok, S::Error> {
    let millis = time
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    millis.serialize(serializer)
}

impl Default for MpvStatusModel {
    fn default() -> Self {
        MpvStatusModel {
            cache_seconds: None,
            paused_for_cache: false,
            buffering_percent: None,
            frame_drops: 0,
            decoder_frame_drops: 0,
            fps: None,
            hw_dec: None,
            source: None,
            target: None,
            video_bitrate_bps: None,
            speed: 1.0,
            fullscreen: false,
            last_update_utc: None,
        }
    }
}

impl MpvStatusModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Applies a property-change message; returns false for other messages and for properties the model does
    /// not keep (mouse-pos and osd-dimensions among them).
    pub fn apply(&mut self, msg: &MpvIpcMessage) -> bool {
        match &msg.name {
            Some(name) if msg.is_property_change() => self.apply_property(name, &msg.data),
            _ => false,
        }
    }

    pub fn apply_property(&mut self, name: &str, data: &Value) -> bool {
        match name {
            observed::DEMUXER_CACHE_DURATION => self.cache_seconds = json::as_f64(data),
            observed::PAUSED_FOR_CACHE => {
                self.paused_for_cache = json::as_bool(data).unwrap_or(false)
            }
            observed::FRAME_DROP_COUNT => self.frame_drops = json::as_i64(data).unwrap_or(0),
            observed::DECODER_FRAME_DROP_COUNT => {
                self.decoder_frame_drops = json::as_i64(data).unwrap_or(0)
            }
            observed::ESTIMATED_VF_FPS => self.fps = json::as_f64(data),
            observed::HWDEC_CURRENT => self.hw_dec = data.as_str().map(str::to_string),
            observed::VIDEO_PARAMS => self.source = MpvVideoParams::from_node(data),
            observed::VIDEO_TARGET_PARAMS => self.target = MpvVideoParams::from_node(data),
            observed::VIDEO_BITRATE => self.video_bitrate_bps = json::as_f64(data),
            observed::CACHE_BUFFERING_STATE => self.buffering_percent = json::as_i64(data),
            observed::SPEED => self.speed = json::as_f64(data).unwrap_or(1.0),
            observed::FULLSCREEN => self.fullscreen = json::as_bool(data).unwrap_or(false),
            _ => return false,
        }
        self.last_update_utc = Some(SystemTime::now());
        true
    }

    /// "39.9 Mbps", or "-" while unknown.
    pub fn describe_bitrate(&self) -> String {
        match self.video_bitrate_bps {
            Some(bps) => format!("{} Mbps", numfmt::fixed(bps / 1_000_000.0, 1, 1)),
            None => "-".to_string(),
        }
    }

    /// "2.50 s", "0.40 s  (buffering 27%)", "3.10 s  speed 1.05x".
    pub fn describe_cache(&self) -> String {
        let mut text = match self.cache_seconds {
            Some(seconds) => format!("{} s", numfmt::fixed(seconds, 2, 2)),
            None => "-".to_string(),
        };
        if self.paused_for_cache {
            text.push_str("  (buffering");
            if let Some(percent) = self.buffering_percent {
                text.push_str(&format!(" {percent}%"));
            }
            text.push(')');
        }
        if (self.speed - 1.0).abs() > 0.001 {
            text.push_str(&format!("  speed {}x", numfmt::fixed(self.speed, 2, 2)));
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_status_model_serialises_for_the_ui() {
        let mut status = MpvStatusModel::new();
        assert_eq!(
            serde_json::to_value(&status).unwrap()["lastUpdateUtc"],
            Value::Null
        );
        status.apply_property(observed::SPEED, &json!(1.05));
        let v = serde_json::to_value(&status).unwrap();
        assert_eq!(v["speed"], json!(1.05));
        assert_eq!(v["pausedForCache"], json!(false));
        assert!(
            v["lastUpdateUtc"].as_u64().unwrap() > 1_700_000_000_000,
            "{v}"
        );
        for key in [
            "cacheSeconds",
            "bufferingPercent",
            "frameDrops",
            "decoderFrameDrops",
            "fps",
            "hwDec",
            "source",
            "target",
            "videoBitrateBps",
            "fullscreen",
        ] {
            assert!(v.get(key).is_some(), "{key} missing in {v}");
        }
    }
}
