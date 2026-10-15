//! The host's half: opens a viewer's datagrams and, once allowed, replays them through an [`Injector`].

use std::collections::HashSet;

use crate::inject::Injector;
use crate::keys::{Button, Key, Mods};
use crate::wire::{Channel, EventKind, GrantState, HostMessage, ViewerMessage};

/// The host's standing rule for viewers who ask for control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    /// Nobody controls this computer.
    Off,
    /// Each viewer who asks waits for the user's answer.
    Ask,
    /// Everyone in the group who asks is allowed.
    Allow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Undecided,
    Allowed,
    Refused,
}

/// Answers are sent at most this often (plus at once when the grant changes).
const REPLY_INTERVAL_US: i64 = 10_000;
/// Nothing heard from the viewer for this long: let go of everything it was holding down.
const RELEASE_AFTER_US: i64 = 1_500_000;

/// One viewer's control session on the host.
pub struct HostControl {
    channel: Channel,
    injector: Box<dyn Injector>,
    policy: Grant,
    decision: Decision,
    wants: bool,
    applied_seq: u32,
    pointer: (u16, u16),
    held_keys: HashSet<u32>,
    held_buttons: HashSet<u32>,
    mods_down: u8,
    last_heard_us: i64,
    last_reply_us: i64,
    announce: u8,
}

impl HostControl {
    pub fn new(key: &[u8; 32], policy: Grant, injector: Box<dyn Injector>) -> Self {
        Self {
            channel: Channel::host(key),
            injector,
            policy,
            decision: Decision::Undecided,
            wants: false,
            applied_seq: 0,
            pointer: (u16::MAX, u16::MAX),
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
            mods_down: 0,
            last_heard_us: 0,
            last_reply_us: i64::MIN / 2,
            announce: 0,
        }
    }

    fn grant_state(&self) -> GrantState {
        match (self.policy, self.decision) {
            (Grant::Off, _) | (_, Decision::Refused) => GrantState::Denied,
            (_, Decision::Allowed) | (Grant::Allow, Decision::Undecided) => GrantState::Granted,
            (Grant::Ask, Decision::Undecided) => GrantState::Pending,
        }
    }

    /// The viewer asked and the user has not answered yet: show the request.
    pub fn requested(&self) -> bool {
        self.wants && self.grant_state() == GrantState::Pending
    }

    /// The viewer is controlling this computer right now.
    pub fn granted(&self) -> bool {
        self.wants && self.grant_state() == GrantState::Granted
    }

    /// The user's answer to this viewer (also how control is taken back).
    pub fn decide(&mut self, allow: bool) {
        self.decision = if allow { Decision::Allowed } else { Decision::Refused };
        self.after_change();
    }

    /// The standing rule changed in the settings. Turning it off takes control back from everyone.
    pub fn set_policy(&mut self, policy: Grant) {
        if policy == self.policy {
            return;
        }
        self.policy = policy;
        self.after_change();
    }

    fn after_change(&mut self) {
        if !self.granted() {
            self.release_all();
        }
        // UDP: say it a few times so the viewer's side follows promptly even if one answer is lost.
        self.announce = 3;
    }

    /// A datagram from the viewer's side of the channel. Returns the answer to send back, if one is due.
    pub fn on_datagram(&mut self, datagram: &[u8], now_us: i64) -> Option<Vec<u8>> {
        let plain = self.channel.open(datagram)?;
        let msg = ViewerMessage::decode(&plain)?;
        self.last_heard_us = now_us;
        if msg.wants_control != self.wants {
            self.wants = msg.wants_control;
            if !self.wants {
                self.release_all();
            }
        }
        if self.granted() {
            self.apply(&msg);
        } else {
            // Not allowed (yet): acknowledge so the viewer's queue does not grow, but do nothing.
            let last = msg.first_seq.wrapping_add(msg.events.len() as u32).wrapping_sub(1);
            if !msg.events.is_empty() {
                self.applied_seq = last;
            }
        }
        if now_us - self.last_reply_us >= REPLY_INTERVAL_US || !msg.events.is_empty() {
            return Some(self.reply(now_us));
        }
        None
    }

