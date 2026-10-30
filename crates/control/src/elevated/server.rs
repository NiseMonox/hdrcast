//! The input process's half: a named pipe that the user's own programs write input into, replayed with `SendInput`
//! by a process that runs above them (the service starts it as SYSTEM in the user's session, [`super::service`]).
//!
//! Access to the pipe is the only gate: its access list names the user (write only) and the owner. A pipe instance
//! is made before the previous one is taken, so there is always one to connect to; each connection has a thread
//! of its own and its own [`Replay`], which lets go of the keys it holds when the connection ends.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{ReadFile, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND};
use windows::Win32::System::IO::CancelIoEx;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, OpenEventW, SetEvent, WaitForSingleObject, SYNCHRONIZATION_SYNCHRONIZE};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

use super::{Frame, Replay};
use crate::inject::{Injector, ScreenRect};

/// Builds the injector for the display a connection announced.
pub type MakeInjector = Arc<dyn Fn(ScreenRect) -> Box<dyn Injector> + Send + Sync>;

/// The real thing: `SendInput` on that display.
pub fn direct_injector() -> MakeInjector {
    Arc::new(|display| Box::new(crate::inject::WinInjector::new(display)))
}

/// Pointer positions are physical pixels, whatever the scaling of the display: the process must not be lied to
/// about the size of the desktop.
pub fn become_dpi_aware() {
    // SAFETY: only changes how this process sees coordinates; fails harmlessly when something decided already.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Who may open the pipe: SYSTEM and the owner (the process that made it, which makes the next instance too) with
/// every right; the user with the right to write, nothing else, so that none of their ordinary programs can make an
/// instance of its own. Medium integrity, so programs of the user at the normal level may write into a pipe of a
/// higher one.
fn sddl(user_sid: &str) -> String {
    format!("D:(A;;GA;;;SY)(A;;GA;;;OW)(A;;0x100082;;;{user_sid})S:(ML;;NW;;;ME)")
}

/// The security descriptor made from an SDDL string, kept alive for as long as objects are made with it.
pub(crate) struct Descriptor(PSECURITY_DESCRIPTOR);

// SAFETY: a LocalAlloc block that is only read.
unsafe impl Send for Descriptor {}

impl Descriptor {
    pub(crate) fn parse(sddl: &str) -> Result<Self, String> {
        let text: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: a NUL-terminated string; the descriptor it makes is freed in Drop.
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(text.as_ptr()), SDDL_REVISION_1, &mut descriptor, None) }
            .map_err(|e| format!("bad access list \"{sddl}\": {e}"))?;
        Ok(Self(descriptor))
    }

    pub(crate) fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: self.0 .0, bInheritHandle: false.into() }
    }

    pub(crate) fn raw(&self) -> PSECURITY_DESCRIPTOR {
        self.0
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by the call in `parse`, freed once.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}

/// One end of a pipe instance, closed when dropped.
struct Pipe(HANDLE);

// SAFETY: a kernel handle; one thread at a time uses it.
unsafe impl Send for Pipe {}

impl Drop for Pipe {
    fn drop(&mut self) {
        // SAFETY: made by CreateNamedPipeW, closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn create_instance(name: &[u16], descriptor: &Descriptor, first: bool) -> Result<Pipe, String> {
    let attributes = descriptor.attributes();
    let open_mode = if first { PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE } else { PIPE_ACCESS_INBOUND };
    // SAFETY: `name` is NUL-terminated and `attributes` points to a descriptor that lives as long as `descriptor`.
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            0,
            4096,
            0,
            Some(&attributes),
        )
    };
    if handle.is_invalid() {
        return Err(format!("cannot make the pipe: {}", windows::core::Error::from_thread()));
    }
    Ok(Pipe(handle))
}

struct Shared {
    clients: AtomicUsize,
    last_gone: Mutex<Instant>,
    /// The connected clients' pipe handles, so that stopping can make their readers give up.
    open: Mutex<Vec<isize>>,
}

/// The pipe server. Dropping it does not stop it; [`Server::stop`] does.
pub struct Server {
    stop: Arc<AtomicBool>,
    pipe: String,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
}

