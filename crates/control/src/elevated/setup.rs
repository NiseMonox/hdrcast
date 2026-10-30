//! The Windows side of the setup: where the input program lives, whether its service is there, and the two things
//! that need administrator approval (setting it up and taking it away), which the program does itself when the app
//! starts it elevated (`HdrCastInput.exe install` / `uninstall`, src/bin/hdrcast_input.rs).
//!
//! Everything here is plain Win32: the service control manager is asked directly, so nothing depends on the language
//! of a tool's output.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND, ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST,
    ERROR_SERVICE_EXISTS, ERROR_SERVICE_MARKED_FOR_DELETE, HANDLE, HLOCAL, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    GetTokenInformation, TokenElevation, TokenUser, DACL_SECURITY_INFORMATION, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW,
    OpenServiceW, QueryServiceStatus, SetServiceObjectSecurity, StartServiceW, ENUM_SERVICE_TYPE, SC_HANDLE, SC_MANAGER_CONNECT,
    SC_MANAGER_CREATE_SERVICE, SERVICE_ALL_ACCESS, SERVICE_CONFIG_DESCRIPTION, SERVICE_CONTROL_STOP, SERVICE_DEMAND_START,
    SERVICE_DESCRIPTIONW, SERVICE_ERROR, SERVICE_ERROR_NORMAL, SERVICE_NO_CHANGE, SERVICE_QUERY_STATUS, SERVICE_START,
    SERVICE_START_TYPE, SERVICE_STATUS, SERVICE_STOP, SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject};
use windows::Win32::UI::Shell::{
    SHGetKnownFolderPath, ShellExecuteExW, FOLDERID_ProgramData, FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, SEE_MASK_FLAG_NO_UI,
    SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

use super::server::Descriptor;
use super::{pipe_name, HelperStatus, HELPER_EXE, PROTOCOL, SERVICE_DISPLAY_NAME, SERVICE_NAME};

/// How long the app waits for the elevated copy (the user may take a while over Windows' prompt).
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(180);

/// DELETE, the right to remove a service.
const DELETE_RIGHT: u32 = 0x0001_0000;

// ---- where things are ---------------------------------------------------------------------------------------------

fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: the shell hands back a CoTaskMem string, which is copied and freed here.
    unsafe {
        let path = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let text = OsString::from_wide(path.as_wide());
        CoTaskMemFree(Some(path.0 as *const _));
        Some(PathBuf::from(text))
    }
}

/// The folder the program is installed in: under the program files folder, which only administrators can write to
/// (the registry, not an environment variable the user could set, says where that is). A service runs as SYSTEM:
/// the program it runs must not be one that anybody else can overwrite.
pub fn install_dir() -> PathBuf {
    known_folder(&FOLDERID_ProgramFiles).unwrap_or_else(|| PathBuf::from(r"C:\Program Files")).join("HdrCast").join("Input")
}

pub fn installed_exe() -> PathBuf {
    install_dir().join(HELPER_EXE)
}

const PROTOCOL_FILE: &str = "protocol";

/// The protocol the installed program speaks, as the installation wrote it down.
fn installed_protocol() -> Option<u8> {
    std::fs::read_to_string(install_dir().join(PROTOCOL_FILE)).ok()?.trim().parse().ok()
}

/// The program's own log (what it was asked to do, and why it could not): there is no window to say it in.
pub fn log_path() -> PathBuf {
    known_folder(&FOLDERID_ProgramData).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData")).join("HdrCast").join("input-helper.log")
}

pub fn log(line: &str) {
    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 64 * 1024) {
        let _ = std::fs::remove_file(&path);
    }
    // SAFETY: no arguments; fills a plain struct.
    let now = unsafe { GetLocalTime() };
    let stamp = format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond);
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{stamp} {line}");
    }
}

// ---- who is asking ------------------------------------------------------------------------------------------------

/// A handle that is closed when it is dropped.
pub(crate) struct Handle(pub(crate) HANDLE);

// SAFETY: a kernel handle, used by one thread at a time.
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a handle this struct owns, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn own_token() -> Result<Handle, String> {
    let mut token = HANDLE::default();
    // SAFETY: the pseudo handle of this process; `token` receives a handle that `Handle` closes.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.map_err(|e| format!("cannot open this process' token: {e}"))?;
    Ok(Handle(token))
}

