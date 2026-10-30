//! The panel over the player (docs/design.md §3.3), the way Parsec and Moonlight have one. While watching, a small
//! round button sits on mpv's picture; a click opens it into a panel with the numbers of the stream and what a
//! viewer can change while watching: the delay (it joins again), the decoder and the sound (both at once). How the
//! picture is encoded is the host's to say.
//!
//! Ctrl+Alt+O in the player window and the button on the Watch page open and fold the panel too; Esc and the panel's
//! own fold button fold it. The button and the panel can be dragged anywhere: the place is kept relative to the
//! player's picture, so it goes along when the player's window moves or turns full screen, and it is remembered
//! (`overlay.json` beside the settings). Neither takes the keyboard from the player by appearing, and both stay on
//! the player's window: they are on top of everything, so they go out of sight where another window would be
//! behind them (Windows: while another program is in front; macOS: while another window covers their place on the
//! player). On Linux the button sits on the screen HdrCast's window is on instead.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, Theme, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::state::AppState;

pub const LABEL: &str = "overlay";
/// The round button, and the panel (logical pixels; the panel's height is what its page measures).
const BUTTON: f64 = 40.0;
const WIDTH: f64 = 300.0;
const HEIGHT: f64 = 470.0;
/// Between the player's edges and the button (logical pixels).
const MARGIN: f64 = 12.0;
const TICK: Duration = Duration::from_millis(100);
const SPOT_FILE: &str = "overlay.json";

#[derive(Default)]
pub struct Overlay {
    expanded: AtomicBool,
    /// Counts the windows made: the window is laid out from scratch when it is a new one.
    made: AtomicU64,
    /// The thread that keeps the window on the player runs (one at a time).
    following: AtomicBool,
    /// Where the window is and what it shows. The thread and the commands both lay it out; never taken on the
    /// main thread (laying out waits for it).
    keeper: Mutex<Keeper>,
    /// Held while the window is made.
    creating: Mutex<()>,
}

/// A rectangle on the screen, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn right(self) -> i32 {
        self.x + self.w
    }

    fn bottom(self) -> i32 {
        self.y + self.h
    }

    /// Moved as little as possible to lie within `bounds` (its top left stays inside when it is larger).
    fn within(self, bounds: Rect) -> Rect {
        Rect {
            x: self.x.min(bounds.right() - self.w).max(bounds.x),
            y: self.y.min(bounds.bottom() - self.h).max(bounds.y),
            ..self
        }
    }
}

/// Where the button sits on the player's picture: 0..1 across the room the margins leave (1, 0: top right).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Spot {
    x: f64,
    y: f64,
}

impl Default for Spot {
    fn default() -> Self {
        Spot { x: 1.0, y: 0.0 }
    }
}

impl Spot {
    fn path() -> PathBuf {
        hdrcast_core::common::app_paths::config_dir().join(SPOT_FILE)
    }

    fn load() -> Spot {
        std::fs::read(Self::path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Spot>(&bytes).ok())
            .filter(|s| s.x.is_finite() && s.y.is_finite())
            .map(|s| Spot { x: s.x.clamp(0.0, 1.0), y: s.y.clamp(0.0, 1.0) })
            .unwrap_or_default()
    }

    fn save(self) {
        if let Ok(bytes) = serde_json::to_vec(&self) {
            let _ = std::fs::write(Self::path(), bytes);
        }
    }
}

/// Physical pixels for `logical` ones at `scale`.
fn px(logical: f64, scale: f64) -> i32 {
    (logical * scale).round() as i32
}

/// The button at `spot` on the player's picture `area`.
fn button_at(area: Rect, spot: Spot, scale: f64) -> Rect {
    let (size, margin) = (px(BUTTON, scale), px(MARGIN, scale));
    let room_x = (area.w - 2 * margin - size).max(0);
    let room_y = (area.h - 2 * margin - size).max(0);
    Rect {
        x: area.x + margin + (spot.x * f64::from(room_x)).round() as i32,
        y: area.y + margin + (spot.y * f64::from(room_y)).round() as i32,
        w: size,
        h: size,
    }
}

/// The spot of a button whose top left is at `x`, `y` (kept on the picture).
fn spot_of(x: i32, y: i32, area: Rect, scale: f64) -> Spot {
    let (size, margin) = (px(BUTTON, scale), px(MARGIN, scale));
    let fraction = |at: i32, start: i32, length: i32| {
        let room = length - 2 * margin - size;
        if room <= 0 {
            0.5
        } else {
            (f64::from(at - start - margin) / f64::from(room)).clamp(0.0, 1.0)
        }
    };
    Spot { x: fraction(x, area.x, area.w), y: fraction(y, area.y, area.h) }
}

