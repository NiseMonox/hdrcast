//! Remote control, host side: one [`HostControl`] per viewer session, registered when the viewer is let in with
//! the key and the rule of that session, fed from the P2P side channel, answered through it, and ticked so that
//! nothing a viewer held down stays down when they vanish.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hdrcast_control::inject::{self, ScreenRect};
use hdrcast_control::{Button, Grant, HostControl, Injector, Key, Mods, SystemInjector};

type LogFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Sends a side-channel datagram to a viewer's session.
pub type SendFn = Arc<dyn Fn(&str, &[u8]) -> bool + Send + Sync>;

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
    /// The first pointer move or key of this viewer has been seen (and said so in the log).
    input_seen: bool,
}

struct Inner {
    sessions: HashMap<String, Entry>,
    /// The shared display: the viewer's pointer is confined to it.
    display: ScreenRect,
}

pub struct ControlHub {
    inner: Mutex<Inner>,
    send: SendFn,
    log: LogFn,
    started: Instant,
    stopped: AtomicBool,
}

impl ControlHub {
    pub fn start(send: SendFn, log: LogFn) -> Arc<Self> {
        let hub = Arc::new(Self {
            inner: Mutex::new(Inner { sessions: HashMap::new(), display: ScreenRect { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0 } }),
            send,
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

    /// The display being shared (as the capture engine names it): pointers of sessions registered from now on are
    /// confined to it.
    pub fn set_display(&self, display_name: &str) {
        let rect = inject::display_rect(display_name).unwrap_or_else(|| {
            (self.log)(&format!("[control] cannot find the display {display_name}: the pointer will use the main display"));
            ScreenRect { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0 }
        });
        self.inner.lock().unwrap().display = rect;
    }

    /// A viewer was let in: its datagrams are sealed with `key`, and `policy` (0 off, 1 ask, 2 allow) is what it
    /// may do. Datagrams of sessions nobody registered are ignored.
    pub fn register(&self, sid: &str, name: &str, key: &[u8; 32], policy: u8) {
        let mut inner = self.inner.lock().unwrap();
        let injector: Box<dyn Injector> = if std::env::var_os(DRY_RUN_ENV).is_some() {
            Box::new(LoggingInjector { log: self.log.clone(), name: name.to_string(), pointer_logged: None })
        } else {
            // the log hears how the input service does (docs/design.md §9.1)
            Box::new(SystemInjector::for_session(inner.display, self.log.clone()))
        };
        let entry = Entry {
            control: HostControl::new(key, grant_of(policy), injector),
            name: name.to_string(),
            requested: false,
            granted: false,
            input_seen: false,
        };
        inner.sessions.insert(sid.to_string(), entry);
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
            let Some(entry) = inner.sessions.get_mut(sid) else { return };
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
        // Real input is never logged (it could be a password), but that some arrived is worth one line: it tells a viewer
        // that sends nothing from a host that cannot get what it sends into a window.
        if !entry.input_seen && entry.control.input_events() > 0 {
            entry.input_seen = true;
            notes.push(format!("[control] {}'s mouse or keyboard input is arriving", entry.name));
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

    /// The rule of one session changed (0 off, 1 ask, 2 allow). Turning it off takes control back.
    pub fn set_policy(&self, sid: &str, policy: u8) {
        let mut notes = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            let Some(entry) = inner.sessions.get_mut(sid) else { return };
            entry.control.set_policy(grant_of(policy));
            Self::note_changes(entry, &mut notes);
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
