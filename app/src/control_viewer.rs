//! Remote control, viewer side: mpv's window is where the stream plays, so mpv tells us (over its JSON IPC)
//! where the pointer is and which keys and buttons go down; those go to the host over the P2P side channel.
//!
//! While the host allows control, an input section is switched on in mpv that sends every key to this IPC
//! client instead of acting on it (see `hdrcast_control::keys`); Ctrl+Alt+Enter stays with the player and
//! toggles full screen.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hdrcast_control::{keys, ControlState, ViewerControl};
use hdrcast_media::receiver::mpv_ipc::{observed, MpvIpcMessage, MpvIpcMessageKind};
use serde_json::{json, Value};

use crate::mpv_ipc::MpvIpcClient;

type LogFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Sends a side-channel datagram to the host; false when there is no path (or the host does not take them).
pub type SendFn = Arc<dyn Fn(&[u8]) -> bool + Send + Sync>;

#[derive(Default, Clone, Copy)]
struct Osd {
    w: f64,
    h: f64,
    left: f64,
    right: f64,
    top: f64,
    bottom: f64,
}

#[derive(Default)]
struct Inner {
    control: Option<ViewerControl>,
    sid: String,
    ipc: Option<MpvIpcClient>,
    /// Our client name inside mpv ("ipc_0"), which the input section's bindings address.
    client_name: Option<String>,
    name_request: Option<i64>,
    section_on: bool,
    osd: Osd,
    mouse: (f64, f64, bool),
    last_state: Option<ControlState>,
}

pub struct ViewerControlLink {
    inner: Mutex<Inner>,
    send: SendFn,
    log: LogFn,
    started: Instant,
    stopped: AtomicBool,
}

