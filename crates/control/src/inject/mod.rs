//! Putting a viewer's input into the host's operating system.

use std::sync::Arc;

use crate::keys::{Button, Key};

#[cfg(target_os = "macos")]
mod mac;
#[cfg(windows)]
mod win;

#[cfg(windows)]
pub(crate) use win::WinInjector;

/// The shared display in the operating system's global coordinates (Windows: physical pixels of the virtual
/// desktop; macOS: points of the global display space).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Says something worth knowing to the host's log, in its words ("[control] ...").
pub type Notices = Arc<dyn Fn(&str) + Send + Sync>;

/// What [`crate::host::HostControl`] needs from the operating system. Modifiers arrive as their own presses,
/// before the key or button they belong to.
pub trait Injector: Send {
    /// Moves the pointer; `x` and `y` run 0..=65535 across the shared display.
    fn pointer(&mut self, x: u16, y: u16);
    fn button(&mut self, button: Button, down: bool);
    /// `axis` 0 = vertical, 1 = horizontal; `delta` in 1/120 notches, positive = up / right.
    fn wheel(&mut self, axis: u8, delta: i16);
    /// `bit` is one of the [`crate::keys::Mods`] constants.
    fn modifier(&mut self, bit: u8, down: bool);
    fn key(&mut self, key: Key, down: bool);
    /// The viewer this injector serves is (`true`) or is no longer (`false`) controlling this computer. The first
    /// input follows soon after a `true`: a chance to get ready (Windows: start the input service).
    fn activate(&mut self, _on: bool) {}
}

/// The real thing for this operating system.
pub struct SystemInjector {
    #[cfg(windows)]
    inner: crate::elevated::client::ElevatedInjector,
    #[cfg(target_os = "macos")]
    inner: mac::MacInjector,
    #[cfg(not(any(windows, target_os = "macos")))]
    _rect: ScreenRect,
}

impl SystemInjector {
    /// `display` is the display being shared: the pointer is confined to it. This one injects by itself and tells
    /// nobody anything; see [`SystemInjector::for_session`].
    pub fn new(display: ScreenRect) -> Self {
        Self::for_session(display, Arc::new(|_| {}))
    }

    /// The injector of one viewer's session. On Windows it hands the input to the input service's program while the
    /// viewer has control and the service is set up (see [`crate::elevated`]), so that windows of programs run as
    /// administrator react too; `notices` hears how that goes.
    pub fn for_session(display: ScreenRect, notices: Notices) -> Self {
        #[cfg(windows)]
        return Self { inner: crate::elevated::client::ElevatedInjector::new(display, notices) };
        #[cfg(target_os = "macos")]
        {
            let _ = notices;
            return Self { inner: mac::MacInjector::new(display) };
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = notices;
            Self { _rect: display }
        }
    }

    /// Like [`SystemInjector::for_session`], but for an input program somebody started by hand (`HdrCastInput serve` in an
    /// elevated shell) on `pipe`, installed or not: for trying it out.
    #[cfg(windows)]
    #[doc(hidden)]
    pub fn for_helper_at(display: ScreenRect, notices: Notices, pipe: &str) -> Self {
        Self { inner: crate::elevated::client::ElevatedInjector::with_pipe(display, notices, pipe) }
    }

    /// Whether this process may inject input at all. macOS wants the user to tick the app under Privacy &
    /// Security > Accessibility; `prompt` makes the system show that request. Always true on Windows (windows
    /// of programs run as administrator ignore us unless the service in [`crate::elevated`] is set up).
    pub fn permitted(prompt: bool) -> bool {
        #[cfg(target_os = "macos")]
        return mac::accessibility_trusted(prompt);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = prompt;
            cfg!(windows)
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
impl Injector for SystemInjector {
    fn pointer(&mut self, x: u16, y: u16) {
        self.inner.pointer(x, y)
    }
    fn button(&mut self, button: Button, down: bool) {
        self.inner.button(button, down)
    }
    fn wheel(&mut self, axis: u8, delta: i16) {
        self.inner.wheel(axis, delta)
    }
    fn modifier(&mut self, bit: u8, down: bool) {
        self.inner.modifier(bit, down)
    }
    fn key(&mut self, key: Key, down: bool) {
        self.inner.key(key, down)
    }
    fn activate(&mut self, on: bool) {
        self.inner.activate(on)
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
impl Injector for SystemInjector {
    fn pointer(&mut self, _x: u16, _y: u16) {}
    fn button(&mut self, _button: Button, _down: bool) {}
    fn wheel(&mut self, _axis: u8, _delta: i16) {}
    fn modifier(&mut self, _bit: u8, _down: bool) {}
    fn key(&mut self, _key: Key, _down: bool) {}
}

/// The rectangle of a display by the name the capture engine knows it by: on Windows the GDI device name
/// (`\\.\DISPLAY1`), on macOS the decimal `CGDirectDisplayID`. `None` when no such display is attached.
pub fn display_rect(name: &str) -> Option<ScreenRect> {
    #[cfg(windows)]
    return win::display_rect(name);
    #[cfg(target_os = "macos")]
    return mac::display_rect(name);
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = name;
        None
    }
}
