//! Who may connect to this computer, and who may control it (docs/design.md §8.10): the rules in `access.json`.
//!
//! A rule belongs to a device key, not to a name or a peer id: anyone in the group could write those. Viewers
//! older than 0.3.1 sign nothing and always get the rule for everyone else.

use std::path::PathBuf;

use hdrcast_core::common::{app_paths, json};
use hdrcast_core::group::device;
use hdrcast_core::group::now_ms;
use serde::{Deserialize, Serialize};

/// Whether a viewer may start watching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Connect {
    /// Let in at once, even when this computer is not live.
    Allow,
    /// Ask this computer's user (while live, everyone may watch who is not blocked).
    Ask,
    /// Never.
    Block,
}

/// Whether a viewer may use this computer's mouse and keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Control {
    Off,
    /// The viewer may ask; this computer's user says yes or no each time.
    Ask,
    /// Granted as soon as the viewer connects.
    Allow,
}

// A value nobody wrote (a hand edit, a later version) reads as the cautious choice instead of losing the file.
impl<'de> Deserialize<'de> for Connect {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match serde_json::Value::deserialize(d)?.as_str() {
            Some("allow") => Connect::Allow,
            Some("block") => Connect::Block,
            _ => Connect::Ask,
        })
    }
}

impl<'de> Deserialize<'de> for Control {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match serde_json::Value::deserialize(d)?.as_str() {
            Some("ask") => Control::Ask,
            Some("allow") => Control::Allow,
            _ => Control::Off,
        })
    }
}

impl Control {
    /// The number the remote-control hub takes (0 off, 1 ask, 2 allow).
    pub fn policy(self) -> u8 {
        match self {
            Control::Off => 0,
            Control::Ask => 1,
            Control::Allow => 2,
        }
    }