impl Server {
    /// Listens on `pipe` for the user with `user_sid`. Fails when another helper already does.
    pub fn start(pipe: &str, user_sid: &str, make: MakeInjector) -> Result<Server, String> {
        let descriptor = Descriptor::parse(&sddl(user_sid))?;
        let name: Vec<u16> = pipe.encode_utf16().chain(std::iter::once(0)).collect();
        let first = create_instance(&name, &descriptor, true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared { clients: AtomicUsize::new(0), last_gone: Mutex::new(Instant::now()), open: Mutex::new(Vec::new()) });
        let accept = {
            let (stop, shared) = (stop.clone(), shared.clone());
            std::thread::Builder::new()
                .name("input-accept".into())
                .spawn(move || accept_loop(name, descriptor, first, make, stop, shared))
                .map_err(|e| format!("cannot start the server thread: {e}"))?
        };
        Ok(Server { stop, pipe: pipe.to_string(), shared, accept: Some(accept) })
    }

    /// How long nobody has been connected, or `None` while somebody is. A server nobody ever connected to counts
    /// from its start.
    pub fn idle_for(&self) -> Option<Duration> {
        if self.shared.clients.load(Ordering::SeqCst) > 0 {
            return None;
        }
        Some(self.shared.last_gone.lock().unwrap().elapsed())
    }

    /// Stops listening and lets go of every client, which makes each of them let go of the keys it holds.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // wakes the accept thread, which is waiting for somebody to connect
        let _ = super::client::open(&self.pipe);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        // The readers of the clients that are still connected are waiting in ReadFile: tell them to give up (again,
        // for one that was between two reads), then wait for them to have let go.
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.shared.clients.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            for raw in self.shared.open.lock().unwrap().iter() {
                // SAFETY: the handle stays open until its reader has removed it from the list, which it does first.
                unsafe {
                    let _ = CancelIoEx(HANDLE(*raw as *mut _), None);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// A named event: the service sets it to ask the input process to leave, which it does by letting go of its clients.
pub struct StopEvent(HANDLE);

// SAFETY: a kernel handle, which any thread may wait on or set.
unsafe impl Send for StopEvent {}
unsafe impl Sync for StopEvent {}

impl StopEvent {
    /// Makes the event `name`; only what `sddl` names may open it.
    pub(crate) fn create(name: &str, sddl: &str) -> Result<Self, String> {
        let descriptor = Descriptor::parse(sddl)?;
        let attributes = descriptor.attributes();
        let name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `name` is NUL-terminated and `attributes` points to a descriptor that outlives the call.
        unsafe { CreateEventW(Some(&attributes), true, false, PCWSTR(name.as_ptr())) }
            .map(Self)
            .map_err(|e| format!("cannot make the event: {e}"))
    }

    /// The event the service made, to wait on.
    pub fn open(name: &str) -> Result<Self, String> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated.
        unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(wide.as_ptr())) }
            .map(Self)
            .map_err(|e| format!("cannot open the event \"{name}\": {e}"))
    }

    pub(crate) fn set(&self) {
        // SAFETY: a live event handle.
        unsafe {
            let _ = SetEvent(self.0);
        }
    }

    /// Whether the event was set within `timeout`.
    pub fn wait(&self, timeout: Duration) -> bool {
        // SAFETY: a live event handle.
        unsafe { WaitForSingleObject(self.0, u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1)) == WAIT_OBJECT_0 }
    }
}