/// The user of a token in the text form of its SID (`S-1-5-21-...`).
pub(crate) fn sid_of_token(token: HANDLE) -> Result<String, String> {
    let mut needed = 0u32;
    // SAFETY: the first call only asks how large the answer is (and fails for it).
    unsafe {
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
    }
    // u64 units: the answer holds pointers and wants 8-byte alignment
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
    // SAFETY: `buffer` has at least `needed` bytes.
    unsafe { GetTokenInformation(token, TokenUser, Some(buffer.as_mut_ptr().cast()), needed, &mut needed) }
        .map_err(|e| format!("cannot read the user of a token: {e}"))?;
    // SAFETY: the buffer now holds a TOKEN_USER whose SID points into the same buffer.
    let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
    let mut text = PWSTR::null();
    // SAFETY: `user.User.Sid` is a valid SID; the string it produces is freed below.
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) }.map_err(|e| format!("cannot write the user's SID: {e}"))?;
    // SAFETY: `text` is a NUL-terminated string from ConvertSidToStringSidW, a LocalAlloc block.
    let sid = unsafe {
        let sid = text.to_string();
        let _ = windows::Win32::Foundation::LocalFree(Some(HLOCAL(text.0.cast())));
        sid
    };
    sid.map_err(|e| e.to_string())
}

/// The SID of the signed-in user: the input program's pipe is named after it and open to it alone. (An elevated copy
/// started through the user's own prompt has the same SID; the service passes the user's SID to the program it
/// starts, which runs as SYSTEM.)
pub fn user_sid() -> Result<String, String> {
    sid_of_token(own_token()?.0)
}

/// Whether this process runs with administrator rights (an elevated token).
pub fn is_elevated() -> bool {
    let Ok(token) = own_token() else { return false };
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    // SAFETY: `elevation` is the size the call is told it is.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    }
    .is_ok()
        && elevation.TokenIsElevated != 0
}

// ---- what is there ------------------------------------------------------------------------------------------------

pub(crate) fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

/// Whether something listens on that pipe (or did a moment ago): it asks Windows without connecting, so it does not
/// wake the input program or keep it from leaving.
pub(crate) fn pipe_exists(pipe: &str) -> bool {
    let name = wide(OsStr::new(pipe));
    // SAFETY: a NUL-terminated name; waits a millisecond for a free instance and connects to nothing.
    if unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 1) }.as_bool() {
        return true;
    }
    // "busy" (every instance is taken at this very moment) still means it is there
    windows::core::Error::from_thread().code() != ERROR_FILE_NOT_FOUND.to_hresult()
}

pub fn status() -> HelperStatus {
    let installed = installed_exe().is_file() && service_exists(SERVICE_NAME);
    HelperStatus {
        supported: true,
        installed,
        outdated: installed && installed_protocol() != Some(PROTOCOL),
        running: user_sid().is_ok_and(|sid| pipe_exists(&pipe_name(&sid))),
        not_needed: is_elevated(),
    }
}

// ---- the service --------------------------------------------------------------------------------------------------

/// A handle to the service control manager or to one service, closed when dropped.
struct ScHandle(SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: opened by OpenSCManagerW / OpenServiceW / CreateServiceW, closed once.
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}

fn open_manager(access: u32) -> windows::core::Result<ScHandle> {
    // SAFETY: the local machine's active services database.
    unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access) }.map(ScHandle)
}

fn open_service(manager: &ScHandle, name: &str, access: u32) -> windows::core::Result<ScHandle> {
    let name = wide(OsStr::new(name));
    // SAFETY: `name` is NUL-terminated.
    unsafe { OpenServiceW(manager.0, PCWSTR(name.as_ptr()), access) }.map(ScHandle)
}

/// Whether the service is registered (any user may look: the service's access list lets the user read its status).
fn service_exists(name: &str) -> bool {
    open_manager(SC_MANAGER_CONNECT).and_then(|manager| open_service(&manager, name, SERVICE_QUERY_STATUS)).is_ok()
}

/// Starts the service, which starts the input program in the user's session. No prompt: the service's access list
/// lets the user it was set up for start it (and nothing else). Returns before the program listens.
pub fn start(name: &str) -> Result<(), String> {
    let manager = open_manager(SC_MANAGER_CONNECT).map_err(|e| format!("cannot reach the service manager: {e}"))?;
    let service = open_service(&manager, name, SERVICE_START | SERVICE_QUERY_STATUS)
        .map_err(|e| format!("Windows would not let this program start the service \"{name}\": {e}"))?;
    // SAFETY: a live service handle, no start arguments.
    match unsafe { StartServiceW(service.0, None) } {
        Ok(()) => Ok(()),
        Err(e) if e.code() == ERROR_SERVICE_ALREADY_RUNNING.to_hresult() => Ok(()),
        Err(e) => Err(format!("Windows would not start the service \"{name}\": {e}")),
    }
}

