//! Remote control, host side: one [`HostControl`] per viewer who asked, fed from the P2P side channel, answered
//! through it, and ticked so that nothing a viewer held down stays down when they vanish.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hdrcast_control::inject::{self, ScreenRect};
use hdrcast_control::{Button, Grant, HostControl, Injector, Key, Mods, SystemInjector};
use hdrcast_core::group::keys::GroupKeys;

type LogFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Sends a side-channel datagram to a viewer's session.
pub type SendFn = Arc<dyn Fn(&str, &[u8]) -> bool + Send + Sync>;
/// The display name of a viewer's session, for the log.
pub type NameFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// With this set, what a viewer does is written to the log instead of done: for trying remote control with host
/// and viewer on the same computer, where real input would feed back into the player window it comes from.
pub const DRY_RUN_ENV: &str = "HDRCAST_CONTROL_DRY_RUN";

struct LoggingInjector {
    log: LogFn,
    name: String,
    pointer_logged: Option<Instant>,
}

impl LoggingInjector {
    fn say(&self, what: String) {
        (self.log)(&format!("[control] (dry run) {}: {what}", self.name));
    }
}

impl Injector for LoggingInjector {
    fn pointer(&mut self, x: u16, y: u16) {
        // a moving pointer reports 125 times a second: one line per half second is enough to see it arrive
        if self.pointer_logged.is_none_or(|t| t.elapsed() >= Duration::from_millis(500)) {
            self.pointer_logged = Some(Instant::now());
            self.say(format!("pointer to {:.1}%, {:.1}%", f64::from(x) / 655.35, f64::from(y) / 655.35));
        }
    }
    fn button(&mut self, button: Button, down: bool) {
        self.say(format!("{button:?} button {}", if down { "down" } else { "up" }));
    }
    fn wheel(&mut self, axis: u8, delta: i16) {
        self.say(format!("wheel {} {delta}", if axis == 0 { "vertical" } else { "horizontal" }));
    }
    fn modifier(&mut self, bit: u8, down: bool) {
        let name = Mods::ALL.iter().find(|(b, _)| *b == bit).map_or("?", |(_, n)| n);
        self.say(format!("{name} {}", if down { "down" } else { "up" }));
    }
    fn key(&mut self, key: Key, down: bool) {
        // which key it is stays out of the log: it could be someone's password
        let kind = match key {
            Key::Char(_) => "a character key".to_string(),
            Key::Named(n) => format!("{n:?}"),
        };
        self.say(format!("{kind} {}", if down { "down" } else { "up" }));
    }
}

pub fn grant_of(policy: u8) -> Grant {
    match policy {
        1 => Grant::Ask,
        2 => Grant::Allow,
        _ => Grant::Off,
    }
}

struct Entry {
    control: HostControl,
    name: String,
    requested: bool,
    granted: bool,
}

struct Inner {
    policy: Grant,
    sessions: HashMap<String, Entry>,
}

pub struct ControlHub {
    keys: GroupKeys,
    /// The shared display: the viewer's pointer is confined to it.
    display: ScreenRect,
    inner: Mutex<Inner>,
    send: SendFn,
    name_of: NameFn,
    log: LogFn,
    started: Instant,
    stopped: AtomicBool,
}