    fn apply(&mut self, msg: &ViewerMessage) {
        if msg.pointer_inside && (msg.pointer_x, msg.pointer_y) != self.pointer {
            self.pointer = (msg.pointer_x, msg.pointer_y);
            self.injector.pointer(msg.pointer_x, msg.pointer_y);
        }
        if msg.events.is_empty() {
            return;
        }
        let expected = self.applied_seq.wrapping_add(1);
        // The viewer dropped events we never saw (its queue overflowed while the path was down): whatever was
        // held then may never get its release, so let go of everything and continue from what it sends now.
        let gap = self.applied_seq != 0 && msg.first_seq.wrapping_sub(expected) < u32::MAX / 2 && msg.first_seq != expected;
        if gap {
            self.release_all();
        }
        for (i, event) in msg.events.iter().enumerate() {
            let seq = msg.first_seq.wrapping_add(i as u32);
            let fresh = if gap || self.applied_seq == 0 {
                true
            } else {
                // newer than the last one applied, allowing for wrap-around
                seq.wrapping_sub(self.applied_seq).wrapping_sub(1) < u32::MAX / 2
            };
            if !fresh {
                continue;
            }
            self.applied_seq = seq;
            match event.kind {
                EventKind::KeyDown => {
                    let Some(key) = Key::from_code(event.code) else { continue };
                    self.sync_mods(event.mods);
                    self.held_keys.insert(event.code);
                    self.injector.key(key, true);
                }
                EventKind::KeyUp => {
                    let Some(key) = Key::from_code(event.code) else { continue };
                    if self.held_keys.remove(&event.code) {
                        self.injector.key(key, false);
                    }
                    self.release_mods_if_idle();
                }
                EventKind::ButtonDown => {
                    let Some(button) = Button::from_code(event.code) else { continue };
                    self.sync_mods(event.mods);
                    self.held_buttons.insert(event.code);
                    self.injector.button(button, true);
                }
                EventKind::ButtonUp => {
                    let Some(button) = Button::from_code(event.code) else { continue };
                    if self.held_buttons.remove(&event.code) {
                        self.injector.button(button, false);
                    }
                    self.release_mods_if_idle();
                }
                EventKind::Wheel => {
                    self.sync_mods(event.mods);
                    self.injector.wheel(event.code as u8, event.arg);
                    self.release_mods_if_idle();
                }
                EventKind::Text => {
                    if let Some(c) = char::from_u32(event.code).filter(|c| !c.is_control()) {
                        self.injector.key(Key::Char(c), true);
                        self.injector.key(Key::Char(c), false);
                    }
                }
            }
        }
    }

    /// Presses and releases modifier keys until exactly `wanted` are down. mpv reports modifiers only as part of
    /// another key ("Ctrl+c"), never on their own, so they are held for as long as such a key or button is.
    fn sync_mods(&mut self, wanted: u8) {
        for (bit, _) in Mods::ALL {
            let is = self.mods_down & bit != 0;
            let should = wanted & bit != 0;
            if is != should {
                self.injector.modifier(bit, should);
            }
        }
        self.mods_down = wanted & 0x0f;
    }

    fn release_mods_if_idle(&mut self) {
        if self.held_keys.is_empty() && self.held_buttons.is_empty() {
            self.sync_mods(0);
        }
    }

    /// Lets go of every key, button and modifier this session holds down.
    pub fn release_all(&mut self) {
        for code in std::mem::take(&mut self.held_keys) {
            if let Some(key) = Key::from_code(code) {
                self.injector.key(key, false);
            }
        }
        for code in std::mem::take(&mut self.held_buttons) {
            if let Some(button) = Button::from_code(code) {
                self.injector.button(button, false);
            }
        }
        self.sync_mods(0);
    }

    fn reply(&mut self, now_us: i64) -> Vec<u8> {
        self.last_reply_us = now_us;
        let msg = HostMessage { state: self.grant_state(), acked_seq: self.applied_seq };
        self.channel.seal(&msg.encode())
    }