/// The panel opens from the button towards the middle of the picture: leftwards from the button's right edge when
/// the button is in the right half, upwards when it is in the lower half. It stays on the screen.
fn panel_from(button: Rect, spot: Spot, w: i32, h: i32, screen: Rect) -> Rect {
    let x = if spot.x >= 0.5 { button.right() - w } else { button.x };
    let y = if spot.y >= 0.5 { button.bottom() - h } else { button.y };
    Rect { x, y, w, h }.within(screen)
}

/// Where the button goes when the panel (opened from the button at `before`) was dragged to `panel`: the inverse of
/// [`panel_from`]. For each axis the button goes to the panel's far edge (right, bottom) or its near one, whichever
/// agrees with the half the button then lands in; a panel across the middle agrees with both and keeps its side.
fn spot_of_panel(panel: Rect, area: Rect, scale: f64, before: Spot) -> Spot {
    let size = px(BUTTON, scale);
    let far = Spot {
        x: spot_of(panel.right() - size, panel.y, area, scale).x,
        y: spot_of(panel.x, panel.bottom() - size, area, scale).y,
    };
    let near = spot_of(panel.x, panel.y, area, scale);
    let pick = |far: f64, near: f64, was_far: bool| match was_far {
        true if far >= 0.5 => far,
        true => near,
        false if near < 0.5 => near,
        false => far,
    };
    Spot { x: pick(far.x, near.x, before.x >= 0.5), y: pick(far.y, near.y, before.y >= 0.5) }
}

pub fn is_expanded(app: &AppState) -> bool {
    app.overlay.expanded.load(Ordering::SeqCst)
}

fn window(app: &AppState) -> Option<WebviewWindow> {
    app.handle.get()?.get_webview_window(LABEL)
}

/// Watching started: the window is made (hidden), and the button shows once the player's window is there.
pub fn start(app: &Arc<AppState>) {
    let Some(handle) = app.handle.get() else { return };
    {
        let _creating = app.overlay.creating.lock().unwrap();
        if handle.get_webview_window(LABEL).is_none() {
            if let Err(e) = create(handle) {
                app.log(&format!("[app] the button over the player cannot be made: {e}"));
                return;
            }
            app.overlay.made.fetch_add(1, Ordering::SeqCst);
        }
    }
    follow(app);
}

/// Watching ended: the window goes, web view and all.
pub fn close(app: &AppState) {
    app.overlay.expanded.store(false, Ordering::SeqCst);
    if let Some(window) = window(app) {
        let _ = window.destroy();
    }
}

pub fn toggle(app: &AppState) {
    set_expanded(app, !is_expanded(app));
}

/// Opens the button into the panel, or folds the panel back into the button. Not on the main thread.
pub fn set_expanded(app: &AppState, expanded: bool) {
    if app.overlay.expanded.swap(expanded, Ordering::SeqCst) == expanded {
        return;
    }
    if let Some(handle) = app.handle.get() {
        let _ = handle.emit_to(LABEL, "overlay", json!({ "expanded": expanded }));
    }
    relayout(app);
}

/// The page measured the panel (CSS pixels, which are logical ones). Not on the main thread.
pub fn fit(app: &AppState, height: f64) {
    app.overlay.keeper.lock().unwrap().panel_height = Some(height.clamp(120.0, 900.0));
    relayout(app);
}

/// Gives the keyboard back to the player after something of HdrCast's was used (Windows: HdrCast is in front at that
/// moment, so Windows lets it hand the foreground on). A full-screen player covers the taskbar again then.
pub fn focus_player(app: &AppState) {
    if let Some(pid) = crate::watch::player_pid(app) {
        platform::activate(pid);
    }
}

fn relayout(app: &AppState) {
    if let Some(window) = window(app) {
        app.overlay.keeper.lock().unwrap().tick(app, &window);
    }
}

fn create(handle: &AppHandle) -> tauri::Result<WebviewWindow> {
    let builder = WebviewWindowBuilder::new(handle, LABEL, WebviewUrl::App("overlay.html".into()))
        .title("HdrCast")
        .inner_size(BUTTON, BUTTON)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .decorations(false)
        // the page draws the round button and the rounded panel; around them the picture shows
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        // over a picture it is dark whatever the app's appearance
        .theme(Some(Theme::Dark))
        // shown by `set_visible`, which never takes the keyboard from the player
        .visible(false)
        .focused(false);
    #[cfg(target_os = "macos")]
    let builder = builder
        // on every Space, so that it is there wherever the player's window is
        .visible_on_all_workspaces(true)
        // the first click on it counts, also while the player is the application in front
        .accept_first_mouse(true);
    crate::window::web_view_profile(builder).build()
}

