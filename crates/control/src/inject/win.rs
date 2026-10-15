//! Windows: `SendInput`.

use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, VkKeyScanW, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_APPS, VK_BACK, VK_CONTROL, VK_DELETE, VK_DIVIDE, VK_DOWN,
    VK_END, VK_ESCAPE, VK_F1, VK_HOME, VK_INSERT, VK_LEFT, VK_LWIN, VK_MENU, VK_MULTIPLY, VK_NEXT, VK_NUMPAD0, VK_PRIOR,
    VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SNAPSHOT, VK_SPACE, VK_SUBTRACT, VK_TAB, VK_UP, VK_ADD, VK_DECIMAL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use super::ScreenRect;
use crate::keys::{Button, Key, Mods, NamedKey};

const XBUTTON1: u32 = 1;
const XBUTTON2: u32 = 2;

pub struct WinInjector {
    display: ScreenRect,
    /// Modifiers currently held because the viewer holds them (as opposed to ones tapped to type a character).
    mods: u8,
}

fn send(input: INPUT) {
    // SAFETY: one fully initialised INPUT, and the size SendInput is told is that struct's size.
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: u32) {
    send(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    });
}

fn keyboard(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) {
    send(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    });
}

/// A virtual key, with its scan code filled in (some games read only that).
fn vk_event(vk: VIRTUAL_KEY, extended: bool, down: bool) {
    // SAFETY: a plain lookup in the active keyboard layout.
    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as u16;
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    keyboard(vk, scan, flags);
}

fn modifier_vk(bit: u8) -> Option<VIRTUAL_KEY> {
    match bit {
        Mods::SHIFT => Some(VK_SHIFT),
        Mods::CTRL => Some(VK_CONTROL),
        Mods::ALT => Some(VK_MENU),
        Mods::META => Some(VK_LWIN),
        _ => None,
    }
}

/// The virtual key of a named key, and whether it is one of the "extended" keys (the grey ones beside the
/// numeric keypad share scan codes with it and are told apart by that flag).
fn named_vk(key: NamedKey) -> (VIRTUAL_KEY, bool) {
    use NamedKey::*;
    match key {
        Enter => (VK_RETURN, false),
        Tab => (VK_TAB, false),
        Backspace => (VK_BACK, false),
        Delete => (VK_DELETE, true),
        Insert => (VK_INSERT, true),
        Home => (VK_HOME, true),
        End => (VK_END, true),
        PageUp => (VK_PRIOR, true),
        PageDown => (VK_NEXT, true),
        Escape => (VK_ESCAPE, false),
        Left => (VK_LEFT, true),
        Right => (VK_RIGHT, true),
        Up => (VK_UP, true),
        Down => (VK_DOWN, true),
        F1 | F2 | F3 | F4 | F5 | F6 | F7 | F8 | F9 | F10 | F11 | F12 => {
            (VIRTUAL_KEY(VK_F1.0 + (key as u16 - F1 as u16)), false)
        }
        Kp0 | Kp1 | Kp2 | Kp3 | Kp4 | Kp5 | Kp6 | Kp7 | Kp8 | Kp9 => {
            (VIRTUAL_KEY(VK_NUMPAD0.0 + (key as u16 - Kp0 as u16)), false)
        }
        KpDecimal => (VK_DECIMAL, false),
        KpEnter => (VK_RETURN, true),
        KpAdd => (VK_ADD, false),
        KpSubtract => (VK_SUBTRACT, false),
        KpMultiply => (VK_MULTIPLY, false),
        KpDivide => (VK_DIVIDE, true),
        Print => (VK_SNAPSHOT, true),
        Menu => (VK_APPS, true),
    }
}

impl WinInjector {
    pub fn new(display: ScreenRect) -> Self {
        Self { display, mods: 0 }
    }

