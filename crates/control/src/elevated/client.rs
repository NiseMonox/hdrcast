//! The app's half: an injector that hands everything to the input program while it is there and injects by itself
//! while it is not (docs/design.md §9.1).
//!
//! The service (and with it the input program) is started in the background when a viewer is given control, so that
//! it is ready by the first key; the first events go the direct way meanwhile, which is no harm since the keyboard
//! state is the system's, not a program's. A program that goes away is noticed by the next write; the injector then
//! injects by itself again and tries to bring the service back every few seconds.

use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

use super::{pipe_name, setup, Frame, SERVICE_NAME};
use crate::inject::{Injector, Notices, ScreenRect, WinInjector};
use crate::keys::{Button, Key};

/// What the client may do to the pipe: write data and look at it. Not make instances of it.
pub(crate) const WRITE_ONLY: u32 = 0x0010_0000 | 0x80 | 0x2;

/// How long the input program has to show up after the service was started.
const START_TIMEOUT: Duration = if cfg!(test) { Duration::from_millis(400) } else { Duration::from_secs(5) };
/// After the input program was lost, how long before the next try.
const RETRY_EVERY: Duration = Duration::from_secs(5);
/// How often Windows' refusal is looked for while the input program is not used, and how often it may be reported.
const LOOK_EVERY: Duration = Duration::from_secs(2);
const WARN_EVERY: Duration = Duration::from_secs(120);

pub(crate) fn open(pipe: &str) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().access_mode(WRITE_ONLY).open(pipe)
}

/// Whether an error from `open` is the input program not being there (yet) or busy, as opposed to being refused.
fn try_again(e: &std::io::Error) -> bool {
    // ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY
    matches!(e.raw_os_error(), Some(2) | Some(231))
}

/// Starts the service if the input program is not up (unless `fixed_pipe` names one that somebody started by hand),
/// connects, says hello.
fn connect(display: ScreenRect, fixed_pipe: Option<&str>) -> Result<std::fs::File, String> {
    let pipe = match fixed_pipe {
        Some(pipe) => pipe.to_string(),
        None => pipe_name(&setup::user_sid()?),
    };
    let deadline = Instant::now() + START_TIMEOUT;
    let mut start_failed = None;
    let mut next_start = Instant::now();
    loop {
        // A service that is just stopping (the input program left a moment ago) cannot be started yet: asked again
        // every so often while the pipe is not there.
        if fixed_pipe.is_none() && Instant::now() >= next_start && !setup::pipe_exists(&pipe) {
            start_failed = setup::start(SERVICE_NAME).err();
            next_start = Instant::now() + Duration::from_millis(300);
        }
        match open(&pipe) {
            Ok(mut file) => {
                let mut hello = Vec::new();
                Frame::Hello(display).encode(&mut hello);
                file.write_all(&hello).map_err(|e| format!("the input program did not take the greeting: {e}"))?;
                return Ok(file);
            }
            Err(e) if try_again(&e) && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(40)),
            Err(e) if try_again(&e) => {
                return Err(start_failed.unwrap_or_else(|| {
                    format!("it did not come up within {} seconds (see {})", START_TIMEOUT.as_secs(), setup::log_path().display())
                }))
            }
            Err(e) => return Err(format!("it would not let this program in: {e}")),
        }
    }
}

/// What the injector and the thread that brings the input program up share.
#[derive(Default)]
struct Link {
    pipe: Mutex<Option<std::fs::File>>,
    connecting: AtomicBool,
    /// Somebody is controlling: the input program is wanted.
    wanted: AtomicBool,
    /// A problem has been reported since the input program last worked: not again until it has.
    told: AtomicBool,
}

pub(crate) struct ElevatedInjector {
    direct: WinInjector,
    display: ScreenRect,
    notices: Notices,
    /// An input program somebody started by hand, on this pipe (to try it out): neither installed nor started by the app.
    fixed_pipe: Option<String>,
    link: Arc<Link>,
    /// Not before this: the next try to bring the input program up.
    retry_at: Instant,
    looked_at: Instant,
    warned_at: Option<Instant>,
}

impl ElevatedInjector {
    /// `notices` is told what is worth knowing (the service is used; it could not be started; a window cannot be reached).
    pub(crate) fn new(display: ScreenRect, notices: Notices) -> Self {
        let now = Instant::now();
        Self {
            direct: WinInjector::new(display),
            display,
            notices,
            fixed_pipe: None,
            link: Arc::new(Link::default()),
            retry_at: now,
            looked_at: now,
            warned_at: None,
        }
    }

