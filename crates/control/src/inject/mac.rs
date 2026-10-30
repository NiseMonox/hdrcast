//! macOS: Quartz events. Needs the Accessibility permission (System Settings > Privacy & Security).

use std::time::{Duration, Instant};

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode, CGMouseButton, EventField, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

use super::ScreenRect;
use crate::keys::{Button, Key, Mods, NamedKey};

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: core_foundation::dictionary::CFDictionaryRef) -> bool;
}

pub fn accessibility_trusted(prompt: bool) -> bool {
    let key = CFString::from_static_string("AXTrustedCheckOptionPrompt");
    let options = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), CFBoolean::from(prompt).as_CFType())]);
    // SAFETY: `options` is a valid CFDictionary for the duration of the call.
    unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) }
}

pub fn display_rect(name: &str) -> Option<ScreenRect> {
    let id: u32 = name.trim().parse().ok()?;
    if !CGDisplay::active_displays().ok()?.contains(&id) {
        return None;
    }
    let b = CGDisplay::new(id).bounds();
    Some(ScreenRect { x: b.origin.x, y: b.origin.y, width: b.size.width, height: b.size.height })
}

pub struct MacInjector {
    display: ScreenRect,
    position: CGPoint,
    mods: u8,
    left: bool,
    right: bool,
    other: bool,
    /// Quartz does not turn two quick clicks into a double click by itself: the click count is ours to set.
    last_click: Option<(Button, Instant, CGPoint)>,
    click_count: i64,
}

const DOUBLE_CLICK: Duration = Duration::from_millis(500);

fn source() -> Option<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()
}

fn flags(mods: u8) -> CGEventFlags {
    let mut f = CGEventFlags::CGEventFlagNull;
    if mods & Mods::SHIFT != 0 {
        f |= CGEventFlags::CGEventFlagShift;
    }
    if mods & Mods::CTRL != 0 {
        f |= CGEventFlags::CGEventFlagControl;
    }
    if mods & Mods::ALT != 0 {
        f |= CGEventFlags::CGEventFlagAlternate;
    }
    if mods & Mods::META != 0 {
        f |= CGEventFlags::CGEventFlagCommand;
    }
    f
}

fn modifier_keycode(bit: u8) -> Option<CGKeyCode> {
    match bit {
        Mods::SHIFT => Some(56),
        Mods::CTRL => Some(59),
        Mods::ALT => Some(58),
        Mods::META => Some(55),
        _ => None,
    }
}

fn named_keycode(key: NamedKey) -> CGKeyCode {
    use NamedKey::*;
    match key {
        Enter => 36,
        Tab => 48,
        Backspace => 51,
        Delete => 117,
        Insert => 114,
        Home => 115,
        End => 119,
        PageUp => 116,
        PageDown => 121,
        Escape => 53,
        Left => 123,
        Right => 124,
        Down => 125,
        Up => 126,
        F1 => 122,
        F2 => 120,
        F3 => 99,
        F4 => 118,
        F5 => 96,
        F6 => 97,
        F7 => 98,
        F8 => 100,
        F9 => 101,
        F10 => 109,
        F11 => 103,
        F12 => 111,
        Kp0 => 82,
        Kp1 => 83,
        Kp2 => 84,
        Kp3 => 85,
        Kp4 => 86,
        Kp5 => 87,
        Kp6 => 88,
        Kp7 => 89,
        Kp8 => 91,
        Kp9 => 92,
        KpDecimal => 65,
        KpEnter => 76,
        KpAdd => 69,
        KpSubtract => 78,
        KpMultiply => 67,
        KpDivide => 75,
        Print => 105,
        Menu => 110,
    }
}

/// The key that types `c` on the US layout, and whether Shift is part of it. Shortcuts (Cmd+C, games reading
/// W/A/S/D) need a real key code; anything not on this table is typed as Unicode text instead.
fn ansi_keycode(c: char) -> Option<(CGKeyCode, bool)> {
    const PLAIN: &[(char, CGKeyCode)] = &[
        ('a', 0), ('s', 1), ('d', 2), ('f', 3), ('h', 4), ('g', 5), ('z', 6), ('x', 7), ('c', 8), ('v', 9), ('b', 11),
        ('q', 12), ('w', 13), ('e', 14), ('r', 15), ('y', 16), ('t', 17), ('1', 18), ('2', 19), ('3', 20), ('4', 21),
        ('6', 22), ('5', 23), ('=', 24), ('9', 25), ('7', 26), ('-', 27), ('8', 28), ('0', 29), (']', 30), ('o', 31),
        ('u', 32), ('[', 33), ('i', 34), ('p', 35), ('l', 37), ('j', 38), ('\'', 39), ('k', 40), (';', 41), ('\\', 42),
        (',', 43), ('/', 44), ('n', 45), ('m', 46), ('.', 47), (' ', 49), ('`', 50),
    ];
    const SHIFTED: &[(char, char)] = &[
        ('!', '1'), ('@', '2'), ('#', '3'), ('$', '4'), ('%', '5'), ('^', '6'), ('&', '7'), ('*', '8'), ('(', '9'),
        (')', '0'), ('_', '-'), ('+', '='), ('{', '['), ('}', ']'), ('|', '\\'), (':', ';'), ('"', '\''), ('<', ','),
        ('>', '.'), ('?', '/'), ('~', '`'),
    ];
    let find = |ch: char| PLAIN.iter().find(|(k, _)| *k == ch).map(|(_, code)| *code);
    if let Some(code) = find(c) {
        return Some((code, false));
    }
    if c.is_ascii_uppercase() {
        return find(c.to_ascii_lowercase()).map(|code| (code, true));
    }
    SHIFTED.iter().find(|(s, _)| *s == c).and_then(|(_, base)| find(*base)).map(|code| (code, true))
}

