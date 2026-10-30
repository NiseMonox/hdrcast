//! What a program at the normal integrity level can put into the window in front, with and without the input
//! service (src/elevated/mod.rs). Moves the pointer to the middle of the display, types "hdrcast" and puts the
//! pointer back; writes what happened to a file. It has no window, so starting it does not change which window is
//! in front (a console program would take the front for itself).
//!
//! It is started the way HdrCast is, through Explorer, so that it has the normal integrity level even when it is
//! built from an elevated shell: `explorer.exe inject_probe.exe`. Explorer passes no arguments, so the settings
//! are read from `inject_probe.cfg` next to the program:
//!
//!     mode=direct        inject by itself (what HdrCast does without the service)
//!     mode=helper        through an input program somebody started by hand (`HdrCastInput serve` in an elevated shell)
//!     mode=session       as HdrCast does it: starts the installed service and goes through the program it starts
//!     out=C:\...\result.txt
//!     linger=3000        keep moving the pointer for that many milliseconds first (the window in front is only
//!                        looked at every couple of seconds, so a short run never gets to say anything about it)

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    probe::run();
}

#[cfg(windows)]
mod probe {
    use std::sync::{Arc, Mutex};
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use hdrcast_control::elevated::{input_is_blocked, pipe_name, setup};
    use hdrcast_control::inject::{display_rect, Injector, ScreenRect, SystemInjector};
    use hdrcast_control::Key;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

    fn cursor() -> POINT {
        let mut p = POINT::default();
        // SAFETY: `p` is a valid POINT to write to.
        let _ = unsafe { GetCursorPos(&mut p) };
        p
    }

    fn display_under(p: POINT) -> Option<ScreenRect> {
        (1..=16).filter_map(|n| display_rect(&format!(r"\\.\DISPLAY{n}"))).find(|r| {
            let (x, y) = (f64::from(p.x), f64::from(p.y));
            x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
        })
    }

    fn setting(config: &str, key: &str) -> Option<String> {
        config.lines().find_map(|l| l.strip_prefix(&format!("{key}=")).map(|v| v.trim().to_string()))
    }

    pub fn run() {
        let exe = std::env::current_exe().unwrap_or_default();
        let config = std::fs::read_to_string(exe.with_extension("cfg")).unwrap_or_default();
        let mode = setting(&config, "mode").unwrap_or_else(|| "direct".to_string());
        let out = setting(&config, "out");
        let linger = setting(&config, "linger").and_then(|v| v.parse().ok()).unwrap_or(0u64);
        let report = measure(&mode, Duration::from_millis(linger));
        if let Some(out) = out {
            let _ = std::fs::write(out, report);
        }
    }

    fn measure(mode: &str, linger: Duration) -> String {
        // SAFETY: only changes how this process sees coordinates (physical pixels, like the app).
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let before = cursor();
        let blocked_at_start = input_is_blocked();
        let Some(rect) = display_under(before) else { return "no display under the pointer\n".to_string() };
        let said = Arc::new(Mutex::new(Vec::<String>::new()));
        let notices = {
            let said = said.clone();
            Arc::new(move |line: &str| said.lock().unwrap().push(line.to_string()))
        };
        let came_up = |said: &Arc<Mutex<Vec<String>>>| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !said.lock().unwrap().iter().any(|l| l.contains("goes through") || l.contains("not available")) && Instant::now() < deadline {
                sleep(Duration::from_millis(50));
            }
        };
        let mut injector = match mode {
            "helper" => {
                let pipe = pipe_name(&setup::user_sid().unwrap_or_default());
                let mut injector = SystemInjector::for_helper_at(rect, notices, &pipe);
                injector.activate(true);
                came_up(&said);
                injector
            }
            "session" => {
                let mut injector = SystemInjector::for_session(rect, notices);
                injector.activate(true);
                came_up(&said);
                injector
            }
            // as the app has it, minus the service: nobody calls `activate`
            _ => SystemInjector::for_session(rect, notices),
        };

        let middle = (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
        let until = Instant::now() + linger;
        while Instant::now() < until {
            injector.pointer(32768, 32768);
            sleep(Duration::from_millis(100));
        }
        injector.pointer(32768, 32768);
        sleep(Duration::from_millis(300));
        let after = cursor();
        let arrived = (f64::from(after.x) - middle.0).abs() <= 6.0 && (f64::from(after.y) - middle.1).abs() <= 6.0;
        for c in "hdrcast".chars() {
            injector.key(Key::Char(c), true);
            injector.key(Key::Char(c), false);
            sleep(Duration::from_millis(30));
        }
        sleep(Duration::from_millis(300));
        injector.activate(false);
        drop(injector);
        // SAFETY: puts the pointer back where the person had it.
        unsafe {
            let _ = SetCursorPos(before.x, before.y);
        }
        format!(
            "mode={mode}\nbefore={},{}\nmiddle={:.0},{:.0}\nafter={},{}\npointer_arrived={arrived}\nwindows_refuses_input_at_start={blocked_at_start}\nnotices={:?}\n",
            before.x,
            before.y,
            middle.0,
            middle.1,
            after.x,
            after.y,
            said.lock().unwrap()
        )
    }
}