/// Starts the thread that keeps the window on the player, unless it runs already. It ends with the window.
fn follow(app: &Arc<AppState>) {
    if app.overlay.following.swap(true, Ordering::SeqCst) {
        return;
    }
    let weak = Arc::downgrade(app);
    let spawned = std::thread::Builder::new().name("overlay".into()).spawn(move || loop {
        while let Some(app) = weak.upgrade() {
            let Some(window) = window(&app) else { break };
            app.overlay.keeper.lock().unwrap().tick(&app, &window);
            drop(app);
            std::thread::sleep(TICK);
        }
        let Some(app) = weak.upgrade() else { return };
        app.overlay.following.store(false, Ordering::SeqCst);
        // a window made just as this thread was leaving: it carries on
        if window(&app).is_none() || app.overlay.following.swap(true, Ordering::SeqCst) {
            return;
        }
    });
    if spawned.is_err() {
        app.overlay.following.store(false, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Keeper {
    made: u64,
    handle: Option<platform::Handle>,
    spot: Option<Spot>,
    /// The panel's height from its page (logical pixels).
    panel_height: Option<f64>,
    /// The player's picture as last seen: it stays while the player's window is gone for a moment (joining again).
    area: Option<Rect>,
    /// What the window was laid out from last, where it goes, and how often in a row it was not found there.
    laid_out: Option<(Rect, Spot, bool, Option<f64>)>,
    placed: Option<Rect>,
    misses: u8,
    shown: bool,
    /// The user is dragging the window.
    dragging: bool,
}

impl Keeper {
    fn tick(&mut self, app: &AppState, window: &WebviewWindow) {
        let made = app.overlay.made.load(Ordering::SeqCst);
        if made != self.made {
            // a new window: nothing about the old one holds
            *self = Keeper { made, spot: self.spot, panel_height: self.panel_height, ..Keeper::default() };
        }
        if self.handle.is_none() {
            self.handle = platform::handle(window);
        }
        let Some(handle) = self.handle.as_ref().cloned() else { return };
        if platform::being_dragged(&handle) {
            self.dragging = true;
            return;
        }
        let player = crate::watch::player_pid(app);
        let seen = player.and_then(|pid| platform::player_area(&handle, pid)).or_else(|| {
            // where the player's window cannot be looked for, the screen stands in for it
            (!platform::FINDS_PLAYER && player.is_some()).then(|| screen_area(app, window)).flatten()
        });
        if seen.is_some() {
            self.area = seen;
        }
        let expanded = is_expanded(app);
        if std::mem::take(&mut self.dragging) {
            self.dropped(app, window, &handle, expanded);
        }

        // the panel may show before there is a player window (on the screen's corner); the button only on one
        let area = self.area.or_else(|| expanded.then(|| screen_area(app, window)).flatten());
        if let Some(area) = area {
            let spot = *self.spot.get_or_insert_with(Spot::load);
            let key = (area, spot, expanded, self.panel_height);
            if self.laid_out != Some(key) || self.placed.is_none() {
                self.laid_out = Some(key);
                let (scale, screen) = monitor_at(window, area);
                let button = button_at(area, spot, scale);
                self.placed = Some(if expanded {
                    let height = px(self.panel_height.unwrap_or(HEIGHT), scale);
                    panel_from(button, spot, px(WIDTH, scale), height, screen)
                } else {
                    button
                });
                self.misses = 0;
            }
            // Put there, and again while it is not there: a new window only settles its frame after its first move,
            // and a move to a screen with another scale resizes it. A few tries per layout, not a fight.
            if let Some(rect) = self.placed {
                if platform::client_rect(&handle) == Some(rect) {
                    self.misses = 0;
                } else if self.misses < 5 {
                    self.misses += 1;
                    platform::put(&handle, rect);
                }
            }
        }

        // nothing in front (a moment during a switch): as it was
        let wanted =
            area.is_some() && (seen.is_some() || expanded) && platform::in_front(&handle, player, self.placed).unwrap_or(self.shown);
        if wanted != self.shown {
            self.shown = wanted;
            platform::show(&handle, wanted);
        }
    }

    /// A drag ended: the button's spot is where it (or the panel) was dropped, kept on the picture.
    fn dropped(&mut self, app: &AppState, window: &WebviewWindow, handle: &platform::Handle, expanded: bool) {
        let (Some(area), Some(now)) = (self.area, platform::client_rect(handle)) else { return };
        if self.placed == Some(now) {
            // pressed, not moved (a Mac reports every press on it: a click on the button or in the panel)
            return;
        }
        let (scale, _) = monitor_at(window, area);
        let before = self.spot.unwrap_or_default();
        let spot = if expanded { spot_of_panel(now, area, scale, before) } else { spot_of(now.x, now.y, area, scale) };
        self.spot = Some(spot);
        spot.save();
        // laid out again from the spot: dropped off the picture, it comes back onto it
        self.placed = None;
        self.laid_out = None;
        if !expanded && platform::is_front(handle) {
            // the button was only moved: the keyboard goes back to the player
            focus_player(app);
        }
    }
}

/// The scale of the screen at the middle of `area` (the one layout works in, see [`platform::layout_scale`]), and the
/// part of that screen windows may use.
fn monitor_at(window: &WebviewWindow, area: Rect) -> (f64, Rect) {
    let monitor = window.monitor_from_point(f64::from(area.x + area.w / 2), f64::from(area.y + area.h / 2)).ok().flatten();
    match monitor {
        Some(m) => (platform::layout_scale(&m), platform::work_area(&m)),
        None => (1.0, area),
    }
}

/// The screen HdrCast's window is on, for when there is no player window to sit on.
fn screen_area(app: &AppState, window: &WebviewWindow) -> Option<Rect> {
    let main = app.handle.get()?.get_webview_window(crate::window::LABEL);
    let monitor = main
        .and_then(|w| w.current_monitor().ok().flatten())
        .or_else(|| window.primary_monitor().ok().flatten())?;
    Some(platform::work_area(&monitor))
}

/// The part of a screen windows may use, in the window library's physical pixels.
#[cfg(not(target_os = "macos"))]
fn physical_work_area(monitor: &tauri::Monitor) -> Rect {
    let work = monitor.work_area();
    Rect { x: work.position.x, y: work.position.y, w: work.size.width as i32, h: work.size.height as i32 }
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;

    use tauri::{Monitor, WebviewWindow};
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetAncestor, GetClientRect, GetForegroundWindow, GetGUIThreadInfo, GetWindowRect,
        GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetForegroundWindow, SetWindowPos, ShowWindowAsync,
        GA_ROOTOWNER, GUITHREADINFO, GUI_INMOVESIZE, HWND_TOPMOST, SWP_NOACTIVATE, SW_HIDE, SW_SHOWNOACTIVATE,
    };

    use super::Rect;

    pub const FINDS_PLAYER: bool = true;

    /// The window's handle, as a number: a handle is not `Send`.
    pub type Handle = isize;

    pub fn handle(window: &WebviewWindow) -> Option<Handle> {
        window.hwnd().ok().map(|hwnd| hwnd.0 as isize)
    }

    /// Layout works in physical pixels here: sizes are scaled for the screen they are on.
    pub fn layout_scale(monitor: &Monitor) -> f64 {
        monitor.scale_factor()
    }

    pub fn work_area(monitor: &Monitor) -> Rect {
        super::physical_work_area(monitor)
    }

    fn hwnd(handle: &Handle) -> HWND {
        HWND(*handle as *mut c_void)
    }

    fn pid_of(hwnd: HWND) -> u32 {
        let mut pid = 0u32;
        // SAFETY: a plain query; a handle that is gone (or null) leaves `pid` at 0.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32)) };
        pid
    }

    /// Where the window's client area is on the screen.
    fn client_of(hwnd: HWND) -> Option<Rect> {
        let mut rect = RECT::default();
        let mut corner = POINT { x: 0, y: 0 };
        // SAFETY: queries on a window handle; one that has closed meanwhile fails them.
        unsafe {
            if GetClientRect(hwnd, &mut rect).is_err() || !ClientToScreen(hwnd, &mut corner).as_bool() {
                return None;
            }
        }
        Some(Rect { x: corner.x, y: corner.y, w: rect.right, h: rect.bottom })
    }

    /// The largest visible window of process `pid`: mpv's video window.
    fn player_window(pid: u32) -> Option<HWND> {
        struct Search {
            pid: u32,
            best: Option<(HWND, i64)>,
        }

        unsafe extern "system" fn each(hwnd: HWND, data: LPARAM) -> BOOL {
            // SAFETY: `data` is the &mut Search passed to EnumWindows below, alive for the whole enumeration.
            let search = unsafe { &mut *(data.0 as *mut Search) };
            // SAFETY: a query on the window handle EnumWindows hands over.
            if pid_of(hwnd) == search.pid && unsafe { IsWindowVisible(hwnd) }.as_bool() {
                if let Some(client) = client_of(hwnd) {
                    let size = i64::from(client.w) * i64::from(client.h);
                    if search.best.is_none_or(|(_, best)| size > best) {
                        search.best = Some((hwnd, size));
                    }
                }
            }
            BOOL(1)
        }

        let mut search = Search { pid, best: None };
        // SAFETY: the callback only runs during this call and only touches `search`.
        unsafe {
            let _ = EnumWindows(Some(each), LPARAM(&mut search as *mut Search as isize));
        }
        search.best.map(|(hwnd, _)| hwnd)
    }

    /// Where mpv (process `pid`) shows the picture on the screen; None while it has no window or is minimized.
    pub fn player_area(_handle: &Handle, pid: u32) -> Option<Rect> {
        let hwnd = player_window(pid)?;
        // SAFETY: a query on a window handle EnumWindows just returned.
        if unsafe { IsIconic(hwnd) }.as_bool() {
            return None;
        }
        client_of(hwnd)
    }

    pub fn client_rect(handle: &Handle) -> Option<Rect> {
        client_of(hwnd(handle))
    }

    /// Puts the window's client area at `rect`, on top, without taking the keyboard. Its outer edges are not where
    /// it draws (Windows keeps an invisible frame around some windows), so they are measured first.
    pub fn put(handle: &Handle, rect: Rect) {
        let hwnd = hwnd(handle);
        let mut outer = RECT::default();
        let Some(inner) = client_of(hwnd) else { return };
        // SAFETY: the window's own handle; SetWindowPos waits for the thread that owns the window, which never
        // waits for this one.
        unsafe {
            if GetWindowRect(hwnd, &mut outer).is_err() {
                return;
            }
            let (left, top) = (inner.x - outer.left, inner.y - outer.top);
            let (right, bottom) = (outer.right - inner.right(), outer.bottom - inner.bottom());
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                rect.x - left,
                rect.y - top,
                rect.w + left + right,
                rect.h + top + bottom,
                SWP_NOACTIVATE,
            );
        }
    }

    pub fn show(handle: &Handle, visible: bool) {
        // SAFETY: the window's own handle. Async: this thread does not wait for the one that owns the window.
        unsafe {
            let _ = ShowWindowAsync(hwnd(handle), if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
    }

    /// The user is moving the window (the system's move loop runs for it).
    pub fn being_dragged(handle: &Handle) -> bool {
        let hwnd = hwnd(handle);
        let mut info = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        // SAFETY: plain queries; `info` says its own size.
        unsafe {
            let thread = GetWindowThreadProcessId(hwnd, None);
            GetGUIThreadInfo(thread, &mut info).is_ok() && info.flags.contains(GUI_INMOVESIZE) && info.hwndMoveSize == hwnd
        }
    }

    /// The window itself is in front (it was just used).
    pub fn is_front(handle: &Handle) -> bool {
        // SAFETY: a plain query.
        unsafe { GetForegroundWindow() == hwnd(handle) }
    }

    /// Whether the window in front belongs to the player (process `player`) or to HdrCast itself: its windows, and
    /// anything they own. None while no window is in front (for a moment during a switch).
    pub fn in_front(_handle: &Handle, player: Option<u32>, _at: Option<Rect>) -> Option<bool> {
        // SAFETY: plain queries.
        let (front, owner) = unsafe {
            let front = GetForegroundWindow();
            (front, GetAncestor(front, GA_ROOTOWNER))
        };
        if front.is_invalid() {
            return None;
        }
        let ours = |pid: u32| pid != 0 && (pid == std::process::id() || Some(pid) == player);
        Some(ours(pid_of(front)) || ours(pid_of(owner)))
    }

    pub fn activate(pid: u32) {
        if let Some(hwnd) = player_window(pid) {
            // SAFETY: a window handle EnumWindows just returned.
            unsafe {
                let _ = SetForegroundWindow(hwnd);
            }
        }
    }
}

/// macOS: the window server's list of windows (CoreGraphics) and AppKit. Places here are in points of the global
/// display space (0, 0 at the top left of the main display), which is what the window library's logical positions
/// are on a Mac: the layout works in points, whatever the screen's scale.
#[cfg(target_os = "macos")]
mod platform {
    use std::sync::{Arc, Mutex};

    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWindow};
    use objc2_core_foundation::{CFDictionary, CFNumber, CFString, CFType, CGRect};
    use objc2_core_graphics::{
        kCGWindowAlpha, kCGWindowBounds, kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerPID, CGEvent, CGEventSource,
        CGEventSourceStateID, CGMouseButton, CGRectMakeWithDictionaryRepresentation, CGWindowListCopyWindowInfo,
        CGWindowListOption,
    };
    use tauri::{LogicalPosition, LogicalSize, Manager, Monitor, WebviewWindow};

    use super::Rect;

    pub const FINDS_PLAYER: bool = true;

    #[derive(Default)]
    struct Seen {
        /// The window server's number for the player's window, as last found.
        player_window: Option<u32>,
        /// The left mouse button was down at the last look.
        button_down: bool,
        /// It went down over this window.
        pressed_here: bool,
    }

    #[derive(Clone)]
    pub struct Handle {
        window: WebviewWindow,
        seen: Arc<Mutex<Seen>>,
    }

    pub fn handle(window: &WebviewWindow) -> Option<Handle> {
        Some(Handle { window: window.clone(), seen: Arc::default() })
    }

    pub fn layout_scale(_monitor: &Monitor) -> f64 {
        1.0
    }

    /// The part of the screen windows may use, in points.
    pub fn work_area(monitor: &Monitor) -> Rect {
        let (work, scale) = (monitor.work_area(), monitor.scale_factor());
        let points = |v: f64| (v / scale).round() as i32;
        Rect {
            x: points(f64::from(work.position.x)),
            y: points(f64::from(work.position.y)),
            w: points(f64::from(work.size.width)),
            h: points(f64::from(work.size.height)),
        }
    }

    /// A window on the screen, as the window server describes it.
    struct Listed {
        number: u32,
        pid: i64,
        layer: i64,
        alpha: f64,
        bounds: Rect,
    }

    /// The value under `key` in a window's description.
    fn value<'a>(info: &'a CFDictionary, key: &CFString) -> Option<&'a CFType> {
        // SAFETY: the key is a CFString, the key type of these dictionaries; the value (if any) is a CF object the
        // dictionary keeps alive for as long as it lives itself.
        unsafe { info.value((key as *const CFString).cast()).cast::<CFType>().as_ref() }
    }

    fn number(info: &CFDictionary, key: &CFString) -> Option<f64> {
        value(info, key)?.downcast_ref::<CFNumber>()?.as_f64()
    }

    fn listed(info: &CFDictionary) -> Option<Listed> {
        // SAFETY (the keys): constant CFStrings exported by CoreGraphics, valid for the life of the process.
        let (number_key, pid_key, layer_key, alpha_key, bounds_key) =
            unsafe { (kCGWindowNumber, kCGWindowOwnerPID, kCGWindowLayer, kCGWindowAlpha, kCGWindowBounds) };
        let bounds = value(info, bounds_key)?.downcast_ref::<CFDictionary>()?;
        let mut rect = CGRect::default();
        // SAFETY: `bounds` is a rectangle in the window server's dictionary form (X, Y, Width, Height), and `rect` a
        // valid place for the result.
        if !unsafe { CGRectMakeWithDictionaryRepresentation(Some(bounds), &mut rect) } {
            return None;
        }
        Some(Listed {
            number: number(info, number_key)? as u32,
            pid: number(info, pid_key)? as i64,
            layer: number(info, layer_key).unwrap_or(0.0) as i64,
            alpha: number(info, alpha_key).unwrap_or(1.0),
            bounds: Rect {
                x: rect.origin.x.round() as i32,
                y: rect.origin.y.round() as i32,
                w: rect.size.width.round() as i32,
                h: rect.size.height.round() as i32,
            },
        })
    }

    /// The windows on the screen, frontmost first; with `above`, only those in front of that one.
    fn windows(above: Option<u32>) -> Vec<Listed> {
        let which = match above {
            Some(_) => CGWindowListOption::OptionOnScreenAboveWindow,
            None => CGWindowListOption::OptionOnScreenOnly,
        };
        let Some(list) = CGWindowListCopyWindowInfo(which | CGWindowListOption::ExcludeDesktopElements, above.unwrap_or(0))
        else {
            return Vec::new();
        };
        (0..list.count())
            .filter_map(|i| {
                // SAFETY: `i` is in bounds; the array holds one CFDictionary per window, which it keeps alive, and the
                // element is only taken as some CF object until its type is checked.
                let item = unsafe { list.value_at_index(i).cast::<CFType>().as_ref() }?;
                listed(item.downcast_ref::<CFDictionary>()?)
            })
            .collect()
    }

    /// Where mpv (process `pid`) shows the picture: its largest window in the normal layer that is on the screen
    /// (none while it is minimized, on another Space, or has no window yet). mpv draws the picture under its title
    /// bar as well (the bar only shows while the mouse moves near it), so the whole window is the picture.
    pub fn player_area(handle: &Handle, pid: u32) -> Option<Rect> {
        let player = windows(None)
            .into_iter()
            .filter(|w| w.pid == i64::from(pid) && w.layer == 0 && w.alpha > 0.0 && w.bounds.w > 0 && w.bounds.h > 0)
            .max_by_key(|w| i64::from(w.bounds.w) * i64::from(w.bounds.h));
        handle.seen.lock().unwrap().player_window = player.as_ref().map(|w| w.number);
        player.map(|w| w.bounds)
    }

    pub fn client_rect(handle: &Handle) -> Option<Rect> {
        let window = &handle.window;
        let scale = window.scale_factor().ok()?;
        let position = window.inner_position().ok()?.to_logical::<f64>(scale);
        let size = window.inner_size().ok()?.to_logical::<f64>(scale);
        Some(Rect {
            x: position.x.round() as i32,
            y: position.y.round() as i32,
            w: size.width.round() as i32,
            h: size.height.round() as i32,
        })
    }

    pub fn put(handle: &Handle, rect: Rect) {
        let _ = handle.window.set_size(LogicalSize::new(f64::from(rect.w.max(1)), f64::from(rect.h.max(1))));
        let _ = handle.window.set_position(LogicalPosition::new(f64::from(rect.x), f64::from(rect.y)));
    }

    /// Shown without taking the keyboard: the window library's `show` makes the window the key one, so it is put up
    /// with `orderFrontRegardless` instead (and taken down with `orderOut:`).
    pub fn show(handle: &Handle, visible: bool) {
        let app = handle.window.app_handle().clone();
        let _ = handle.window.run_on_main_thread(move || {
            // looked up on the main thread, which is also where windows are closed: never one that is gone
            let Some(window) = app.get_webview_window(super::LABEL) else { return };
            let Ok(ns_window) = window.ns_window() else { return };
            // SAFETY: the NSWindow of the panel's window, which is open (just looked up, on the thread that closes
            // windows); AppKit is used on the main thread.
            let ns_window = unsafe { &*ns_window.cast::<NSWindow>() };
            if visible {
                ns_window.orderFrontRegardless();
            } else {
                ns_window.orderOut(None);
            }
        });
    }

    /// The left mouse button went down over this window and is still down: a drag of it (the system moves it), or a
    /// click in it. Neither is laid out over.
    pub fn being_dragged(handle: &Handle) -> bool {
        let down = CGEventSource::button_state(CGEventSourceStateID::CombinedSessionState, CGMouseButton::Left);
        let fresh = down && !handle.seen.lock().unwrap().button_down;
        // where the press was, near enough: the pointer now (the window follows it while it is dragged)
        let here = fresh && {
            let pointer = CGEvent::new(None).map(|event| CGEvent::location(Some(&*event)));
            match (pointer, client_rect(handle)) {
                (Some(p), Some(r)) => {
                    p.x >= f64::from(r.x) && p.x < f64::from(r.right()) && p.y >= f64::from(r.y) && p.y < f64::from(r.bottom())
                }
                _ => false,
            }
        };
        let mut seen = handle.seen.lock().unwrap();
        if fresh {
            seen.pressed_here = here;
        }
        seen.button_down = down;
        down && seen.pressed_here
    }

    fn is_active(pid: u32) -> bool {
        libc::pid_t::try_from(pid)
            .ok()
            .and_then(NSRunningApplication::runningApplicationWithProcessIdentifier)
            .is_some_and(|app| app.isActive())
    }

    /// HdrCast is the application in front (its window was just used).
    pub fn is_front(_handle: &Handle) -> bool {
        NSRunningApplication::currentApplication().isActive()
    }

    /// No other window lies over the panel's place `at` on the player's window: the windows in front of the player's
    /// in the normal layer are looked at (HdrCast's main window counts too; this one floats above that layer). Without
    /// a window of the player to look at, as on Windows: up while the player or HdrCast is the application in front.
    pub fn in_front(handle: &Handle, player: Option<u32>, at: Option<Rect>) -> Option<bool> {
        let player_window = handle.seen.lock().unwrap().player_window;
        let (Some(pid), Some(number), Some(at)) = (player, player_window, at) else {
            return Some(is_active(std::process::id()) || player.is_some_and(is_active));
        };
        let covered = windows(Some(number)).iter().any(|w| {
            w.layer == 0
                && w.alpha > 0.0
                && w.pid != i64::from(pid)
                && w.bounds.x < at.right()
                && at.x < w.bounds.right()
                && w.bounds.y < at.bottom()
                && at.y < w.bounds.bottom()
        });
        Some(!covered)
    }

    /// Hands the keyboard to the player. HdrCast is the application in front at that moment (its panel was just
    /// used), which is when macOS lets it pass that on.
    #[allow(deprecated)] // "ignoring other apps": without effect from macOS 14 on, where an app in front may do this anyway
    pub fn activate(pid: u32) {
        if let Some(app) = libc::pid_t::try_from(pid).ok().and_then(NSRunningApplication::runningApplicationWithProcessIdentifier) {
            let _ = app.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);
        }
    }
}