/// Asks the service to stop and waits (a few seconds) for it to have.
fn stop_service(name: &str) {
    let Ok(manager) = open_manager(SC_MANAGER_CONNECT) else { return };
    let Ok(service) = open_service(&manager, name, SERVICE_STOP | SERVICE_QUERY_STATUS) else { return };
    let mut status = SERVICE_STATUS::default();
    // SAFETY: a live service handle; the call fails harmlessly for a service that is not running.
    unsafe {
        let _ = ControlService(service.0, SERVICE_CONTROL_STOP, &mut status);
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        // SAFETY: a live service handle and a status struct to fill.
        if unsafe { QueryServiceStatus(service.0, &mut status) }.is_err() || status.dwCurrentState == SERVICE_STOPPED {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// What the service is registered with: the program in quotes (the folder has a space in its name, and an unquoted path
/// is a way in for whoever can make a file called `C:\Program.exe`), told which user's session to serve.
pub(crate) fn service_command_line(exe: &Path, user_sid: &str) -> String {
    format!("\"{}\" service --user {user_sid}", exe.display())
}

/// Who may do what with the service: SYSTEM and the administrators as with any service; the user it is for may look
/// at it and start it (that is how HdrCast brings the input program up without any prompt) and may neither stop,
/// change nor remove it.
pub(crate) fn service_sddl(user_sid: &str) -> String {
    format!("D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWRPLORC;;;{user_sid})")
}

fn is_sid(text: &str) -> bool {
    text.starts_with("S-1-") && text.chars().all(|c| c.is_ascii_digit() || c == '-' || c == 'S')
}

/// Runs `step` until it works, for a few seconds: a program that was just stopped needs a moment to let go of its file.
fn retry<T>(mut step: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut tries = 0;
    loop {
        match step() {
            Ok(v) => return Ok(v),
            Err(e) if tries >= 20 || e.kind() == std::io::ErrorKind::NotFound => return Err(e),
            Err(_) => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(150));
            }
        }
    }
}

/// Elevated: puts this program into `dir`, registers the service `service` that runs it for the user with `user_sid`
/// (as SYSTEM, started on demand), and writes down which protocol it speaks.
pub fn install(user_sid: &str, dir: &Path, service: &str) -> Result<(), String> {
    // goes into a command line and an access list
    if !is_sid(user_sid) {
        return Err(format!("\"{user_sid}\" is not a user SID"));
    }
    let source = std::env::current_exe().map_err(|e| format!("cannot tell where this program is: {e}"))?;
    let target = dir.join(HELPER_EXE);
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    // a service that is running holds its file
    stop_service(service);
    if source != target {
        retry(|| std::fs::copy(&source, &target)).map_err(|e| format!("cannot copy the program to {}: {e}", target.display()))?;
    }
    std::fs::write(dir.join(PROTOCOL_FILE), PROTOCOL.to_string()).map_err(|e| format!("cannot write {PROTOCOL_FILE}: {e}"))?;

    let manager = open_manager(SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE).map_err(|e| format!("cannot reach the service manager: {e}"))?;
    let name = wide(OsStr::new(service));
    let display = wide(OsStr::new(SERVICE_DISPLAY_NAME));
    let command = wide(OsStr::new(&service_command_line(&target, user_sid)));
    // SAFETY: every string is NUL-terminated and outlives the call; no account named, so LocalSystem.
    let created = unsafe {
        CreateServiceW(
            manager.0,
            PCWSTR(name.as_ptr()),
            PCWSTR(display.as_ptr()),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_DEMAND_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(command.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )
    };
    let handle = match created {
        Ok(handle) => ScHandle(handle),
        // set up before (an older copy, another user): the same service, now with this program and user
        Err(e) if e.code() == ERROR_SERVICE_EXISTS.to_hresult() => {
            let existing = open_service(&manager, service, SERVICE_ALL_ACCESS).map_err(|e| format!("cannot open the service: {e}"))?;
            // SAFETY: as above; everything that is not named stays as it was.
            unsafe {
                ChangeServiceConfigW(
                    existing.0,
                    ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
                    SERVICE_START_TYPE(SERVICE_NO_CHANGE),
                    SERVICE_ERROR(SERVICE_NO_CHANGE),
                    PCWSTR(command.as_ptr()),
                    PCWSTR::null(),
                    None,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    PCWSTR(display.as_ptr()),
                )
            }
            .map_err(|e| format!("cannot update the service: {e}"))?;
            existing
        }
        Err(e) => return Err(format!("cannot create the service: {e}")),
    };

    let mut description = wide(OsStr::new(
        "Types for HdrCast into windows of programs that run as administrator. HdrCast starts it while a viewer controls this computer; it stops by itself afterwards.",
    ));
    let info = SERVICE_DESCRIPTIONW { lpDescription: PWSTR(description.as_mut_ptr()) };
    // SAFETY: `info` points to a NUL-terminated string that outlives the call. (A missing description is no reason to fail.)
    unsafe {
        let _ = ChangeServiceConfig2W(handle.0, SERVICE_CONFIG_DESCRIPTION, Some((&info as *const SERVICE_DESCRIPTIONW).cast()));
    }

    let access = Descriptor::parse(&service_sddl(user_sid))?;
    // SAFETY: a live service handle with the right to change its access list, and a descriptor that outlives the call.
    unsafe { SetServiceObjectSecurity(handle.0, DACL_SECURITY_INFORMATION, access.raw()) }
        .map_err(|e| format!("cannot let the user start the service: {e}"))
}

/// Elevated: stops the service, removes it, and removes the files `install` put in `dir`.
pub fn uninstall(dir: &Path, service: &str) -> Result<(), String> {
    stop_service(service);
    let manager = open_manager(SC_MANAGER_CONNECT).map_err(|e| format!("cannot reach the service manager: {e}"))?;
    match open_service(&manager, service, DELETE_RIGHT | SERVICE_QUERY_STATUS) {
        // SAFETY: a live service handle with the right to delete it.
        Ok(handle) => match unsafe { DeleteService(handle.0) } {
            Ok(()) => {}
            // it goes when the last handle to it is closed
            Err(e) if e.code() == ERROR_SERVICE_MARKED_FOR_DELETE.to_hresult() => {}
            Err(e) => return Err(format!("Windows would not remove the service: {e}")),
        },
        // 1 when there was no such service: that is what was asked for
        Err(e) if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => {}
        Err(e) => return Err(format!("cannot open the service: {e}")),
    }
    for name in [HELPER_EXE, PROTOCOL_FILE] {
        match retry(|| std::fs::remove_file(dir.join(name))) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot remove {}: {e}", dir.join(name).display())),
        }
    }
    // folders only if they are empty
    let _ = std::fs::remove_dir(dir);
    if let Some(parent) = dir.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

// ---- asking Windows' permission -----------------------------------------------------------------------------------

/// Why the elevated copy did not do what it was started for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptError {
    /// The user said No on Windows' prompt.
    Cancelled,
    DidNotStart,
    DidNotFinish,
    /// It ran and failed; the reason is in [`log_path`].
    Failed(u32),
    Os(String),
}

impl fmt::Display for PromptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PromptError::Cancelled => write!(f, "Windows' permission prompt was cancelled."),
            PromptError::DidNotStart => write!(f, "{HELPER_EXE} did not start."),
            PromptError::DidNotFinish => write!(f, "{HELPER_EXE} did not finish in time."),
            PromptError::Failed(code) => write!(f, "{HELPER_EXE} could not finish (code {code}); the reason is in {}.", log_path().display()),
            PromptError::Os(text) => write!(f, "{text}"),
        }
    }
}