impl ViewerControlLink {
    pub fn start(send: SendFn, log: LogFn) -> Arc<Self> {
        let link = Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            send,
            log,
            started: Instant::now(),
            stopped: AtomicBool::new(false),
        });
        let me = link.clone();
        thread::Builder::new()
            .name("control-viewer".into())
            .spawn(move || {
                while !me.stopped.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(4));
                    me.poll();
                }
            })
            .ok();
        link
    }

    fn now_us(&self) -> i64 {
        self.started.elapsed().as_micros() as i64
    }

    /// The path to the host is up with session `sid` (a reconnect makes a new session, with a new key).
    /// `key` = None: this host does not offer remote control.
    pub fn session(&self, sid: &str, key: Option<[u8; 32]>) {
        let mut inner = self.inner.lock().unwrap();
        if inner.sid == sid && inner.control.is_some() == key.is_some() {
            return;
        }
        inner.sid = sid.to_string();
        inner.control = key.map(|k| ViewerControl::new(&k, true, self.now_us()));
        inner.last_state = None;
    }

    /// A side-channel datagram from the host.
    pub fn on_datagram(&self, payload: &[u8]) {
        if let Some(control) = self.inner.lock().unwrap().control.as_mut() {
            control.on_datagram(payload);
        }
    }

    /// A (re)started mpv is reachable: ask who we are in it; its input section has to be defined again.
    pub fn ipc_ready(&self, ipc: MpvIpcClient) {
        let mut inner = self.inner.lock().unwrap();
        inner.client_name = None;
        inner.section_on = false;
        inner.name_request = Some(ipc.command(&[json!("client_name")]));
        inner.ipc = Some(ipc);
    }

    pub fn on_ipc_message(&self, msg: &MpvIpcMessage) {
        let mut inner = self.inner.lock().unwrap();
        match msg.kind {
            MpvIpcMessageKind::Reply => {
                if msg.request_id.is_some() && msg.request_id == inner.name_request {
                    inner.name_request = None;
                    inner.client_name = msg.data.as_str().map(str::to_string);
                }
            }
            MpvIpcMessageKind::Event => match msg.event.as_deref() {
                Some("client-message") => {
                    // ["key-binding", <binding name>, <state: "d-", "u-", "r-", "p-", …>, <key name>, …]
                    let args: Vec<&str> =
                        msg.raw.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                    if args.len() >= 4 && args[0] == "key-binding" && args[1] == keys::BINDING {
                        if let Some(control) = inner.control.as_mut() {
                            control.key_binding(args[2], args[3]);
                        }
                    }
                }
                Some("property-change") => {
                    let number = |v: &Value, key: &str| v.get(key).and_then(Value::as_f64).unwrap_or(0.0);
                    match msg.name.as_deref() {
                        Some(observed::MOUSE_POS) => {
                            let hover = msg.data.get("hover").and_then(Value::as_bool).unwrap_or(false);
                            inner.mouse = (number(&msg.data, "x"), number(&msg.data, "y"), hover);
                        }
                        Some(observed::OSD_DIMENSIONS) => {
                            inner.osd = Osd {
                                w: number(&msg.data, "w"),
                                h: number(&msg.data, "h"),
                                left: number(&msg.data, "ml"),
                                right: number(&msg.data, "mr"),
                                top: number(&msg.data, "mt"),
                                bottom: number(&msg.data, "mb"),
                            };
                        }
                        _ => return,
                    }
                    // The picture sits inside the window with black bars where the shapes differ.
                    let (x, y, hover) = inner.mouse;
                    let osd = inner.osd;
                    let (pw, ph) = (osd.w - osd.left - osd.right, osd.h - osd.top - osd.bottom);
                    if pw >= 1.0 && ph >= 1.0 {
                        let (fx, fy) = ((x - osd.left) / pw, (y - osd.top) / ph);
                        let inside = hover && (0.0..=1.0).contains(&fx) && (0.0..=1.0).contains(&fy);
                        if let Some(control) = inner.control.as_mut() {
                            control.pointer(fx, fy, inside);
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn poll(&self) {
        let now = self.now_us();
        let mut note = None;
        let datagram = {
            let mut inner = self.inner.lock().unwrap();
            let Some(control) = inner.control.as_mut() else { return };
            let datagram = control.poll(now);
            let state = control.state(now);
            let granted = control.granted();
            if inner.last_state != Some(state) {
                inner.last_state = Some(state);
                note = Some(match state {
                    ControlState::Granted => {
                        "[control] the host allowed remote control: the player window now sends your mouse and keyboard \
                         (Ctrl+Alt+Enter toggles full screen)"
                    }
                    ControlState::Denied => "[control] the host does not allow remote control",
                    ControlState::Requested => "[control] asked the host for remote control",
                    ControlState::Unsupported => "[control] the host did not answer the remote control request",
                    ControlState::Off => "[control] off",
                });
            }
            Self::sync_section(&mut inner, granted);
            datagram
        };
        if let Some(line) = note {
            (self.log)(line);
        }
        if let Some(datagram) = datagram {
            (self.send)(&datagram);
        }
    }

    /// Switches mpv's input section on while control is granted and off otherwise.
    fn sync_section(inner: &mut Inner, granted: bool) {
        if granted == inner.section_on {
            return;
        }
        let (Some(ipc), Some(name)) = (inner.ipc.as_ref(), inner.client_name.as_ref()) else { return };
        if !ipc.is_connected() {
            return;
        }
        if granted {
            ipc.command(&[json!("define-section"), json!(keys::SECTION), json!(keys::mpv_input_section(name)), json!("force")]);
            ipc.command(&[json!("enable-section"), json!(keys::SECTION)]);
            // a drag in the picture must reach the host, not move the player's window
            ipc.set_property("window-dragging", false);
        } else {
            ipc.command(&[json!("disable-section"), json!(keys::SECTION)]);
            ipc.set_property("window-dragging", true);
        }
        inner.section_on = granted;
    }

    /// "off", "requested", "granted", "denied" or "unsupported", for the page.
    pub fn state(&self) -> &'static str {
        let inner = self.inner.lock().unwrap();
        match inner.control.as_ref().map(|c| c.state(self.now_us())) {
            Some(ControlState::Requested) => "requested",
            Some(ControlState::Granted) => "granted",
            Some(ControlState::Denied) => "denied",
            Some(ControlState::Unsupported) => "unsupported",
            Some(ControlState::Off) | None => "off",
        }
    }

    /// No session any more (the path dropped): stop sending until the next one.
    pub fn session_ended(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.control = None;
        inner.sid.clear();
        let _ = &inner.last_state;
        Self::sync_section(&mut inner, false);
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let mut inner = self.inner.lock().unwrap();
        inner.control = None;
        Self::sync_section(&mut inner, false);
        inner.ipc = None;
    }
}
