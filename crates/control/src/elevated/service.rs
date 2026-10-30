//! The service (docs/design.md §9.1): the part Parsec and Sunshine have too. A service runs in session 0, which has no
//! access to what a user sees and types into, so it starts the program that does the typing inside the user's own
//! session, with its own token: SYSTEM, the highest integrity level there is, so that no window is above it.
//!
//! It runs on demand. HdrCast starts the service when a viewer is given control (its access list lets the user it was
//! set up for do that, and nothing else); the service starts `HdrCastInput.exe serve` in that user's session and
//! stops when that program has left, which it does a while after the last viewer. Windows asking the service to stop
//! (uninstalling, shutting down) makes the program let go of what it holds first.
//!
//! The program gets a copy of the service's token with every privilege taken away (`DISABLE_MAX_PRIVILEGE`): it types
//! into windows, which needs the level and none of the privileges, so a flaw in it is worth as little as possible.
//! It stays on the user's own desktop: following the switch to the secure desktop (permission prompts, the lock screen)
//! would let a program of the user press "Yes" on its own prompt through the pipe.

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::OnceLock;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    ERROR_CALL_NOT_IMPLEMENTED, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, ERROR_NOT_ALL_ASSIGNED, ERROR_SERVICE_SPECIFIC_ERROR, HANDLE,
    LUID, WAIT_OBJECT_0,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, CreateRestrictedToken, DuplicateTokenEx, LookupPrivilegeValueW, SecurityImpersonation, SetTokenInformation,
    TokenPrimary, TokenSessionId, DISABLE_MAX_PRIVILEGE, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_ALL_ACCESS, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::RemoteDesktop::{WTSActive, WTSEnumerateSessionsW, WTSFreeMemory, WTSQueryUserToken, WTS_SESSION_INFOW};
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS,
    SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, SetEvent, TerminateProcess,
    WaitForMultipleObjects, WaitForSingleObject, CREATE_NO_WINDOW, INFINITE, NORMAL_PRIORITY_CLASS, PROCESS_INFORMATION, STARTUPINFOW,
};

use super::server::StopEvent;
use super::setup::{self, sid_of_token, wide, Handle};

/// Who the service is for: the user whose session it starts the program in.
struct Config {
    name: String,
    user_sid: String,
}

static CONFIG: OnceLock<Config> = OnceLock::new();
/// The handle Windows gave this service to report its state with, and the event its control handler sets when
/// Windows asks it to stop (both as plain numbers: the callbacks have nowhere else to find them).
static STATUS_HANDLE: AtomicIsize = AtomicIsize::new(0);
static STOP_REQUESTED: AtomicIsize = AtomicIsize::new(0);

/// How long the program is given to let go of what it holds before it is ended.
const LEAVE_WITHIN_MS: u32 = 4_000;

/// Runs as the service `name` for the user `user_sid`: hands this thread to the service control manager until the
/// service has stopped. Only works when the service control manager started the process.
pub fn run(name: &str, user_sid: &str) -> Result<(), String> {
    let _ = CONFIG.set(Config { name: name.to_string(), user_sid: user_sid.to_string() });
    let mut service_name = wide(std::ffi::OsStr::new(name));
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: PWSTR(service_name.as_mut_ptr()), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW { lpServiceName: PWSTR::null(), lpServiceProc: None },
    ];
    // SAFETY: `table` ends with the all-zero entry the call wants and outlives it (the call returns when the service is done).
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }.map_err(|e| {
        if e.code() == ERROR_FAILED_SERVICE_CONTROLLER_CONNECT.to_hresult() {
            "this is run by Windows as a service, not by hand".to_string()
        } else {
            e.to_string()
        }
    })
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    if std::panic::catch_unwind(serve).is_err() {
        setup::log("service: stopped by a bug");
        send_status(SERVICE_STOPPED, 0, ERROR_SERVICE_SPECIFIC_ERROR.0, 2, 0, 0);
    }
}