impl std::error::Error for PromptError {}

/// Sets the service up: starts `helper` (the copy next to the app) elevated, which Windows asks the user about, and
/// waits. The program then copies itself to [`install_dir`] and registers the service.
pub fn install_with_prompt(helper: &Path, timeout: Duration) -> Result<(), PromptError> {
    let sid = user_sid().map_err(PromptError::Os)?;
    run_elevated(helper, &format!("install --user {sid}"), timeout)
}

/// Takes it away again: the same prompt, the same copy next to the app (the installed one cannot delete itself).
pub fn uninstall_with_prompt(helper: &Path, timeout: Duration) -> Result<(), PromptError> {
    run_elevated(helper, "uninstall", timeout)
}

fn run_elevated(exe: &Path, parameters: &str, timeout: Duration) -> Result<(), PromptError> {
    if !exe.is_file() {
        return Err(PromptError::Os(format!("{} is missing.", exe.display())));
    }
    let exe = wide(exe.as_os_str());
    let parameters = wide(OsStr::new(parameters));
    // u32::MAX itself would mean "wait forever"
    let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1).min(u32::MAX - 1);
    // ShellExecuteEx may hand the launch to shell extensions that need a single-threaded apartment of their own.
    std::thread::Builder::new()
        .name("input-helper-setup".into())
        .spawn(move || {
            // SAFETY: a fresh thread nobody else initialized; balanced by CoUninitialize below.
            let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.is_ok();
            let result = shell_execute_runas(&exe, &parameters, timeout_ms);
            if com {
                // SAFETY: balances the call above.
                unsafe { CoUninitialize() };
            }
            result
        })
        .map_err(|e| PromptError::Os(e.to_string()))?
        .join()
        .unwrap_or(Err(PromptError::DidNotStart))
}

