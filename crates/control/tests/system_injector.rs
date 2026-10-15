//! The real injector against the real desktop. Ignored by default because it does inject input, though as little
//! as can prove the point: the pointer is "moved" to where it already is, and Shift is tapped once.
//!
//!     cargo test -p hdrcast-control --test system_injector -- --ignored

#![cfg(windows)]

use std::thread::sleep;
use std::time::Duration;

use hdrcast_control::inject::{display_rect, Injector, ScreenRect, SystemInjector};
use hdrcast_control::Mods;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetProcessDPIAware};

fn cursor() -> POINT {
    let mut p = POINT::default();
    // SAFETY: `p` is a valid POINT to write to.
    unsafe { GetCursorPos(&mut p) }.expect("GetCursorPos");
    p
}

fn display_under(p: POINT) -> Option<ScreenRect> {
    (1..=16).filter_map(|n| display_rect(&format!(r"\\.\DISPLAY{n}"))).find(|r| {
        let (x, y) = (f64::from(p.x), f64::from(p.y));
        x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
    })
}

#[test]
#[ignore = "injects real input (a pointer move of at most a pixel or two, one Shift tap)"]
fn pointer_positions_land_where_they_are_aimed_and_modifiers_go_down_and_up() {
    // SAFETY: no arguments; only changes how this test process sees coordinates (physical pixels, like the app).
    unsafe {
        let _ = SetProcessDPIAware();
    }
    let before = cursor();
    let rect = display_under(before).expect("the pointer is on some display");
    let to_unit = |v: i32, origin: f64, size: f64| (((f64::from(v) - origin) / (size - 1.0)) * 65535.0).round() as u16;

    let mut injector = SystemInjector::new(rect);
    injector.pointer(to_unit(before.x, rect.x, rect.width), to_unit(before.y, rect.y, rect.height));
    sleep(Duration::from_millis(60));
    let after = cursor();
    // the person at the desk may nudge the mouse meanwhile; an absolute move that mapped wrongly would be far off
    assert!((after.x - before.x).abs() <= 24 && (after.y - before.y).abs() <= 24, "pointer went from {before:?} to {after:?} on {rect:?}");

    // SAFETY: reads the key state.
    let shift_down = || unsafe { GetAsyncKeyState(i32::from(VK_SHIFT.0)) } < 0;
    injector.modifier(Mods::SHIFT, true);
    sleep(Duration::from_millis(30));
    let while_down = shift_down();
    injector.modifier(Mods::SHIFT, false);
    sleep(Duration::from_millis(30));
    assert!(while_down, "Shift did not go down");
    assert!(!shift_down(), "Shift did not come back up");
}

#[test]
fn every_attached_display_has_a_rectangle_and_unknown_names_have_none() {
    assert!(display_rect(r"\\.\DISPLAY1").is_some_and(|r| r.width > 0.0 && r.height > 0.0));
    assert!(display_rect(r"\\.\NOSUCHDISPLAY").is_none());
}