unsafe extern "system" fn control_handler(control: u32, _event_type: u32, _event_data: *mut c_void, _context: *mut c_void) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            let event = STOP_REQUESTED.load(Ordering::SeqCst);
            if event != 0 {
                // SAFETY: the event made in `serve`, which lives until the service has stopped.
                let _ = unsafe { SetEvent(HANDLE(event as *mut c_void)) };
            }
            0
        }
        SERVICE_CONTROL_INTERROGATE => 0,
        _ => ERROR_CALL_NOT_IMPLEMENTED.0,
    }
}

fn send_status(state: SERVICE_STATUS_CURRENT_STATE, accepted: u32, exit_code: u32, specific: u32, checkpoint: u32, wait_hint: u32) {
    let handle = STATUS_HANDLE.load(Ordering::SeqCst);
    if handle == 0 {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: exit_code,
        dwServiceSpecificExitCode: specific,
        dwCheckPoint: checkpoint,
        dwWaitHint: wait_hint,
    };
    // SAFETY: the handle Windows gave this service, and a fully filled status.
    unsafe {
        let _ = SetServiceStatus(SERVICE_STATUS_HANDLE(handle as *mut c_void), &status);
    }
}

/// The service's whole life: start the program, wait until it leaves or Windows asks the service to stop, report.
fn serve() {
    let Some(config) = CONFIG.get() else { return };
    let name = wide(std::ffi::OsStr::new(&config.name));
    // SAFETY: a plain function as the handler; `name` is NUL-terminated.
    let handle = match unsafe { RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(control_handler), None) } {
        Ok(handle) => handle,
        Err(e) => {
            setup::log(&format!("service: cannot listen to Windows: {e}"));
            return;
        }
    };
    STATUS_HANDLE.store(handle.0 as isize, Ordering::SeqCst);
    send_status(SERVICE_START_PENDING, 0, 0, 0, 1, 10_000);

    // SAFETY: an unnamed manual-reset event, closed when `stop` is dropped.
    let stop = match unsafe { CreateEventW(None, true, false, PCWSTR::null()) } {
        Ok(event) => Handle(event),
        Err(e) => {
            setup::log(&format!("service: cannot make an event: {e}"));
            send_status(SERVICE_STOPPED, 0, ERROR_SERVICE_SPECIFIC_ERROR.0, 3, 0, 0);
            return;
        }
    };
    STOP_REQUESTED.store(stop.0 .0 as isize, Ordering::SeqCst);

    let worker = match start_program(config) {
        Ok(worker) => worker,
        Err(e) => {
            setup::log(&format!("service: {e}"));
            send_status(SERVICE_STOPPED, 0, ERROR_SERVICE_SPECIFIC_ERROR.0, 1, 0, 0);
            return;
        }
    };
    send_status(SERVICE_RUNNING, SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN, 0, 0, 0, 0);
    setup::log(&format!("service: the input program runs in session {}", worker.session));

    // SAFETY: two live handles.
    let woke = unsafe { WaitForMultipleObjects(&[stop.0, worker.process.0], false, INFINITE) };
    if woke == WAIT_OBJECT_0 {
        send_status(SERVICE_STOP_PENDING, 0, 0, 0, 1, LEAVE_WITHIN_MS + 2_000);
        worker.ask_to_leave();
        setup::log("service: stopped on request");
    } else {
        let mut code = 0u32;
        // SAFETY: a live process handle.
        let _ = unsafe { GetExitCodeProcess(worker.process.0, &mut code) };
        setup::log(&format!("service: the input program has left (code {code}), so the service stops"));
    }
    STOP_REQUESTED.store(0, Ordering::SeqCst);
    send_status(SERVICE_STOPPED, 0, 0, 0, 0, 0);
}

/// The program that types, running in the user's session.
struct Worker {
    process: Handle,
    session: u32,
    stop: StopEvent,
}

