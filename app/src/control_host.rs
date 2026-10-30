//! Remote control, host side: one [`HostControl`] per viewer session, registered when the viewer is let in with
//! the key and the rule of that session, fed from the P2P side channel, answered through it, and ticked so that
//! nothing a viewer held down stays down when they vanish. Every viewer's pointer is confined to the display that
//! is shared, which each stream names when it starts and which is looked up again every second, because it moves
//! when its resolution or the arrangement of the displays changes.

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

/// Finds a display's rectangle by the name the capture engine knows it by.
type FindDisplay = Arc<dyn Fn(&str) -> Option<ScreenRect> + Send + Sync>;

/// With this set, what a viewer does is written to the log instead of done: for trying remote control with host
/// and viewer on the same computer, where real input would feed back into the player window it comes from.
pub const DRY_RUN_ENV: &str = "HDRCAST_CONTROL_DRY_RUN";

/// The pointer's display while no stream has said which one is shared, or when the one it named cannot be found.
const MAIN_DISPLAY: ScreenRect = ScreenRect { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0 };
/// How often the shared display is looked up again while somebody may control (Sunshine and Parsec, for one,
/// switch a display's resolution for their own clients). Tests look it up themselves.
const DISPLAY_CHECK: Duration = if cfg!(test) { Duration::from_secs(3600) } else { Duration::from_secs(1) };

struct LoggingInjector {
    log: LogFn,
    name: String,
    /// Where the pointer would go.
    display: ScreenRect,
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
            let (px, py) = self.display.point(x, y);
            self.say(format!("pointer to {:.1}%, {:.1}% = {px:.0}, {py:.0} on the desktop", f64::from(x) / 655.35, f64::from(y) / 655.35));
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
    fn set_display(&mut self, display: ScreenRect) {
        self.display = display;
        // the next move shows where pointers land now
        self.pointer_logged = None;
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
    /// The shared display, as the capture engine names it (empty until a stream named it), and where it is: the
    /// viewer's pointer is confined to it.
    display_name: String,
    display: ScreenRect,
    /// When `display` was last looked up.
    display_checked: Instant,
}

impl Inner {
    /// Pointers mean places on `rect`, the rectangle of the display `name`, from now on: for the sessions there are
    /// and for those registered later. Whether that is news.
    fn use_display(&mut self, name: &str, rect: ScreenRect) -> bool {
        self.display_checked = Instant::now();
        if self.display_name == name && self.display == rect {
            return false;
        }
        self.display_name = name.to_string();
        self.display = rect;
        for entry in self.sessions.values_mut() {
            entry.control.set_display(rect);
        }
        true
    }
}

pub struct ControlHub {
    inner: Mutex<Inner>,
    send: SendFn,
    log: LogFn,
    /// What viewers do is written to the log instead of done ([`DRY_RUN_ENV`]).
    dry_run: bool,
    find_display: FindDisplay,
    started: Instant,
    stopped: AtomicBool,
}

impl ControlHub {
    pub fn start(send: SendFn, log: LogFn) -> Arc<Self> {
        let find: FindDisplay = Arc::new(inject::display_rect);
        Self::start_with(send, log, std::env::var_os(DRY_RUN_ENV).is_some(), find)
    }