    /// Timers. Returns a datagram to send, if one is due. Call every few tens of milliseconds.
    pub fn tick(&mut self, now_us: i64) -> Option<Vec<u8>> {
        let holding = !self.held_keys.is_empty() || !self.held_buttons.is_empty() || self.mods_down != 0;
        if holding && now_us - self.last_heard_us > RELEASE_AFTER_US {
            self.release_all();
        }
        if self.announce > 0 && self.wants {
            self.announce -= 1;
            return Some(self.reply(now_us));
        }
        None
    }
}

impl Drop for HostControl {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::NamedKey;
    use crate::viewer::{ControlState, ViewerControl};
    use std::sync::{Arc, Mutex};

    #[derive(Default, Clone)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl Recorder {
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    impl Injector for Recorder {
        fn pointer(&mut self, x: u16, y: u16) {
            self.0.lock().unwrap().push(format!("move {x},{y}"));
        }
        fn button(&mut self, button: Button, down: bool) {
            self.0.lock().unwrap().push(format!("{button:?} {}", if down { "down" } else { "up" }));
        }
        fn wheel(&mut self, axis: u8, delta: i16) {
            self.0.lock().unwrap().push(format!("wheel {axis} {delta}"));
        }
        fn modifier(&mut self, bit: u8, down: bool) {
            self.0.lock().unwrap().push(format!("mod{bit} {}", if down { "down" } else { "up" }));
        }
        fn key(&mut self, key: Key, down: bool) {
            let name = match key {
                Key::Char(c) => c.to_string(),
                Key::Named(n) => format!("{n:?}"),
            };
            self.0.lock().unwrap().push(format!("{name} {}", if down { "down" } else { "up" }));
        }
    }

    const KEY: [u8; 32] = [3; 32];

    fn pair(policy: Grant) -> (ViewerControl, HostControl, Recorder) {
        let rec = Recorder::default();
        (ViewerControl::new(&KEY, true, 0), HostControl::new(&KEY, policy, Box::new(rec.clone())), rec)
    }

    /// Carries what each side has to say to the other, `rounds` times, 10 ms apart.
    fn exchange(viewer: &mut ViewerControl, host: &mut HostControl, now: &mut i64, rounds: usize) {
        for _ in 0..rounds {
            *now += 10_000;
            if let Some(d) = viewer.poll(*now) {
                if let Some(r) = host.on_datagram(&d, *now) {
                    viewer.on_datagram(&r);
                }
            }
            if let Some(r) = host.tick(*now) {
                viewer.on_datagram(&r);
            }
        }
    }

    #[test]
    fn nothing_is_injected_until_the_host_allows_it() {
        let (mut viewer, mut host, rec) = pair(Grant::Ask);
        let mut now = 0;
        exchange(&mut viewer, &mut host, &mut now, 3);
        assert!(host.requested());
        assert_eq!(viewer.state(now), ControlState::Requested);
        viewer.key_binding("d-", "a");          // dropped on the viewer: not granted
        viewer.pointer(0.5, 0.5, true);
        exchange(&mut viewer, &mut host, &mut now, 3);
        assert!(rec.take().is_empty());

        host.decide(true);
        exchange(&mut viewer, &mut host, &mut now, 3);
        assert_eq!(viewer.state(now), ControlState::Granted);
        viewer.pointer(0.25, 0.75, true);
        viewer.key_binding("d-", "Ctrl+c");
        viewer.key_binding("u-", "Ctrl+c");
        exchange(&mut viewer, &mut host, &mut now, 3);
        assert_eq!(rec.take(), ["move 16384,49151", "mod2 down", "c down", "c up", "mod2 up"]);
    }

    #[test]
    fn a_host_that_refuses_or_has_it_off_says_so() {
        let (mut viewer, mut host, rec) = pair(Grant::Off);
        let mut now = 0;
        exchange(&mut viewer, &mut host, &mut now, 3);
        assert_eq!(viewer.state(now), ControlState::Denied);
        assert!(!host.requested());

        let (mut viewer, mut host, _) = pair(Grant::Ask);
        exchange(&mut viewer, &mut host, &mut now, 3);
        host.decide(false);
        exchange(&mut viewer, &mut host, &mut now, 3);
        assert_eq!(viewer.state(now), ControlState::Denied);
        assert!(rec.take().is_empty());
    }