impl MacInjector {
    pub fn new(display: ScreenRect) -> Self {
        let position = CGPoint::new(display.x + display.width / 2.0, display.y + display.height / 2.0);
        Self { display, position, mods: 0, left: false, right: false, other: false, last_click: None, click_count: 1 }
    }

    fn post_mouse(&self, kind: CGEventType, button: CGMouseButton, click_count: Option<i64>, button_number: Option<i64>) {
        let Some(src) = source() else { return };
        let Ok(event) = CGEvent::new_mouse_event(src, kind, self.position, button) else { return };
        event.set_flags(flags(self.mods));
        if let Some(n) = click_count {
            event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, n);
        }
        if let Some(n) = button_number {
            event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, n);
        }
        event.post(CGEventTapLocation::HID);
    }

    fn post_key(&self, code: CGKeyCode, down: bool, extra_mods: u8, text: Option<char>) {
        let Some(src) = source() else { return };
        let Ok(event) = CGEvent::new_keyboard_event(src, code, down) else { return };
        event.set_flags(flags(self.mods | extra_mods));
        if let Some(c) = text {
            let mut buf = [0u8; 4];
            event.set_string(c.encode_utf8(&mut buf));
        }
        event.post(CGEventTapLocation::HID);
    }
}

impl super::Injector for MacInjector {
    fn pointer(&mut self, x: u16, y: u16) {
        let (px, py) = self.display.point(x, y);
        self.position = CGPoint::new(px, py);
        // While a button is down the move must be a drag of that button, or windows and selections do not follow.
        let (kind, button) = if self.left {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.right {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if self.other {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };
        self.post_mouse(kind, button, None, None);
    }

    fn button(&mut self, button: Button, down: bool) {
        if down {
            let now = Instant::now();
            let again = self.last_click.is_some_and(|(b, at, p)| {
                b == button
                    && now.duration_since(at) < DOUBLE_CLICK
                    && (p.x - self.position.x).abs() < 4.0
                    && (p.y - self.position.y).abs() < 4.0
            });
            self.click_count = if again { self.click_count + 1 } else { 1 };
            self.last_click = Some((button, now, self.position));
        }
        let (kind, cg_button, number) = match (button, down) {
            (Button::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left, None),
            (Button::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left, None),
            (Button::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right, None),
            (Button::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right, None),
            (Button::Middle, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, Some(2)),
            (Button::Middle, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, Some(2)),
            (Button::Back, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, Some(3)),
            (Button::Back, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, Some(3)),
            (Button::Forward, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, Some(4)),
            (Button::Forward, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, Some(4)),
        };
        match button {
            Button::Left => self.left = down,
            Button::Right => self.right = down,
            _ => self.other = down,
        }
        self.post_mouse(kind, cg_button, Some(self.click_count), number);
    }

    fn wheel(&mut self, axis: u8, delta: i16) {
        let Some(src) = source() else { return };
        // one notch (120) = three lines, as a mouse wheel does on macOS
        let lines = (delta as i32 * 3) / 120;
        let (vertical, horizontal) = if axis == 0 { (lines, 0) } else { (0, lines) };
        let Ok(event) = CGEvent::new_scroll_event(src, ScrollEventUnit::LINE, 2, vertical, horizontal, 0) else { return };
        event.set_flags(flags(self.mods));
        event.post(CGEventTapLocation::HID);
    }

    fn modifier(&mut self, bit: u8, down: bool) {
        let Some(code) = modifier_keycode(bit) else { return };
        if down {
            self.mods |= bit;
        } else {
            self.mods &= !bit;
        }
        self.post_key(code, down, 0, None);
    }

    fn key(&mut self, key: Key, down: bool) {
        match key {
            Key::Named(n) => self.post_key(named_keycode(n), down, 0, None),
            Key::Char(c) => match ansi_keycode(c) {
                Some((code, shift)) => self.post_key(code, down, if shift { Mods::SHIFT } else { 0 }, None),
                // key code 0 with a Unicode string attached types that string, whatever the layout
                None => self.post_key(0, down, 0, Some(c)),
            },
        }
    }

    fn set_display(&mut self, display: ScreenRect) {
        self.display = display;
    }
}