impl Worker {
    /// Lets the program leave in its own way (letting go of the keys its clients hold), and ends it if it takes too long.
    fn ask_to_leave(&self) {
        self.stop.set();
        // SAFETY: a live process handle.
        unsafe {
            if WaitForSingleObject(self.process.0, LEAVE_WITHIN_MS) != WAIT_OBJECT_0 {
                let _ = TerminateProcess(self.process.0, 1);
            }
        }
    }
}

/// Turns on a privilege this process has but keeps switched off until it is needed (the system does so with most of
/// them); `false` when the process does not have it at all.
fn enable_privilege(name: &str) -> bool {
    let name = wide(std::ffi::OsStr::new(name));
    let mut luid = LUID::default();
    // SAFETY: `name` is NUL-terminated.
    if unsafe { LookupPrivilegeValueW(PCWSTR::null(), PCWSTR(name.as_ptr()), &mut luid) }.is_err() {
        return false;
    }
    let mut token = HANDLE::default();
    // SAFETY: the pseudo handle of this process; the token is closed by `Handle`.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) }.is_err() {
        return false;
    }
    let token = Handle(token);
    let privileges = TOKEN_PRIVILEGES { PrivilegeCount: 1, Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }] };
    // SAFETY: a live token and a list of one privilege. The call "succeeds" for a privilege the token does not have too,
    // and says so through the last error.
    unsafe {
        AdjustTokenPrivileges(token.0, false, Some(&privileges), 0, None, None).is_ok()
            && windows::core::Error::from_thread().code() != ERROR_NOT_ALL_ASSIGNED.to_hresult()
    }
}

fn start_program(config: &Config) -> Result<Worker, String> {
    // what the next steps are made of: looking at a session's user, moving a token to a session, starting a process
    // with it. A service has all three and keeps them off until it needs them.
    for name in ["SeTcbPrivilege", "SeAssignPrimaryTokenPrivilege", "SeIncreaseQuotaPrivilege"] {
        if !enable_privilege(name) {
            setup::log(&format!("service: cannot turn on {name}"));
        }
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot tell where the service's program is: {e}"))?;
    let session = session_of_user(&config.user_sid).ok_or_else(|| format!("{} is not signed in on any session", config.user_sid))?;
    let event_name = stop_event_name();
    // only SYSTEM (what the program runs as) may open it
    let stop = StopEvent::create(&event_name, "D:(A;;GA;;;SY)")?;
    let token = program_token(session)?;
    let command = format!("\"{}\" serve --user {} --stop-event {event_name}", exe.display(), config.user_sid);
    let process = launch(&token, &exe, &command)?;
    Ok(Worker { process, session, stop })
}

/// A name nobody else can guess to be there (and that two services do not share).
fn stop_event_name() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!(r"Global\HdrCastInputStop-{}-{nanos:x}", std::process::id())
}

/// The sessions in which somebody is signed in and active (the console, or a remote desktop).
fn active_sessions() -> Vec<u32> {
    let mut sessions: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: the local server; the list it makes is freed below.
    if unsafe { WTSEnumerateSessionsW(None, 0, 1, &mut sessions, &mut count) }.is_err() {
        return Vec::new();
    }
    // SAFETY: `sessions` points to `count` entries until it is freed.
    let active = unsafe { std::slice::from_raw_parts(sessions, count as usize) }
        .iter()
        .filter(|s| s.State == WTSActive)
        .map(|s| s.SessionId)
        .collect();
    // SAFETY: allocated by WTSEnumerateSessionsW, freed once.
    unsafe { WTSFreeMemory(sessions.cast()) };
    active
}

/// The session in which the user with `user_sid` is signed in and active.
fn session_of_user(user_sid: &str) -> Option<u32> {
    active_sessions().into_iter().find(|&session| {
        let mut token = HANDLE::default();
        // SAFETY: needs the privilege this service has; the token it gives is closed by `Handle`.
        if unsafe { WTSQueryUserToken(session, &mut token) }.is_err() {
            return false;
        }
        let token = Handle(token);
        sid_of_token(token.0).is_ok_and(|sid| sid.eq_ignore_ascii_case(user_sid))
    })
}