    #[test]
    fn lost_datagrams_do_not_lose_or_repeat_keys() {
        let (mut viewer, mut host, rec) = pair(Grant::Allow);
        let mut now = 0;
        exchange(&mut viewer, &mut host, &mut now, 3);
        viewer.key_binding("d-", "a");
        now += 10_000;
        let _lost = viewer.poll(now).unwrap();
        viewer.key_binding("u-", "a");
        viewer.key_binding("d-", "ENTER");
        now += 50_000;
        let d = viewer.poll(now).unwrap();
        // the answer is lost too, so the viewer sends all three again
        let _lost_reply = host.on_datagram(&d, now);
        assert_eq!(rec.take(), ["a down", "a up", "Enter down"]);
        exchange(&mut viewer, &mut host, &mut now, 8);
        assert!(rec.take().is_empty(), "already applied events must not be applied twice");
        viewer.key_binding("u-", "ENTER");
        exchange(&mut viewer, &mut host, &mut now, 3);
        assert_eq!(rec.take(), ["Enter up"]);
    }

    #[test]
    fn taking_control_back_releases_what_was_held() {
        let (mut viewer, mut host, rec) = pair(Grant::Allow);
        let mut now = 0;
        exchange(&mut viewer, &mut host, &mut now, 3);
        viewer.key_binding("d-", "Shift+MBTN_LEFT");
        viewer.key_binding("d-", "w");
        exchange(&mut viewer, &mut host, &mut now, 3);
        assert_eq!(rec.take(), ["mod1 down", "Left down", "mod1 up", "w down"]);
        host.decide(false);
        let mut released = rec.take();
        released.sort();
        assert_eq!(released, ["Left up", "w up"]);
        exchange(&mut viewer, &mut host, &mut now, 3);
        assert_eq!(viewer.state(now), ControlState::Denied);
        viewer.key_binding("d-", "x");
        exchange(&mut viewer, &mut host, &mut now, 3);
        assert!(rec.take().is_empty());
    }

    #[test]
    fn a_viewer_that_goes_silent_lets_go_of_its_keys() {
        let (mut viewer, mut host, rec) = pair(Grant::Allow);
        let mut now = 0;
        exchange(&mut viewer, &mut host, &mut now, 3);
        viewer.key_binding("d-", "Alt+TAB");
        exchange(&mut viewer, &mut host, &mut now, 2);
        assert_eq!(rec.take(), ["mod4 down", "Tab down"]);
        now += 2_000_000;
        host.tick(now);
        assert_eq!(rec.take(), ["Tab up", "mod4 up"]);
        let _ = NamedKey::Tab;
    }

    #[test]
    fn repeats_wheel_and_text_reach_the_host() {
        let (mut viewer, mut host, rec) = pair(Grant::Allow);
        let mut now = 0;
        exchange(&mut viewer, &mut host, &mut now, 3);
        viewer.key_binding("d-", "BS");
        viewer.key_binding("r-", "BS");
        viewer.key_binding("u-", "BS");
        viewer.key_binding("p-", "WHEEL_DOWN");
        viewer.key_binding("d-", "中");
        viewer.key_binding("u-", "中");
        viewer.key_binding("d-", "VOLUME_UP");      // not forwarded
        exchange(&mut viewer, &mut host, &mut now, 3);
        assert_eq!(rec.take(), ["Backspace down", "Backspace down", "Backspace up", "wheel 0 -120", "中 down", "中 up"]);
    }

    #[test]
    fn strangers_and_old_hosts_are_handled() {
        let rec = Recorder::default();
        let mut host = HostControl::new(&KEY, Grant::Allow, Box::new(rec.clone()));
        let mut stranger = ViewerControl::new(&[9; 32], true, 0);
        let d = stranger.poll(1_000_000).unwrap();
        assert!(host.on_datagram(&d, 1_000_000).is_none());
        assert!(!host.granted());

        // a host that never answers (0.2.4): after a while the viewer stops waiting
        let mut viewer = ViewerControl::new(&KEY, true, 0);
        assert_eq!(viewer.state(1_000_000), ControlState::Requested);
        assert_eq!(viewer.state(6_000_000), ControlState::Unsupported);
        viewer.key_binding("d-", "a");
        assert!(rec.take().is_empty());
    }
}
