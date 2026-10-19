//! The icon in the notification area (the menu bar on macOS), there for as long as HdrCast runs. Closing the
//! window while this computer is available to connect only hides it (docs/design.md §2.7); the icon opens it
//! again, and its menu quits. While the screen is being captured the icon carries the red on-air dot, so a
//! computer whose window is out of sight still shows when somebody is watching it.

use std::sync::Mutex;

use serde::Deserialize;
use tauri::image::Image;
use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

use crate::window;

const OPEN: &str = "tray-open";
const QUIT: &str = "tray-quit";

/// Where the icon lives, for the log.
pub const PLACE: &str = if cfg!(target_os = "macos") { "menu bar" } else { "notification area" };

const ICON: Image<'static> = tauri::include_image!("./icons/32x32.png");

/// The words of the menu and the tooltip in the page's language (the page sends them, `tray_texts`); English
/// until it has.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Texts {
    pub open: String,
    pub quit: String,
    /// The tooltip's second half while available to connect.
    pub available: String,
    /// The same while the screen is being captured.
    pub sharing: String,
}

impl Default for Texts {
    fn default() -> Self {
        Texts {
            open: "Open HdrCast".into(),
            quit: "Quit HdrCast".into(),
            available: "Available to connect".into(),
            sharing: "Sharing this screen".into(),
        }
    }
}

/// What the icon says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Not announced to the group: just the name.
    Idle,
    Available,
    /// The screen is being captured: the on-air dot.
    Sharing,
}

struct Shown {
    texts: Texts,
    mode: Mode,
}

pub struct Tray {
    icon: TrayIcon<Wry>,
    open: MenuItem<Wry>,
    quit: MenuItem<Wry>,
    plain: Image<'static>,
    on_air: Image<'static>,
    shown: Mutex<Shown>,
}

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let texts = Texts::default();
    let open = MenuItem::with_id(app, OPEN, &texts.open, true, None::<&str>)?;
    let quit = MenuItem::with_id(app, QUIT, &texts.quit, true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &PredefinedMenuItem::separator(app)?, &quit])?;
    let plain = ICON.to_owned();
    let on_air = with_dot(&plain);
    let icon = TrayIconBuilder::with_id("hdrcast")
        .icon(plain.clone())
        .tooltip(tooltip(&texts, Mode::Idle))
        .menu(&menu)
        // Windows: a click opens the window and the menu is on the right button; macOS: a click shows the menu
        .show_menu_on_left_click(cfg!(target_os = "macos"))
        .on_menu_event(|app: &AppHandle, event: MenuEvent| match event.id().as_ref() {
            OPEN => window::reveal(app),
            QUIT => crate::quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                if !cfg!(target_os = "macos") {
                    window::reveal(tray.app_handle());
                }
            }
        })
        .build(app)?;
    app.manage(Tray { icon, open, quit, plain, on_air, shown: Mutex::new(Shown { texts, mode: Mode::Idle }) });
    Ok(())
}

fn tooltip(texts: &Texts, mode: Mode) -> String {
    match mode {
        Mode::Idle => "HdrCast".to_string(),
        Mode::Available => format!("HdrCast – {}", texts.available),
        Mode::Sharing => format!("HdrCast – {}", texts.sharing),
    }
}

/// The page's language changed (or the page started).
pub fn set_texts(app: &AppHandle, texts: Texts) {
    let Some(tray) = app.try_state::<Tray>() else { return };
    let mut shown = tray.shown.lock().unwrap();
    if shown.texts == texts {
        return;
    }
    let _ = tray.open.set_text(&texts.open);
    let _ = tray.quit.set_text(&texts.quit);
    let _ = tray.icon.set_tooltip(Some(tooltip(&texts, shown.mode)));
    shown.texts = texts;
}