    fn start_with(send: SendFn, log: LogFn, dry_run: bool, find_display: FindDisplay) -> Arc<Self> {
        let hub = Arc::new(Self {
            inner: Mutex::new(Inner {
                sessions: HashMap::new(),
                display_name: String::new(),
                display: MAIN_DISPLAY,
                display_checked: Instant::now(),
            }),
            send,
            log,
            dry_run,
            find_display,
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

    /// The display being shared (as the capture engine names it), by a stream that starts: every viewer's pointer
    /// is confined to it, those already controlling included.
    pub fn set_display(&self, display_name: &str) {
        let rect = (self.find_display)(display_name).unwrap_or_else(|| {
            (self.log)(&format!("[control] cannot find the display {display_name}: the pointer will use the main display"));
            MAIN_DISPLAY
        });
        if self.inner.lock().unwrap().use_display(display_name, rect) {
            self.say_display(display_name, rect);
        }
    }

    /// Looks the shared display up again every [`DISPLAY_CHECK`] while there are sessions.
    fn check_display(&self) {
        let due = {
            let inner = self.inner.lock().unwrap();
            !inner.sessions.is_empty() && inner.display_checked.elapsed() >= DISPLAY_CHECK
        };
        if due {
            self.look_up_display();
        }
    }

    /// Looks the shared display up again: its rectangle changes with its resolution and with the arrangement of the
    /// displays. One that is not there for the moment keeps its last rectangle.
    fn look_up_display(&self) {
        let name = {
            let mut inner = self.inner.lock().unwrap();
            inner.display_checked = Instant::now();
            inner.display_name.clone()
        };
        if name.is_empty() {
            return;
        }
        let Some(rect) = (self.find_display)(&name) else { return };
        // (a display named by a stream meanwhile stands)
        let news = {
            let mut inner = self.inner.lock().unwrap();
            inner.display_name == name && inner.use_display(&name, rect)
        };
        if news {
            self.say_display(&name, rect);
        }
    }

    fn say_display(&self, name: &str, rect: ScreenRect) {
        (self.log)(&format!("[control] a viewer's pointer moves on {name}: {}x{} at ({}, {})", rect.width, rect.height, rect.x, rect.y));
    }

    /// A viewer was let in: its datagrams are sealed with `key`, and `policy` (0 off, 1 ask, 2 allow) is what it
    /// may do. Datagrams of sessions nobody registered are ignored.
    pub fn register(&self, sid: &str, name: &str, key: &[u8; 32], policy: u8) {
        let mut inner = self.inner.lock().unwrap();
        let injector: Box<dyn Injector> = if self.dry_run {
            Box::new(LoggingInjector { log: self.log.clone(), name: name.to_string(), display: inner.display, pointer_logged: None })
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
        self.check_display();
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

#[cfg(test)]
mod tests {
    use super::*;
    use hdrcast_control::ViewerControl;

    const KEY: [u8; 32] = [7; 32];
    const SHARED: &str = r"\\.\DISPLAY2";
    const OTHER: &str = r"\\.\DISPLAY1";
    const FOUR_K: ScreenRect = ScreenRect { x: 0.0, y: 0.0, width: 3840.0, height: 2160.0 };

    type Displays = Arc<Mutex<HashMap<String, ScreenRect>>>;

    /// A hub in dry run, as the app makes it, that finds the displays in `displays`.
    struct Rig {
        hub: Arc<ControlHub>,
        displays: Displays,
        lines: Arc<Mutex<Vec<String>>>,
        /// What the hub sent to the viewer.
        outbox: Arc<Mutex<Vec<Vec<u8>>>>,
        viewer: ViewerControl,
        now: i64,
    }

    impl Rig {
        fn new(displays: &[(&str, ScreenRect)]) -> Rig {
            let displays: Displays = Arc::new(Mutex::new(displays.iter().map(|(n, r)| (n.to_string(), *r)).collect()));
            let (lines, outbox) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
            let (into, sent, known) = (lines.clone(), outbox.clone(), displays.clone());
            let log: LogFn = Arc::new(move |line: &str| into.lock().unwrap().push(line.to_string()));
            let send: SendFn = Arc::new(move |_, payload| {
                sent.lock().unwrap().push(payload.to_vec());
                true
            });
            let find: FindDisplay = Arc::new(move |name: &str| known.lock().unwrap().get(name).copied());
            let hub = ControlHub::start_with(send, log, true, find);
            Rig { hub, displays, lines, outbox, viewer: ViewerControl::new(&KEY, true, 0), now: 0 }
        }

        /// The viewer is let in (it may control) and takes control.
        fn take_control(&mut self) {
            self.hub.register("s1", "Mac", &KEY, 2);
            for _ in 0..10 {
                self.exchange();
                if self.viewer.granted() {
                    return;
                }
            }
            panic!("the viewer did not get control");
        }

        /// What the viewer has to say goes to the hub, and the hub's answers come back.
        fn exchange(&mut self) {
            self.now += 10_000;
            if let Some(datagram) = self.viewer.poll(self.now) {
                self.hub.on_datagram("s1", &datagram);
            }
            for answer in std::mem::take(&mut *self.outbox.lock().unwrap()) {
                self.viewer.on_datagram(&answer);
            }
        }

        /// The viewer's pointer goes to (`x`, `y`) of the picture.
        fn point(&mut self, x: f64, y: f64) {
            self.viewer.pointer(x, y, true);
            self.exchange();
        }

        fn display_is(&self, name: &str, rect: Option<ScreenRect>) {
            let mut displays = self.displays.lock().unwrap();
            match rect {
                Some(rect) => displays.insert(name.to_string(), rect),
                None => displays.remove(name),
            };
        }

        /// What the log said about pointers, after the hub stopped.
        fn about_the_pointer(self) -> Vec<String> {
            self.hub.stop();
            self.lines.lock().unwrap().iter().filter(|l| l.contains("pointer")).cloned().collect()
        }
    }

    #[test]
    fn pointers_land_on_the_shared_display_and_follow_it_when_it_changes() {
        let mut rig = Rig::new(&[(SHARED, FOUR_K)]);
        // the stream that starts names its display before anybody is let in
        rig.hub.set_display(SHARED);
        rig.take_control();
        rig.point(0.5, 0.5);
        // a streaming program switches the display to its client's resolution while the viewer controls
        rig.display_is(SHARED, Some(ScreenRect { x: 0.0, y: 0.0, width: 2560.0, height: 1600.0 }));
        rig.hub.look_up_display();
        rig.point(0.25, 0.75);
        // gone for a moment (being reconfigured, say): the pointer keeps its display
        rig.display_is(SHARED, None);
        rig.hub.look_up_display();

        assert_eq!(
            rig.about_the_pointer(),
            [
                r"[control] a viewer's pointer moves on \\.\DISPLAY2: 3840x2160 at (0, 0)",
                "[control] (dry run) Mac: pointer to 50.0%, 50.0% = 1920, 1080 on the desktop",
                r"[control] a viewer's pointer moves on \\.\DISPLAY2: 2560x1600 at (0, 0)",
                "[control] (dry run) Mac: pointer to 25.0%, 75.0% = 640, 1199 on the desktop",
            ]
        );
    }

    #[test]
    fn another_display_shared_reaches_the_viewers_already_controlling() {
        let mut rig = Rig::new(&[(SHARED, FOUR_K), (OTHER, ScreenRect { x: 3840.0, y: 1080.0, width: 1920.0, height: 1080.0 })]);
        rig.hub.set_display(SHARED);
        rig.take_control();
        rig.point(0.0, 0.0);
        // the host shares the other display now: its stream starts again and names it (and it stays named)
        rig.hub.set_display(OTHER);
        rig.hub.look_up_display();
        rig.point(1.0, 1.0);

        assert_eq!(
            rig.about_the_pointer(),
            [
                r"[control] a viewer's pointer moves on \\.\DISPLAY2: 3840x2160 at (0, 0)",
                "[control] (dry run) Mac: pointer to 0.0%, 0.0% = 0, 0 on the desktop",
                r"[control] a viewer's pointer moves on \\.\DISPLAY1: 1920x1080 at (3840, 1080)",
                "[control] (dry run) Mac: pointer to 100.0%, 100.0% = 5759, 2159 on the desktop",
            ]
        );
    }
}
