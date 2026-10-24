//! What the app knows between commands: the three settings files, the group, the displays, and the views of
//! them the page shows.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};

use hdrcast_capture::OutputInfo;
use hdrcast_core::common::firewall::{self, row as firewall_row, FirewallVerdict};
use hdrcast_core::common::process_job::ProcessJob;
use hdrcast_core::common::tool_locator::{self, Platform};
use hdrcast_core::common::{app_paths, json};
use hdrcast_core::group::config::GroupConfig;
use hdrcast_core::group::device;
use hdrcast_core::group::invite_flow::ClipboardInviteWatcher;
use hdrcast_core::group::now_ms;
use hdrcast_core::group::shared_file::SharedGroupFile;
use hdrcast_core::DeviceKey;
use hdrcast_media::receiver::settings::ReceiverSettings;
use hdrcast_media::sender::settings::{HdrMode, SenderSettings};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::access::AccessSettings;
use crate::logs::Logs;

/// The name of the firewall rule and of this program wherever Windows shows one.
pub const APP_NAME: &str = "HdrCast";

/// Preferences that belong to this app rather than to the sharing or watching side (`app.json`). 0.3.0 kept its
/// remote-control rule here too ("remoteControl"); `access.json` took it over (see `AccessSettings::load`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UiSettings {
    /// "auto", "en", "zh" or "ja".
    pub language: String,
    /// "auto", "light" or "dark".
    pub theme: String,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self { language: "auto".into(), theme: "auto".into() }
    }
}

/// A device that asked to connect lately, for the list of devices one can give a rule to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenDevice {
    pub key: String,
    pub name: String,
    pub at_ms: i64,
}

impl UiSettings {
    pub const FILE_NAME: &'static str = "app.json";

    pub fn path() -> PathBuf {
        app_paths::settings_file(Self::FILE_NAME)
    }

    pub fn load() -> Self {
        json::load_file::<UiSettings>(&Self::path()).unwrap_or_default().normalized()
    }

    pub fn normalized(mut self) -> Self {
        if !["auto", "en", "zh", "ja"].contains(&self.language.as_str()) {
            self.language = "auto".into();
        }
        if !["auto", "light", "dark"].contains(&self.theme.as_str()) {
            self.theme = "auto".into();
        }
        self
    }

    pub fn save(&self) -> std::io::Result<()> {
        json::save_file(&Self::path(), self)
    }
}

pub struct Inner {
    pub ui: UiSettings,
    pub sender: SenderSettings,
    pub receiver: ReceiverSettings,
    pub group: SharedGroupFile,
    pub outputs: Vec<OutputInfo>,
    pub firewall: FirewallVerdict,
    pub clipboard: ClipboardInviteWatcher,
    pub access: AccessSettings,
    /// Counts changes to `access` the page did not make itself (a request answered with "always", a device
    /// that renamed itself), so the page knows to fetch the rules again.
    pub access_rev: u64,
}

pub struct AppState {
    pub logs: Arc<Logs>,
    /// Every child process is put in here, so none outlives the app, however it ends.
    pub job: Arc<ProcessJob>,
    pub platform: Platform,
    pub backdrop: bool,
    pub startup_invite: Mutex<Option<String>>,
    /// Started with `--background` (at sign-in): the page leaves the window hidden (taken by the first `app_init`).
    pub start_hidden: AtomicBool,
    pub inner: Mutex<Inner>,
    /// This computer's device key (docs/design.md §8.10).
    pub device: Arc<DeviceKey>,
    /// Devices that asked to connect since the app started, newest first.
    pub seen: Mutex<Vec<SeenDevice>>,
    /// The app, for what needs the window (set once the window exists).
    pub handle: OnceLock<tauri::AppHandle>,
    pub share: crate::share::Share,
    pub watch: crate::watch::Watch,
    pub live: crate::live::Live,
    /// The panel over the player while watching.
    pub overlay: crate::overlay::Overlay,
}

impl AppState {
    pub fn new(logs: Arc<Logs>, backdrop: bool, startup_invite: Option<String>, start_hidden: bool) -> Self {
        let platform = Platform::CURRENT;
        let outputs = hdrcast_capture::enumerate_outputs().unwrap_or_else(|e| {
            logs.tagged("app", &format!("cannot enumerate displays: {e}"));
            Vec::new()
        });
        let mut sender = SenderSettings::load(&SenderSettings::default_path());
        adopt_output(&mut sender, &outputs);
        let (device, error) = DeviceKey::load_or_create(&DeviceKey::default_path());
        if let Some(e) = error {
            logs.tagged("app", &format!("could not save this computer's device key ({e}): hosts that trust it will not know it after a restart"));
        }
        let inner = Inner {
            ui: UiSettings::load(),
            sender,
            receiver: ReceiverSettings::load(&ReceiverSettings::default_path()),
            group: SharedGroupFile::open_default(),
            outputs,
            firewall: firewall::check(&current_exe()),
            clipboard: ClipboardInviteWatcher::new(),
            access: AccessSettings::load(),
            access_rev: 0,
        };
        Self {
            logs,
            job: Arc::new(ProcessJob::new()),
            platform,
            backdrop,
            startup_invite: Mutex::new(startup_invite),
            start_hidden: AtomicBool::new(start_hidden),
            inner: Mutex::new(inner),
            device: Arc::new(device),
            seen: Mutex::new(Vec::new()),
            handle: OnceLock::new(),
            share: Default::default(),
            watch: Default::default(),
            live: Default::default(),
            overlay: Default::default(),
        }
    }