/// A primary token (the kind a process can run as) that is a copy of `token`.
fn duplicate_primary(token: &Handle) -> Result<Handle, String> {
    let mut copy = HANDLE::default();
    // SAFETY: a live token; the copy is closed by `Handle`.
    unsafe { DuplicateTokenEx(token.0, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut copy) }
        .map_err(|e| format!("cannot copy the token: {e}"))?;
    Ok(Handle(copy))
}

/// A copy of `token` with the same user, groups and level and none of the privileges (bar the one every process needs
/// to open files at all). The level is what lets the program type into windows; the privileges it never needs.
fn remove_privileges(token: &Handle) -> Result<Handle, String> {
    let mut restricted = HANDLE::default();
    // SAFETY: a live token; the new one is closed by `Handle`.
    unsafe { CreateRestrictedToken(token.0, DISABLE_MAX_PRIVILEGE, None, None, None, &mut restricted) }
        .map_err(|e| format!("cannot take the privileges off the token: {e}"))?;
    Ok(Handle(restricted))
}

/// This service's own token (SYSTEM), as a copy that belongs to `session` and has no privileges left.
fn program_token(session: u32) -> Result<Handle, String> {
    let mut own = HANDLE::default();
    // SAFETY: the pseudo handle of this process; the token is closed by `Handle`.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own) }.map_err(|e| format!("cannot open the service's token: {e}"))?;
    let primary = duplicate_primary(&Handle(own))?;
    // SAFETY: a live token and a 4-byte session number; this is what the service's privileges are for.
    unsafe { SetTokenInformation(primary.0, TokenSessionId, (&session as *const u32).cast(), std::mem::size_of::<u32>() as u32) }
        .map_err(|e| format!("cannot move the token to session {session}: {e}"))?;
    remove_privileges(&primary)
}