    /// For trying it out: talks to the input program listening on `pipe`, installed or not.
    pub(crate) fn with_pipe(display: ScreenRect, notices: Notices, pipe: &str) -> Self {
        Self { fixed_pipe: Some(pipe.to_string()), ..Self::new(display, notices) }
    }

    /// Brings the input program up (by starting the service) in the background when it is installed, needed and not there.
    fn bring_up(&mut self) {
        self.retry_at = Instant::now() + RETRY_EVERY;
        if self.link.pipe.lock().unwrap().is_some() || self.link.connecting.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.fixed_pipe.is_none() {
            let status = setup::status();
            if !status.installed || status.not_needed {
                self.link.connecting.store(false, Ordering::SeqCst);
                return;
            }
            if status.outdated {
                if !self.link.told.swap(true, Ordering::SeqCst) {
                    (self.notices)("[control] the HdrCast input service is from an older HdrCast: set it up again in Settings. Until then windows of programs run as administrator will not react");
                }
                self.link.connecting.store(false, Ordering::SeqCst);
                return;
            }
        }
        let (link, notices, display, fixed_pipe) = (self.link.clone(), self.notices.clone(), self.display, self.fixed_pipe.clone());
        let spawned = std::thread::Builder::new().name("input-helper-connect".into()).spawn(move || {
            match connect(display, fixed_pipe.as_deref()) {
                Ok(file) => {
                    let mut slot = link.pipe.lock().unwrap();
                    // control may have been given back while the program came up: then nobody wants the connection
                    if link.wanted.load(Ordering::SeqCst) {
                        *slot = Some(file);
                        drop(slot);
                        link.told.store(false, Ordering::SeqCst);
                        notices("[control] input goes through the HdrCast input service: windows of programs run as administrator react too");
                    }
                }
                Err(e) => {
                    if !link.told.swap(true, Ordering::SeqCst) {
                        notices(&format!(
                            "[control] the HdrCast input service is not available: {e}. Windows of programs run as administrator will ignore the viewer; setting it up again in Settings may help"
                        ));
                    }
                }
            }
            link.connecting.store(false, Ordering::SeqCst);
        });
        if spawned.is_err() {
            self.link.connecting.store(false, Ordering::SeqCst);
        }
    }

    /// Sends a frame to the input program; `false` when there is none (or it just went away) and the caller has to inject.
    fn via_helper(&mut self, frame: Frame) -> bool {
        let mut slot = self.link.pipe.lock().unwrap();
        if let Some(file) = slot.as_mut() {
            let mut bytes = Vec::with_capacity(36);
            frame.encode(&mut bytes);
            if file.write_all(&bytes).is_ok() {
                return true;
            }
            *slot = None;
            drop(slot);
            (self.notices)("[control] the HdrCast input service went away: injecting without it");
            self.retry_at = Instant::now() + RETRY_EVERY;
            return false;
        }
        drop(slot);
        if self.link.wanted.load(Ordering::SeqCst) && Instant::now() >= self.retry_at {
            self.bring_up();
        }
        self.look_for_a_refusal();
        false
    }

    /// Without it, input into a window of a program run as administrator is thrown away by Windows without a
    /// word. Say it, once in a while, when that is happening.
    fn look_for_a_refusal(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.looked_at) < LOOK_EVERY || self.warned_at.is_some_and(|t| now.duration_since(t) < WARN_EVERY) {
            return;
        }
        self.looked_at = now;
        if !input_is_blocked() {
            return;
        }
        self.warned_at = Some(now);
        let advice = if setup::status().installed {
            "the HdrCast input service is not running"
        } else {
            "switch on \"Control programs that run as administrator\" in Settings to get past it"
        };
        (self.notices)(&format!(
            "[control] Windows refuses the viewer's mouse and keyboard right now: usually a window of a program run as administrator is in front (or the screen is locked); {advice}"
        ));
    }
}

impl Injector for ElevatedInjector {
    fn pointer(&mut self, x: u16, y: u16) {
        if !self.via_helper(Frame::Pointer(x, y)) {
            self.direct.pointer(x, y);
        }
    }

    fn button(&mut self, button: Button, down: bool) {
        if !self.via_helper(Frame::Button(button, down)) {
            self.direct.button(button, down);
        }
    }

    fn wheel(&mut self, axis: u8, delta: i16) {
        if !self.via_helper(Frame::Wheel(axis, delta)) {
            self.direct.wheel(axis, delta);
        }
    }