impl ControlHub {
    /// `display_name` is the shared display as the capture engine names it.
    pub fn start(keys: GroupKeys, policy: u8, display_name: &str, send: SendFn, name_of: NameFn, log: LogFn) -> Arc<Self> {
        let display = inject::display_rect(display_name).unwrap_or_else(|| {
            log(&format!("[control] cannot find the display {display_name}: the pointer will use the main display"));
            ScreenRect { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0 }
        });
        let hub = Arc::new(Self {
            keys,
            display,
            inner: Mutex::new(Inner { policy: grant_of(policy), sessions: HashMap::new() }),
            send,
            name_of,
            log,
            started: Instant::now(),
            stopped: AtomicBool::new(false),
        });
        let me = hub.clone();
        thread::Builder::new()
            .name("control-host".into())
            .spawn(move || {
                while !me.stopped.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(15));
                    me.tick();
                }
            })
            .ok();
        hub
    }

    fn now_us(&self) -> i64 {
        self.started.elapsed().as_micros() as i64
    }

    /// A side-channel datagram from the viewer of session `sid` (the link has checked where it came from).
    pub fn on_datagram(&self, sid: &str, payload: &[u8]) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        let now = self.now_us();
        let mut notes = Vec::new();
        let reply = {
            let mut inner = self.inner.lock().unwrap();
            let policy = inner.policy;
            let entry = inner.sessions.entry(sid.to_string()).or_insert_with(|| {
                let name = (self.name_of)(sid).unwrap_or_else(|| "A viewer".to_string());
                let injector: Box<dyn Injector> = if std::env::var_os(DRY_RUN_ENV).is_some() {
                    Box::new(LoggingInjector { log: self.log.clone(), name: name.clone(), pointer_logged: None })
                } else {
                    Box::new(SystemInjector::new(self.display))
                };
                Entry { control: HostControl::new(&self.keys.control_key(sid), policy, injector), name, requested: false, granted: false }
            });
            let reply = entry.control.on_datagram(payload, now);
            Self::note_changes(entry, &mut notes);
            reply
        };
        for line in notes {
            (self.log)(&line);
        }
        if let Some(reply) = reply {
            (self.send)(sid, &reply);
        }
    }

    fn note_changes(entry: &mut Entry, notes: &mut Vec<String>) {
        let (requested, granted) = (entry.control.requested(), entry.control.granted());
        if requested && !entry.requested {
            notes.push(format!("[control] {} asks to control this computer", entry.name));
        }
        if granted && !entry.granted {
            notes.push(format!("[control] {} now controls this computer's mouse and keyboard", entry.name));
        }
        if !granted && entry.granted {
            notes.push(format!("[control] {} no longer controls this computer", entry.name));
        }
        entry.requested = requested;
        entry.granted = granted;
    }

    fn tick(&self) {
        let now = self.now_us();
        let mut out = Vec::new();
        let mut notes = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            for (sid, entry) in inner.sessions.iter_mut() {
                if let Some(datagram) = entry.control.tick(now) {
                    out.push((sid.clone(), datagram));
                }
                Self::note_changes(entry, &mut notes);
            }
        }
        for line in notes {
            (self.log)(&line);
        }
        for (sid, datagram) in out {
            (self.send)(&sid, &datagram);
        }
    }

    /// The user's answer to one viewer, or taking control back from them. The error is a sentence to show.
    pub fn decide(&self, sid: &str, allow: bool) -> Result<(), String> {
        if allow && !SystemInjector::permitted(true) {
            return Err("This computer has not allowed HdrCast to control it yet. Allow it under System Settings > \
                        Privacy & Security > Accessibility, then try again."
                .to_string());
        }
        let mut notes = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            let Some(entry) = inner.sessions.get_mut(sid) else { return Err("That viewer is gone.".to_string()) };
            entry.control.decide(allow);
            Self::note_changes(entry, &mut notes);
        }
        for line in notes {
            (self.log)(&line);
        }
        Ok(())
    }

    pub fn set_policy(&self, policy: u8) {
        let mut notes = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.policy = grant_of(policy);
            let policy = inner.policy;
            for entry in inner.sessions.values_mut() {
                entry.control.set_policy(policy);
                Self::note_changes(entry, &mut notes);
            }
        }
        for line in notes {
            (self.log)(&line);
        }
    }

    /// "none", "requested" or "granted", for the viewer list.
    pub fn state_of(&self, sid: &str) -> &'static str {
        match self.inner.lock().unwrap().sessions.get(sid) {
            Some(e) if e.control.granted() => "granted",
            Some(e) if e.control.requested() => "requested",
            _ => "none",
        }
    }

    /// Forgets the sessions of viewers who left (dropping a session lets go of whatever it held down).
    pub fn keep_only(&self, live_sids: &[String]) {
        self.inner.lock().unwrap().sessions.retain(|sid, _| live_sids.iter().any(|s| s == sid));
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.inner.lock().unwrap().sessions.clear();
    }
}