    /// A device asked to connect: remember it for the list of devices one can give a rule to.
    pub fn note_seen(&self, device_key: &Option<String>, name: &str) {
        let Some(key) = device_key else { return };
        let mut seen = self.seen.lock().unwrap();
        seen.retain(|d| &d.key != key);
        seen.insert(0, SeenDevice { key: key.clone(), name: name.to_string(), at_ms: now_ms() });
        seen.truncate(32);
    }

    /// Something needs the user (a request to connect, or for control): the window comes back if it was closed
    /// to the notification area or minimized, and its taskbar button flashes / the dock icon bounces.
    pub fn request_attention(&self) {
        if let Some(handle) = self.handle.get() {
            crate::window::surface(handle);
        }
    }

    pub fn log(&self, line: &str) {
        self.logs.write(line);
    }

    /// A logger for the engine's parts, which all take one of these.
    pub fn logger(&self) -> Arc<dyn Fn(&str) + Send + Sync> {
        let logs = self.logs.clone();
        Arc::new(move |line: &str| logs.write(line))
    }

    /// The same logger for the parts that take a plain closure.
    pub fn log_fn(&self) -> impl Fn(&str) + Send + Sync + 'static {
        let logs = self.logs.clone();
        move |line: &str| logs.write(line)
    }

    pub fn group(&self) -> GroupConfig {
        self.inner.lock().unwrap().group.current().clone()
    }
}

/// Forwarding the port on the router (UPnP) is on unless the environment says otherwise: `HDRCAST_NO_ROUTER=1`
/// is for networks where the router must be left alone, and for trying a build out on one computer.
pub fn router_mapping_wanted() -> bool {
    std::env::var_os("HDRCAST_NO_ROUTER").is_none()
}

pub fn current_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from(APP_NAME))
}

pub fn platform_name(platform: Platform) -> &'static str {
    match platform {
        Platform::Windows => "windows",
        Platform::MacOs => "macos",
        Platform::Other => "linux",
    }
}

// ---- displays -------------------------------------------------------------------------------------------------

/// Finds the display for a saved configuration: by index, falling back to the remembered names (the system may
/// number the displays differently after a cable or driver change).
pub fn resolve_output<'a>(outputs: &'a [OutputInfo], index: i32, device_name: &str, friendly_name: &str) -> Option<&'a OutputInfo> {
    let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    let at_index = |o: &&OutputInfo| i64::from(o.index) == i64::from(index);
    outputs
        .iter()
        .find(|o| at_index(o) && (device_name.is_empty() || same(&o.device_name, device_name)))
        .or_else(|| outputs.iter().find(|o| !device_name.is_empty() && same(&o.device_name, device_name)))
        .or_else(|| outputs.iter().find(|o| !friendly_name.is_empty() && same(&o.friendly_name, friendly_name)))
        .or_else(|| outputs.iter().find(at_index))
        .or_else(|| outputs.first())
}

pub fn selected_output<'a>(sender: &SenderSettings, outputs: &'a [OutputInfo]) -> Option<&'a OutputInfo> {
    resolve_output(outputs, sender.output_index, &sender.output_device_name, &sender.output_friendly_name)
}

/// Points the settings at the display they resolve to, so index and names agree again.
pub fn adopt_output(sender: &mut SenderSettings, outputs: &[OutputInfo]) {
    if let Some(o) = selected_output(sender, outputs) {
        sender.output_index = o.index as i32;
        sender.output_device_name = o.device_name.clone();
        sender.output_friendly_name = o.friendly_name.clone();
    }
}

pub fn outputs_view(outputs: &[OutputInfo]) -> Value {
    Value::Array(
        outputs
            .iter()
            .map(|o| {
                json!({
                    "index": o.index,
                    "deviceName": o.device_name,
                    "friendlyName": if o.friendly_name.is_empty() { "Display" } else { o.friendly_name.as_str() },
                    "width": o.width,
                    "height": o.height,
                    "refreshHz": o.refresh_rate,
                    "hdr": o.hdr,
                    "maxLuminance": o.max_luminance,
                })
            })
            .collect(),
    )
}