    fn modifier(&mut self, bit: u8, down: bool) {
        if !self.via_helper(Frame::Modifier(bit, down)) {
            self.direct.modifier(bit, down);
        }
    }

    fn key(&mut self, key: Key, down: bool) {
        if !self.via_helper(Frame::Key(key, down)) {
            self.direct.key(key, down);
        }
    }

    fn activate(&mut self, on: bool) {
        self.link.wanted.store(on, Ordering::SeqCst);
        if on {
            self.bring_up();
        } else {
            // closing the pipe lets the input program know nobody holds anything any more
            *self.link.pipe.lock().unwrap() = None;
        }
    }
}

// ---- being refused -------------------------------------------------------------------------------------------------

/// Whether Windows is refusing this program's input right now. The giveaway is `SetCursorPos`: told to put the
/// pointer where it already is, it moves nothing, yet it fails with "access denied" while a window of a program run
/// as administrator is in front (and while the screen is locked or a permission prompt is up). `SendInput` says
/// nothing of the sort: it reports success and the input is dropped.
pub fn input_is_blocked() -> bool {
    let mut at = POINT::default();
    // SAFETY: a plain query, and a move to where the pointer is.
    unsafe { GetCursorPos(&mut at).is_ok() && SetCursorPos(at.x, at.y).is_err() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elevated::server::{MakeInjector, Server};
    use crate::elevated::testing::{wait_until, Recorder};
    use crate::keys::Mods;

    const SCREEN: ScreenRect = ScreenRect { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0 };

    fn helper_listening(tag: &str, rec: &Recorder) -> (Server, String) {
        let sid = setup::user_sid().unwrap();
        let pipe = format!("{}-test-client-{tag}-{}", pipe_name(&sid), std::process::id());
        let rec = rec.clone();
        let make: MakeInjector = Arc::new(move |_| Box::new(rec.clone()));
        (Server::start(&pipe, &sid, make).expect("the server starts"), pipe)
    }

    fn notices() -> (Notices, Arc<Mutex<Vec<String>>>) {
        let said = Arc::new(Mutex::new(Vec::<String>::new()));
        let into = said.clone();
        (Arc::new(move |line: &str| into.lock().unwrap().push(line.to_string())), said)
    }

    #[test]
    fn input_goes_through_the_helper_and_giving_control_back_lets_go_of_what_was_held() {
        let rec = Recorder::default();
        let (server, pipe) = helper_listening("through", &rec);
        let (notices, said) = notices();
        let mut injector = ElevatedInjector::with_pipe(SCREEN, notices, &pipe);
        injector.activate(true);
        // before it is connected the injector would inject by itself: wait for the helper
        assert!(wait_until(|| said.lock().unwrap().iter().any(|l| l.contains("goes through"))), "{:?}", said.lock().unwrap());
        injector.pointer(1, 2);
        injector.modifier(Mods::SHIFT, true);
        injector.key(Key::Char('a'), true);
        assert_eq!(rec.wait_for(3), ["move 1,2", "mod1 down", "Char('a') down"]);
        injector.activate(false);
        assert_eq!(rec.wait_for(5)[3..], ["Char('a') up", "mod1 up"]);
        server.stop();
    }

    #[test]
    fn control_given_back_while_the_helper_comes_up_leaves_no_connection() {
        let rec = Recorder::default();
        let (server, pipe) = helper_listening("back", &rec);
        let (notices, _said) = notices();
        let mut injector = ElevatedInjector::with_pipe(SCREEN, notices, &pipe);
        injector.activate(true);
        injector.activate(false);
        assert!(wait_until(|| !injector.link.connecting.load(Ordering::SeqCst)));
        assert!(injector.link.pipe.lock().unwrap().is_none());
        server.stop();
    }

    #[test]
    fn a_helper_that_is_not_there_is_reported_once() {
        let (notices, said) = notices();
        let mut injector = ElevatedInjector::with_pipe(SCREEN, notices, r"\\.\pipe\hdrcast-input-test-nobody-home");
        injector.activate(true);
        assert!(wait_until(|| !said.lock().unwrap().is_empty()));
        assert!(wait_until(|| !injector.link.connecting.load(Ordering::SeqCst)));
        // asked again (the injector does every few seconds while input flows): not repeated
        injector.activate(true);
        assert!(wait_until(|| !injector.link.connecting.load(Ordering::SeqCst)));
        let said = said.lock().unwrap();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("not available"), "{said:?}");
    }
}