/// Called with every status tick; touches the icon only when what it says changes.
pub fn show(app: &AppHandle, mode: Mode) {
    let Some(tray) = app.try_state::<Tray>() else { return };
    let mut shown = tray.shown.lock().unwrap();
    if shown.mode == mode {
        return;
    }
    if (shown.mode == Mode::Sharing) != (mode == Mode::Sharing) {
        let image = if mode == Mode::Sharing { tray.on_air.clone() } else { tray.plain.clone() };
        let _ = tray.icon.set_icon(Some(image));
    }
    let _ = tray.icon.set_tooltip(Some(tooltip(&shown.texts, mode)));
    shown.mode = mode;
}

/// Whether the icon is there: without it, a hidden window could not be opened again.
pub fn exists(app: &AppHandle) -> bool {
    app.try_state::<Tray>().is_some()
}

/// Quitting: the icon goes at once, not after the goodbyes to the group.
pub fn remove(app: &AppHandle) {
    if let Some(tray) = app.try_state::<Tray>() {
        let _ = tray.icon.set_visible(false);
    }
}

/// The icon with the red on-air dot in its lower right corner, cut out of the icon by a thin transparent ring
/// so that it stands apart on light and dark taskbars alike.
fn with_dot(icon: &Image<'_>) -> Image<'static> {
    const RED: [f32; 3] = [255.0, 59.0, 48.0]; // --live in ui/styles.css
    let (w, h) = (icon.width(), icon.height());
    let mut rgba = icon.rgba().to_vec();
    let r = w.min(h) as f32 * 0.24;
    let gap = (w.min(h) as f32 / 20.0).max(1.0);
    let (cx, cy) = (w as f32 - r - 0.5, h as f32 - r - 0.5);
    for y in 0..h {
        for x in 0..w {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let cut = (r + gap - d + 0.5).clamp(0.0, 1.0); // dot and ring, antialiased at the outer edge
            if cut <= 0.0 {
                continue;
            }
            let dot = (r - d + 0.5).clamp(0.0, 1.0);
            let i = ((y * w + x) * 4) as usize;
            let under_a = rgba[i + 3] as f32 / 255.0 * (1.0 - cut);
            let a = dot + under_a * (1.0 - dot);
            for c in 0..3 {
                let under = rgba[i + c] as f32;
                rgba[i + c] = if a > 0.0 { ((RED[c] * dot + under * under_a * (1.0 - dot)) / a).round() as u8 } else { 0 };
            }
            rgba[i + 3] = (a * 255.0).round() as u8;
        }
    }
    Image::new_owned(rgba, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_on_air_icon_has_a_red_dot_in_a_clear_ring_and_is_otherwise_the_same() {
        let plain = ICON.to_owned();
        let dotted = with_dot(&plain);
        let (w, px) = (plain.width() as usize, |img: &Image<'_>, x: usize, y: usize| {
            let i = (y * img.width() as usize + x) * 4;
            [img.rgba()[i], img.rgba()[i + 1], img.rgba()[i + 2], img.rgba()[i + 3]]
        });
        let r = w as f32 * 0.24;
        let centre = (w as f32 - r - 0.5) as usize;
        assert_eq!(px(&dotted, centre, centre), [255, 59, 48, 255]);
        assert_eq!(px(&dotted, 2, 2), px(&plain, 2, 2), "far from the dot nothing changes");
        assert_eq!(px(&dotted, w / 2, w / 4), px(&plain, w / 2, w / 4));
        // just outside the dot, towards the middle of the icon: the gap
        let gap_at = (w as f32 - 2.0 * r - 1.5) as usize;
        assert!(px(&plain, gap_at, centre)[3] > 200, "the icon is opaque there");
        assert_eq!(px(&dotted, gap_at, centre)[3], 0);
    }

    #[test]
    fn the_tooltip_names_the_state() {
        let texts = Texts::default();
        assert_eq!(tooltip(&texts, Mode::Idle), "HdrCast");
        assert_eq!(tooltip(&texts, Mode::Available), "HdrCast – Available to connect");
        assert_eq!(tooltip(&texts, Mode::Sharing), "HdrCast – Sharing this screen");
    }
}