/// Starts `command` with `token`, on the user's desktop, without a window.
fn launch(token: &Handle, exe: &std::path::Path, command: &str) -> Result<Handle, String> {
    let application = wide(exe.as_os_str());
    let mut command_line = wide(std::ffi::OsStr::new(command));
    let directory = exe.parent().map(|d| wide(d.as_os_str()));
    let mut desktop = wide(std::ffi::OsStr::new(r"winsta0\default"));
    let startup = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, lpDesktop: PWSTR(desktop.as_mut_ptr()), ..Default::default() };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: every string is NUL-terminated and outlives the call; the command line is a buffer the call may change.
    unsafe {
        CreateProcessAsUserW(
            Some(token.0),
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW | NORMAL_PRIORITY_CLASS,
            None,
            directory.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
            &startup,
            &mut info,
        )
    }
    .map_err(|e| format!("cannot start the input program in the user's session: {e}"))?;
    drop(Handle(info.hThread));
    Ok(Handle(info.hProcess))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel, TokenPrivileges, TOKEN_MANDATORY_LABEL,
    };

    use super::*;

    #[test]
    fn the_stop_event_is_global_and_never_the_same_twice() {
        let a = stop_event_name();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = stop_event_name();
        assert!(a.starts_with(r"Global\HdrCastInputStop-"), "{a}");
        assert_ne!(a, b);
    }

    #[test]
    fn started_by_hand_it_says_who_starts_it() {
        let error = run("HdrCastInputTestNoSuchService", "S-1-5-21-1-2-3-1001").unwrap_err();
        assert!(error.contains("service"), "{error}");
    }

    fn own_token() -> Handle {
        let mut own = HANDLE::default();
        // SAFETY: the pseudo handle of this process; the token is closed by `Handle`.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own) }.unwrap();
        Handle(own)
    }

    /// What `GetTokenInformation` says for `class`, in a buffer that holds pointers (8-byte units).
    fn token_information(token: &Handle, class: windows::Win32::Security::TOKEN_INFORMATION_CLASS) -> Vec<u64> {
        let mut needed = 0u32;
        // SAFETY: the first call only asks how large the answer is.
        unsafe {
            let _ = GetTokenInformation(token.0, class, None, 0, &mut needed);
        }
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
        // SAFETY: `buffer` has at least `needed` bytes.
        unsafe { GetTokenInformation(token.0, class, Some(buffer.as_mut_ptr().cast()), needed, &mut needed) }.unwrap();
        buffer
    }

    /// The numbers of the privileges a token has.
    fn privileges(token: &Handle) -> Vec<u32> {
        let buffer = token_information(token, TokenPrivileges);
        // SAFETY: the buffer holds a TOKEN_PRIVILEGES with `PrivilegeCount` entries after the header.
        unsafe {
            let all = &*(buffer.as_ptr() as *const TOKEN_PRIVILEGES);
            (0..all.PrivilegeCount as usize).map(|i| all.Privileges.as_ptr().add(i).read().Luid.LowPart).collect()
        }
    }

    /// The integrity level: 0x2000 medium, 0x3000 high, 0x4000 system.
    fn integrity(token: &Handle) -> u32 {
        let buffer = token_information(token, TokenIntegrityLevel);
        // SAFETY: the buffer holds a TOKEN_MANDATORY_LABEL whose SID points into the same buffer.
        unsafe {
            let label = &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL);
            let count = *GetSidSubAuthorityCount(label.Label.Sid);
            *GetSidSubAuthority(label.Label.Sid, u32::from(count) - 1)
        }
    }

    #[test]
    fn the_token_for_the_input_program_keeps_the_user_and_the_level_and_loses_every_privilege() {
        let own = own_token();
        let copy = remove_privileges(&duplicate_primary(&own).unwrap()).unwrap();
        assert!(privileges(&own).len() > 1, "this test runs with some privileges");
        // 23 is SeChangeNotifyPrivilege, which every process needs to open files at all
        assert_eq!(privileges(&copy), [23]);
        assert_eq!(sid_of_token(copy.0), sid_of_token(own.0));
        assert_eq!(integrity(&copy), integrity(&own), "the level is what lets it type into higher windows");
    }

    #[test]
    fn a_privilege_that_is_held_is_turned_on_and_one_that_is_not_held_is_not() {
        // 19 is SeShutdownPrivilege: held (and off) for administrators, not held by anybody else
        let held = privileges(&own_token()).contains(&19);
        assert_eq!(enable_privilege("SeShutdownPrivilege"), held);
        assert!(!enable_privilege("SeNoSuchPrivilege"));
    }

    #[test]
    #[ignore = "starts a process on this user's desktop"]
    fn a_program_starts_with_such_a_token_and_says_how_it_ended() {
        // Straight from the process' own token, so that it is "a restricted version of the caller's token" and starting
        // a process with it needs no privilege (the service has them, and the token it uses has been moved to a session).
        let copy = remove_privileges(&own_token()).unwrap();
        let cmd = Path::new(&std::env::var_os("SystemRoot").unwrap()).join("System32").join("cmd.exe");
        let process = launch(&copy, &cmd, "cmd.exe /c exit 7").unwrap();
        // SAFETY: a live process handle.
        assert_eq!(unsafe { WaitForSingleObject(process.0, 10_000) }, WAIT_OBJECT_0);
        let mut code = 0u32;
        // SAFETY: a live process handle.
        unsafe { GetExitCodeProcess(process.0, &mut code) }.unwrap();
        assert_eq!(code, 7);
    }

    #[test]
    #[ignore = "needs somebody to be signed in on this computer"]
    fn somebody_is_signed_in_and_active() {
        assert!(!active_sessions().is_empty());
    }
}