    /// A character key. mpv reports the character with Shift already applied ("A", "!"), so the layout tells us
    /// which key and which of Shift / Ctrl / Alt (AltGr) produce it; those not held already are tapped around the
    /// key press. A character the layout cannot type goes in as Unicode text.
    fn char_key(&mut self, c: char, down: bool) {
        if c == ' ' {
            vk_event(VK_SPACE, false, down);
            return;
        }
        let mut units = [0u16; 2];
        let utf16 = c.encode_utf16(&mut units);
        // SAFETY: a plain lookup in the active keyboard layout.
        let scan = if utf16.len() == 1 { unsafe { VkKeyScanW(utf16[0]) } } else { -1 };
        if scan == -1 {
            if down {
                for unit in utf16.iter() {
                    keyboard(VIRTUAL_KEY(0), *unit, KEYEVENTF_UNICODE);
                    keyboard(VIRTUAL_KEY(0), *unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
                }
            }
            return;
        }
        let vk = VIRTUAL_KEY((scan & 0xff) as u16);
        let state = (scan >> 8) as u8;
        // VkKeyScan's shift state: 1 = Shift, 2 = Ctrl, 4 = Alt (the same bits as ours)
        let needed = state & (Mods::SHIFT | Mods::CTRL | Mods::ALT);
        let extra = needed & !self.mods;
        if !down {
            vk_event(vk, false, false);
            return;
        }
        for (bit, _) in Mods::ALL {
            if extra & bit != 0 {
                vk_event(modifier_vk(bit).unwrap(), false, true);
            }
        }
        vk_event(vk, false, true);
        for (bit, _) in Mods::ALL {
            if extra & bit != 0 {
                vk_event(modifier_vk(bit).unwrap(), false, false);
            }
        }
    }
}

impl super::Injector for WinInjector {
    fn pointer(&mut self, x: u16, y: u16) {
        // SAFETY: plain metric queries.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN) as f64,
                GetSystemMetrics(SM_YVIRTUALSCREEN) as f64,
                GetSystemMetrics(SM_CXVIRTUALSCREEN) as f64,
                GetSystemMetrics(SM_CYVIRTUALSCREEN) as f64,
            )
        };
        if vw < 2.0 || vh < 2.0 {
            return;
        }
        let px = self.display.x + (x as f64 / 65535.0) * (self.display.width - 1.0);
        let py = self.display.y + (y as f64 / 65535.0) * (self.display.height - 1.0);
        // absolute coordinates run 0..=65535 across the whole virtual desktop
        let ax = ((px - vx) * 65535.0 / (vw - 1.0)).round() as i32;
        let ay = ((py - vy) * 65535.0 / (vh - 1.0)).round() as i32;
        mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK, ax, ay, 0);
    }

    fn button(&mut self, button: Button, down: bool) {
        let (flags, data) = match (button, down) {
            (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (Button::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
            (Button::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
            (Button::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
            (Button::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
        };
        mouse(flags, 0, 0, data);
    }

    fn wheel(&mut self, axis: u8, delta: i16) {
        let flags = if axis == 0 { MOUSEEVENTF_WHEEL } else { MOUSEEVENTF_HWHEEL };
        mouse(flags, 0, 0, delta as i32 as u32);
    }

    fn modifier(&mut self, bit: u8, down: bool) {
        let Some(vk) = modifier_vk(bit) else { return };
        if down {
            self.mods |= bit;
        } else {
            self.mods &= !bit;
        }
        vk_event(vk, false, down);
    }

    fn key(&mut self, key: Key, down: bool) {
        match key {
            Key::Char(c) => self.char_key(c, down),
            Key::Named(n) => {
                let (vk, extended) = named_vk(n);
                vk_event(vk, extended, down);
            }
        }
    }
}

pub fn display_rect(device_name: &str) -> Option<ScreenRect> {
    struct Search {
        name: Vec<u16>,
        found: Option<RECT>,
    }

    unsafe extern "system" fn each(monitor: HMONITOR, _dc: HDC, _clip: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the &mut Search passed to EnumDisplayMonitors below, alive for the whole enumeration.
        let search = unsafe { &mut *(data.0 as *mut Search) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: `info` is a MONITORINFOEXW whose cbSize says so, which is what lets GetMonitorInfoW fill szDevice.
        let ok = unsafe { GetMonitorInfoW(monitor, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) };
        if ok.as_bool() {
            let len = info.szDevice.iter().position(|c| *c == 0).unwrap_or(info.szDevice.len());
            if info.szDevice[..len] == search.name[..] {
                search.found = Some(info.monitorInfo.rcMonitor);
                return BOOL(0);
            }
        }
        BOOL(1)
    }

    let mut search = Search { name: device_name.encode_utf16().collect(), found: None };
    // SAFETY: the callback only runs during this call and only touches `search`.
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut search as *mut Search as isize));
    }
    search.found.map(|r| ScreenRect {
        x: r.left as f64,
        y: r.top as f64,
        width: (r.right - r.left) as f64,
        height: (r.bottom - r.top) as f64,
    })
}