fn shell_execute_runas(exe: &[u16], parameters: &[u16], timeout_ms: u32) -> Result<(), PromptError> {
    let verb = wide(OsStr::new("runas"));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        // keep the process handle, start synchronously, and report failures to us rather than in a message box
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: `info` is fully initialized and the strings it points to (zero-terminated UTF-16) outlive the call.
    if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
        return Err(if e.code() == ERROR_CANCELLED.to_hresult() { PromptError::Cancelled } else { PromptError::Os(e.to_string()) });
    }
    if info.hProcess.is_invalid() {
        return Err(PromptError::DidNotStart);
    }
    let process = Handle(info.hProcess);
    // SAFETY: `process` is a process handle we own until it is dropped.
    unsafe {
        if WaitForSingleObject(process.0, timeout_ms) != WAIT_OBJECT_0 {
            return Err(PromptError::DidNotFinish);
        }
        let mut code = 1u32;
        GetExitCodeProcess(process.0, &mut code).map_err(|e| PromptError::Os(e.to_string()))?;
        if code == 0 {
            Ok(())
        } else {
            Err(PromptError::Failed(code))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_runs_the_program_in_quotes_for_one_user() {
        let line = service_command_line(Path::new(r"C:\Program Files\HdrCast\Input\HdrCastInput.exe"), "S-1-5-21-1-2-3-1001");
        assert_eq!(line, r#""C:\Program Files\HdrCast\Input\HdrCastInput.exe" service --user S-1-5-21-1-2-3-1001"#);
    }

    #[test]
    fn the_user_may_start_the_service_and_may_not_stop_change_or_remove_it() {
        let sddl = service_sddl("S-1-5-21-1-2-3-1001");
        // the user's entry: query configuration, query status, enumerate dependents, start, interrogate, read the list
        assert!(sddl.ends_with("(A;;CCLCSWRPLORC;;;S-1-5-21-1-2-3-1001)"), "{sddl}");
        let users = sddl.rsplit('(').next().unwrap();
        for right in ["WP", "DT", "DC", "SD", "WD", "WO"] {
            assert!(!users.contains(right), "{right} must not be there: {users}");
        }
        assert!(Descriptor::parse(&sddl).is_ok(), "Windows must be able to read it");
    }

    #[test]
    fn this_user_has_a_sid_and_an_unknown_pipe_or_service_does_not_exist() {
        let sid = user_sid().unwrap();
        assert!(sid.starts_with("S-1-5-"), "{sid}");
        assert!(is_sid(&sid));
        assert!(!pipe_exists(r"\\.\pipe\hdrcast-input-test-no-such-pipe"));
        assert!(!service_exists("HdrCastInputTestNoSuchService"));
    }

    #[test]
    fn install_refuses_a_user_that_is_no_sid() {
        let dir = std::env::temp_dir().join("hdrcast-input-never-made");
        assert!(install("S-1-5-1\"/><Evil", &dir, "x").is_err());
        assert!(install("someone", &dir, "x").is_err());
        assert!(!dir.exists());
    }

    #[test]
    fn starting_a_service_that_is_not_there_says_so() {
        let error = start("HdrCastInputTestNoSuchService").unwrap_err();
        assert!(error.contains("HdrCastInputTestNoSuchService"), "{error}");
    }
}