/// Elsewhere the window library does the work: the button goes on the screen's corner and stays up while watching.
#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use tauri::{Monitor, PhysicalPosition, PhysicalSize, WebviewWindow};

    use super::Rect;

    pub const FINDS_PLAYER: bool = false;

    pub type Handle = WebviewWindow;

    pub fn handle(window: &WebviewWindow) -> Option<Handle> {
        Some(window.clone())
    }

    pub fn layout_scale(monitor: &Monitor) -> f64 {
        monitor.scale_factor()
    }

    pub fn work_area(monitor: &Monitor) -> Rect {
        super::physical_work_area(monitor)
    }

    pub fn player_area(_window: &Handle, _pid: u32) -> Option<Rect> {
        None
    }

    pub fn client_rect(window: &Handle) -> Option<Rect> {
        let (position, size) = (window.inner_position().ok()?, window.inner_size().ok()?);
        Some(Rect { x: position.x, y: position.y, w: size.width as i32, h: size.height as i32 })
    }

    pub fn put(window: &Handle, rect: Rect) {
        let _ = window.set_size(PhysicalSize::new(rect.w.max(1) as u32, rect.h.max(1) as u32));
        let _ = window.set_position(PhysicalPosition::new(rect.x, rect.y));
    }

    pub fn show(window: &Handle, visible: bool) {
        let _ = if visible { window.show() } else { window.hide() };
    }

    pub fn being_dragged(_window: &Handle) -> bool {
        false
    }

    pub fn is_front(_window: &Handle) -> bool {
        false
    }

    pub fn in_front(_window: &Handle, _player: Option<u32>, _at: Option<Rect>) -> Option<bool> {
        Some(true)
    }

    pub fn activate(_pid: u32) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect { x: 960, y: 525, w: 1920, h: 1080 };

    #[test]
    fn the_button_starts_at_the_top_right_and_its_spot_comes_back() {
        let button = button_at(AREA, Spot::default(), 1.5);
        // 40 px at 150 % is 60, the margin 18
        assert_eq!(button, Rect { x: 960 + 1920 - 18 - 60, y: 525 + 18, w: 60, h: 60 });
        assert_eq!(spot_of(button.x, button.y, AREA, 1.5), Spot::default());
        let middle = Spot { x: 0.25, y: 0.75 };
        let b = button_at(AREA, middle, 1.0);
        let back = spot_of(b.x, b.y, AREA, 1.0);
        assert!((back.x - 0.25).abs() < 0.001 && (back.y - 0.75).abs() < 0.001, "{back:?}");
        // dropped off the picture: back onto its edge
        assert_eq!(spot_of(AREA.x - 500, AREA.bottom() + 300, AREA, 1.0), Spot { x: 0.0, y: 1.0 });
    }

    #[test]
    fn the_panel_opens_towards_the_middle_and_stays_on_the_screen() {
        let screen = Rect { x: 0, y: 0, w: 3840, h: 2110 };
        let top_right = Spot::default();
        let button = button_at(AREA, top_right, 1.5);
        let panel = panel_from(button, top_right, 450, 700, screen);
        assert_eq!((panel.right(), panel.y), (button.right(), button.y), "from the button's top right corner");
        let bottom_left = Spot { x: 0.0, y: 1.0 };
        let button = button_at(AREA, bottom_left, 1.5);
        let panel = panel_from(button, bottom_left, 450, 700, screen);
        assert_eq!((panel.x, panel.bottom()), (button.x, button.bottom()), "upwards from its bottom left corner");
        // a player at the screen's top: the panel does not go above it
        let high = Rect { x: 0, y: 0, w: 640, h: 360 };
        let button = button_at(high, Spot { x: 0.0, y: 1.0 }, 1.0);
        assert_eq!(panel_from(button, Spot { x: 0.0, y: 1.0 }, 300, 470, screen).y, 0);
    }

    #[test]
    fn a_dragged_panel_takes_the_button_along() {
        let screen = Rect { x: 0, y: 0, w: 3840, h: 2110 };
        for spot in [Spot::default(), Spot { x: 0.0, y: 1.0 }, Spot { x: 0.1, y: 0.2 }, Spot { x: 0.8, y: 0.9 }] {
            let panel = panel_from(button_at(AREA, spot, 1.5), spot, 450, 700, screen);
            // not dragged at all: the button stays (the panel spans the middle for some of these)
            let back = spot_of_panel(panel, AREA, 1.5, spot);
            assert!((back.x - spot.x).abs() < 0.01 && (back.y - spot.y).abs() < 0.01, "{spot:?} -> {back:?}");
            assert_eq!(panel_from(button_at(AREA, back, 1.5), back, 450, 700, screen), panel);
        }
        // a panel dragged from the top right to the top left opens from the left there: it does not jump
        let dropped = Rect { x: AREA.x + 100, y: AREA.y + 100, w: 450, h: 700 };
        let spot = spot_of_panel(dropped, AREA, 1.5, Spot::default());
        assert!(spot.x < 0.5 && spot.y < 0.5, "{spot:?}");
        assert_eq!(panel_from(button_at(AREA, spot, 1.5), spot, 450, 700, screen), dropped);
        // one dragged down past the picture's bottom: the button goes onto the bottom edge, the panel opens upwards
        // from it (back onto the picture)
        let dropped = Rect { x: AREA.right() - 600, y: AREA.y + 600, w: 450, h: 700 };
        let spot = spot_of_panel(dropped, AREA, 1.5, Spot::default());
        assert!(spot.x >= 0.5 && spot.y == 1.0, "{spot:?}");
        let panel = panel_from(button_at(AREA, spot, 1.5), spot, 450, 700, screen);
        assert_eq!((panel.right(), panel.bottom()), (dropped.right(), AREA.bottom() - 18));
    }
}