    fn from_policy(policy: u64) -> Control {
        match policy {
            1 => Control::Ask,
            2 => Control::Allow,
            _ => Control::Off,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Rule {
    pub connect: Connect,
    pub control: Control,
}

impl Default for Rule {
    fn default() -> Self {
        Rule { connect: Connect::Ask, control: Control::Off }
    }
}

/// The rule of one device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DeviceRule {
    /// The device key, base64url.
    pub key: String,
    /// The name it used when last seen (its owner's name in the group).
    pub name: String,
    pub connect: Connect,
    pub control: Control,
    pub last_seen_ms: i64,
}

impl Default for DeviceRule {
    fn default() -> Self {
        DeviceRule { key: String::new(), name: String::new(), connect: Connect::Allow, control: Control::Ask, last_seen_ms: 0 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AccessSettings {
    pub v: i32,
    /// Announce this computer to the group whenever the app runs, so that the group can connect without it
    /// going live first: the screen is captured only while somebody it lets in is watching.
    pub available: bool,
    /// Everyone without a rule of their own, and every viewer older than 0.3.1.
    pub others: Rule,
    pub devices: Vec<DeviceRule>,
}

impl Default for AccessSettings {
    fn default() -> Self {
        AccessSettings { v: 1, available: true, others: Rule::default(), devices: Vec::new() }
    }
}

pub const MAX_DEVICES: usize = 64;

impl AccessSettings {
    pub const FILE_NAME: &'static str = "access.json";

    pub fn path() -> PathBuf {
        app_paths::settings_file(Self::FILE_NAME)
    }

    /// The saved rules. Without `access.json` (first run of 0.3.1), 0.3.0's one remote-control setting in
    /// `app.json` ("remoteControl": 0 off, 1 ask, 2 allow) becomes the rule for everyone else.
    pub fn load() -> AccessSettings {
        if let Some(saved) = json::load_file::<AccessSettings>(&Self::path()) {
            return saved.normalized();
        }
        let legacy = json::load_file_value(&crate::state::UiSettings::path())
            .and_then(|app| json::get(&app, "remoteControl").and_then(json::as_i64))
            .unwrap_or(0);
        let mut fresh = AccessSettings::default();
        fresh.others.control = Control::from_policy(legacy.max(0) as u64);
        fresh
    }

    pub fn save(&self) -> std::io::Result<()> {
        json::save_file(&Self::path(), self)
    }

    /// Keeps only usable device rules (one per key, the newest first), at most [`MAX_DEVICES`].
    pub fn normalized(mut self) -> AccessSettings {
        self.v = 1;
        let mut kept: Vec<DeviceRule> = Vec::new();
        for mut rule in std::mem::take(&mut self.devices) {
            if device::parse_public_key(&rule.key).is_none() || kept.iter().any(|k| k.key == rule.key) {
                continue;
            }
            rule.name = hdrcast_core::config::clean_display_name(&rule.name);
            kept.push(rule);
        }
        kept.truncate(MAX_DEVICES);
        self.devices = kept;
        self
    }

    pub fn device(&self, key: &str) -> Option<&DeviceRule> {
        self.devices.iter().find(|d| d.key == key)
    }

    /// The rule for a viewer whose join was signed by `device_key` (`None`: unsigned, or not checked).
    pub fn rule_for(&self, device_key: Option<&str>) -> Rule {
        match device_key.and_then(|key| self.device(key)) {
            Some(d) => Rule { connect: d.connect, control: d.control },
            None => self.others,
        }
    }

    /// Does anybody get remote control at all? (Said in the live record: viewers only ask hosts that offer it.)
    pub fn offers_control(&self) -> bool {
        self.others.control != Control::Off || self.devices.iter().any(|d| d.control != Control::Off)
    }

    /// Notes that the device was seen under `name`; true when that changed what is saved.
    pub fn touch(&mut self, key: &str, name: &str) -> bool {
        let Some(rule) = self.devices.iter_mut().find(|d| d.key == key) else { return false };
        let now = now_ms();
        // the name is worth saving at once; the time only when it is an hour old
        let changed = rule.name != name || now - rule.last_seen_ms > 3_600_000;
        if changed {
            rule.name = name.to_string();
            rule.last_seen_ms = now;
        }
        changed
    }

    /// Adds a device (or updates its name) with the given rule.
    pub fn upsert(&mut self, key: &str, name: &str, connect: Connect, control: Control) {
        match self.devices.iter_mut().find(|d| d.key == key) {
            Some(rule) => {
                rule.name = name.to_string();
                rule.connect = connect;
                rule.control = control;
                rule.last_seen_ms = now_ms();
            }
            None => self.devices.insert(
                0,
                DeviceRule { key: key.to_string(), name: name.to_string(), connect, control, last_seen_ms: now_ms() },
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hdrcast_core::DeviceKey;

    #[test]
    fn rules_are_found_by_device_key_and_everyone_else_is_the_fallback() {
        let mine = DeviceKey::generate().public_key().to_string();
        let mut access = AccessSettings::default();
        assert!(access.available, "available by default");
        assert_eq!(access.others, Rule { connect: Connect::Ask, control: Control::Off });
        access.upsert(&mine, "laptop", Connect::Allow, Control::Allow);
        assert_eq!(access.rule_for(Some(&mine)), Rule { connect: Connect::Allow, control: Control::Allow });
        assert_eq!(access.rule_for(None), access.others);
        assert_eq!(access.rule_for(Some(DeviceKey::generate().public_key())), access.others);
        assert!(access.offers_control());
        access.devices[0].control = Control::Off;
        assert!(!access.offers_control());
    }

    #[test]
    fn broken_and_duplicate_device_rules_are_dropped() {
        let key = DeviceKey::generate().public_key().to_string();
        let text = format!(
            r#"{{"available":false,"others":{{"connect":"block","control":"ask"}},"devices":[
                {{"key":"{key}","name":" a ","connect":"ask","control":"off"}},
                {{"key":"{key}","name":"again","connect":"allow","control":"allow"}},
                {{"key":"nonsense","name":"x"}},
                {{"key":"{key}x"}}
            ]}}"#
        );
        let access = serde_json::from_str::<AccessSettings>(&text).unwrap().normalized();
        assert!(!access.available);
        assert_eq!(access.others, Rule { connect: Connect::Block, control: Control::Ask });
        assert_eq!(access.devices.len(), 1);
        assert_eq!((access.devices[0].name.as_str(), access.devices[0].connect), ("a", Connect::Ask));
        // a value nobody wrote reads as the cautious choice instead of losing the file
        let odd = serde_json::from_str::<AccessSettings>(r#"{"available":true,"others":{"connect":"maybe","control":7}}"#).unwrap();
        assert_eq!(odd.others, Rule { connect: Connect::Ask, control: Control::Off });
        assert!(odd.available);
    }
}