/// What a session started now would send: shown under the display picker and in the action bar.
pub fn plan_view(sender: &SenderSettings, outputs: &[OutputInfo]) -> Value {
    let Some(o) = selected_output(sender, outputs) else { return Value::Null };
    let hdr = o.hdr && sender.hdr_mode == HdrMode::Auto;
    let (width, height) = sender.resolve_size(o.width, o.height);
    let note = if o.hdr && sender.hdr_mode == HdrMode::ForceSdr {
        "The monitor is in HDR mode, streaming SDR anyway."
    } else if !o.hdr && sender.hdr_mode == HdrMode::Auto {
        "This display is in SDR mode: turn on HDR in the system's display settings for an HDR stream."
    } else {
        ""
    };
    let need = upload_mbps_for(sender.bitrate_bps(), sender.max_viewers.max(0) as u32, 25);
    json!({
        "hdr": hdr,
        "width": width,
        "height": height,
        "fps": sender.fps,
        "format": format_label(hdr, width, height),
        "note": note,
        "uploadNeedMbps": need,
        "uploadTooMuch": sender.upload_mbps > 0 && need > f64::from(sender.upload_mbps) * 0.8,
    })
}

/// "p010 / HDR10 3840x2160": the frames of a stream, as the page shows them.
pub fn format_label(hdr: bool, width: u32, height: u32) -> String {
    format!("{} {width}x{height}", if hdr { "p010 / HDR10" } else { "nv12 / SDR" })
}

/// Upload a stream needs: bitrate × viewers × (1 + SRT overhead).
pub fn upload_mbps_for(bitrate_bps: u64, viewers: u32, overhead_percent: u32) -> f64 {
    bitrate_bps as f64 / 1e6 * f64::from(viewers) * (1.0 + f64::from(overhead_percent) / 100.0)
}

// ---- tools, firewall, group -------------------------------------------------------------------------------------

pub fn ffmpeg_path(sender: &SenderSettings) -> Option<PathBuf> {
    tool_locator::resolve(Some(&sender.ffmpeg_path), &tool_locator::exe_name(tool_locator::FFMPEG))
}

pub fn mpv_path(receiver: &ReceiverSettings) -> Option<PathBuf> {
    tool_locator::resolve(Some(&receiver.mpv_path), &tool_locator::exe_name(tool_locator::MPV))
}

pub fn tools_view(sender: &SenderSettings, receiver: &ReceiverSettings) -> Value {
    let path = |p: Option<PathBuf>| p.map(|p| p.to_string_lossy().into_owned());
    json!({
        "ffmpeg": { "path": path(ffmpeg_path(sender)) },
        "mpv": { "path": path(mpv_path(receiver)) },
    })
}

pub fn firewall_view(verdict: FirewallVerdict) -> Value {
    let status = firewall_row::status(verdict);
    json!({
        "applicable": verdict != FirewallVerdict::NotApplicable,
        "allowed": firewall_row::inbound_allowed(verdict),
        "canFix": status.show_allow_button,
        "text": status.text,
    })
}

/// The access rules as the page shows them: every device with the code people compare. The page sends the same
/// shape back (the codes and `rev` are ignored then).
pub fn access_view(access: &AccessSettings, rev: u64) -> Value {
    json!({
        "rev": rev,
        "available": access.available,
        "others": access.others,
        "devices": access.devices.iter().map(|d| json!({
            "key": d.key,
            "code": device::code_of(&d.key),
            "name": d.name,
            "connect": d.connect,
            "control": d.control,
            "lastSeenMs": d.last_seen_ms,
        })).collect::<Vec<_>>(),
    })
}

/// Devices one could give a rule to: those that asked to connect lately and those online in the group, minus
/// the ones that have a rule already and this computer itself.
pub fn device_candidates(app: &AppState, access: &AccessSettings) -> Value {
    let mut list: Vec<(String, String, &'static str)> = Vec::new();
    let mut add = |key: &str, name: &str, source: &'static str| {
        if key != app.device.public_key() && access.device(key).is_none() && !list.iter().any(|(k, _, _)| k == key) {
            list.push((key.to_string(), name.to_string(), source));
        }
    };
    for seen in app.seen.lock().unwrap().iter() {
        add(&seen.key, &seen.name, "asked");
    }
    for (key, name) in crate::live::device_keys(app) {
        add(&key, &name, "online");
    }
    Value::Array(
        list.into_iter()
            .map(|(key, name, source)| json!({ "key": key, "code": device::code_of(&key), "name": name, "source": source }))
            .collect(),
    )
}

pub fn group_view(group: &GroupConfig) -> Value {
    json!({
        "configured": group.is_configured(),
        "groupName": group.group_name,
        "displayName": group.display_name,
        "peerId": group.peer_id,
        "detectClipboard": group.detect_clipboard_invites,
    })
}