impl Drop for StopEvent {
    fn drop(&mut self) {
        // SAFETY: made by CreateEventW / OpenEventW, closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Why the input process left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Left {
    /// Nobody was connected for as long as it stays.
    Idle,
    /// The service asked it to.
    Asked,
}

/// How often the process looks at the clock and at the event.
const POLL: Duration = if cfg!(test) { Duration::from_millis(20) } else { Duration::from_secs(1) };

/// Waits while clients come and go: until nobody has been connected for `stay`, or `stop` is set.
pub fn wait_until_done(server: &Server, stay: Duration, stop: Option<&StopEvent>) -> Left {
    loop {
        let asked = match stop {
            Some(event) => event.wait(POLL),
            None => {
                std::thread::sleep(POLL);
                false
            }
        };
        if asked {
            return Left::Asked;
        }
        if server.idle_for().is_some_and(|idle| idle >= stay) {
            return Left::Idle;
        }
    }
}

fn accept_loop(
    name: Vec<u16>,
    descriptor: Descriptor,
    mut waiting: Pipe,
    make: MakeInjector,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
) {
    // A spare instance exists all the time, so that a second client who comes while the first is being taken does not
    // find every instance busy. (An instance can be connected to from the moment it exists.)
    let mut spare = match create_instance(&name, &descriptor, false) {
        Ok(spare) => Some(spare),
        Err(e) => {
            super::setup::log(&format!("input: {e}"));
            return;
        }
    };
    loop {
        // SAFETY: `waiting` is a pipe instance made by `create_instance`.
        let connected = match unsafe { ConnectNamedPipe(waiting.0, None) } {
            Ok(()) => true,
            // the client got in between the pipe being made and this call: connected all the same
            Err(e) => e.code() == ERROR_PIPE_CONNECTED.to_hresult(),
        };
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if !connected {
            // a client that gave up before it was taken; the same instance serves the next one
            // SAFETY: the instance is ours.
            unsafe {
                let _ = DisconnectNamedPipe(waiting.0);
            }
            continue;
        }
        let refilled = create_instance(&name, &descriptor, false).map_err(|e| super::setup::log(&format!("input: {e}"))).ok();
        let Some(next) = spare.take().or_else(|| create_instance(&name, &descriptor, false).ok()) else {
            return;
        };
        let client = std::mem::replace(&mut waiting, next);
        spare = refilled;
        let raw = client.0 .0 as isize;
        shared.clients.fetch_add(1, Ordering::SeqCst);
        shared.open.lock().unwrap().push(raw);
        let (make, for_client) = (make.clone(), shared.clone());
        let spawned = std::thread::Builder::new().name("input-client".into()).spawn(move || {
            serve_client(&client, &make);
            // the handle leaves the list before it is closed
            for_client.open.lock().unwrap().retain(|h| *h != raw);
            drop(client);
            *for_client.last_gone.lock().unwrap() = Instant::now();
            for_client.clients.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            super::setup::log("input: cannot start a thread for a client");
            shared.open.lock().unwrap().retain(|h| *h != raw);
            shared.clients.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

fn serve_client(pipe: &Pipe, make: &MakeInjector) {
    let make = make.clone();
    let mut replay = Replay::new(move |display| make(display));
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 512];
    'read: loop {
        let mut read = 0u32;
        // SAFETY: `chunk` is the buffer the call is told about.
        let result = unsafe { ReadFile(pipe.0, Some(&mut chunk), Some(&mut read), None) };
        if result.is_err() || read == 0 {
            break;
        }
        pending.extend_from_slice(&chunk[..read as usize]);
        let mut at = 0;
        loop {
            match Frame::decode(&pending[at..]) {
                Ok(Some((frame, used))) => {
                    if !replay.apply(frame) {
                        break 'read;
                    }
                    at += used;
                }
                Ok(None) => break,
                Err(_) => break 'read,
            }
        }
        pending.drain(..at);
        if pending.len() > chunk.len() {
            break;
        }
    }
    // lets go of whatever the client still held
    drop(replay);
    // SAFETY: the instance is ours; the handle is closed when `pipe` is dropped by the caller.
    unsafe {
        let _ = DisconnectNamedPipe(pipe.0);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::elevated::testing::Recorder;
    use crate::elevated::{pipe_name, setup};
    use crate::keys::{Key, Mods};

    fn start(tag: &str, rec: &Recorder) -> (Server, String) {
        let sid = setup::user_sid().unwrap();
        let pipe = format!("{}-test-{tag}-{}", pipe_name(&sid), std::process::id());
        let rec = rec.clone();
        let make: MakeInjector = Arc::new(move |_display| Box::new(rec.clone()));
        (Server::start(&pipe, &sid, make).expect("the server starts"), pipe)
    }

    /// Like the app does: an instance that is busy at this very moment is tried again.
    fn connect(pipe: &str) -> std::fs::File {
        for _ in 0..200 {
            match super::super::client::open(pipe) {
                Ok(file) => return file,
                Err(e) if matches!(e.raw_os_error(), Some(2) | Some(231)) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => panic!("the user may write to the pipe: {e}"),
            }
        }
        panic!("no instance of the pipe became free");
    }

    fn send(file: &mut std::fs::File, frames: &[Frame]) {
        let mut bytes = Vec::new();
        for f in frames {
            f.encode(&mut bytes);
        }
        file.write_all(&bytes).unwrap();
    }

    const SCREEN: ScreenRect = ScreenRect { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0 };

    #[test]
    fn input_written_into_the_pipe_is_replayed_and_a_vanished_client_lets_go() {
        let rec = Recorder::default();
        let (server, pipe) = start("replay", &rec);
        assert!(setup::pipe_exists(&pipe));
        let mut client = connect(&pipe);
        send(&mut client, &[Frame::Hello(SCREEN), Frame::Pointer(100, 200), Frame::Modifier(Mods::SHIFT, true), Frame::Key(Key::Char('a'), true)]);
        assert_eq!(rec.wait_for(3), ["move 100,200", "mod1 down", "Char('a') down"]);
        assert_eq!(server.idle_for(), None, "somebody is connected");
        drop(client);
        let lines = rec.wait_for(5);
        assert_eq!(lines[3..], ["Char('a') up", "mod1 up"], "everything it held is let go");
        server.stop();
    }

    #[test]
    fn several_clients_at_once_and_one_after_the_other() {
        let rec = Recorder::default();
        let (server, pipe) = start("several", &rec);
        let mut a = connect(&pipe);
        let mut b = connect(&pipe);
        send(&mut a, &[Frame::Hello(SCREEN), Frame::Pointer(1, 1)]);
        send(&mut b, &[Frame::Hello(SCREEN), Frame::Pointer(2, 2)]);
        let mut lines = rec.wait_for(2);
        lines.sort();
        assert_eq!(lines, ["move 1,1", "move 2,2"]);
        drop((a, b));
        let deadline = Instant::now() + Duration::from_secs(2);
        while server.idle_for().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(server.idle_for().is_some(), "both are gone");
        let mut c = connect(&pipe);
        send(&mut c, &[Frame::Hello(SCREEN), Frame::Pointer(3, 3)]);
        assert!(rec.wait_for(3).contains(&"move 3,3".to_string()));
        server.stop();
    }

    #[test]
    fn a_client_that_sends_rubbish_or_skips_the_hello_is_cut_off() {
        let rec = Recorder::default();
        let (server, pipe) = start("rubbish", &rec);
        let mut skipper = connect(&pipe);
        send(&mut skipper, &[Frame::Pointer(5, 5)]);
        let mut garbage = connect(&pipe);
        garbage.write_all(&[0xEE, 1, 2, 3, 4]).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(rec.lines().is_empty());
        // and the server is still there for a decent client
        let mut ok = connect(&pipe);
        send(&mut ok, &[Frame::Hello(SCREEN), Frame::Pointer(7, 7)]);
        assert_eq!(rec.wait_for(1), ["move 7,7"]);
        server.stop();
    }

    #[test]
    fn stopping_lets_go_of_what_a_connected_client_holds() {
        let rec = Recorder::default();
        let (server, pipe) = start("stop", &rec);
        let mut client = connect(&pipe);
        send(&mut client, &[Frame::Hello(SCREEN), Frame::Modifier(Mods::CTRL, true), Frame::Key(Key::Char('c'), true)]);
        assert_eq!(rec.wait_for(2), ["mod2 down", "Char('c') down"]);
        server.stop();
        // the client is still connected, and the server has let go for it
        assert_eq!(rec.lines()[2..], ["Char('c') up", "mod2 up"]);
        drop(client);
    }

    #[test]
    fn the_input_process_leaves_when_nobody_comes_or_when_asked() {
        let rec = Recorder::default();
        let (server, _pipe) = start("leave", &rec);
        assert_eq!(wait_until_done(&server, Duration::from_millis(100), None), Left::Idle);

        let name = format!(r"Local\HdrCastInputTestStop-{}", std::process::id());
        let event = StopEvent::create(&name, "D:(A;;GA;;;OW)").unwrap();
        let waiting = StopEvent::open(&name).unwrap();
        assert!(!waiting.wait(Duration::from_millis(30)));
        event.set();
        // asked: long before the minute nobody has to come for
        assert_eq!(wait_until_done(&server, Duration::from_secs(60), Some(&waiting)), Left::Asked);
        server.stop();
    }

    #[test]
    fn a_second_helper_for_the_same_user_does_not_start() {
        let rec = Recorder::default();
        let (server, pipe) = start("twice", &rec);
        let sid = setup::user_sid().unwrap();
        let make: MakeInjector = Arc::new(|_| unreachable!());
        assert!(Server::start(&pipe, &sid, make).is_err());
        server.stop();
    }
}
