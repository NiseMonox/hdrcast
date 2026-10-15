//! The Windows side: the firewall's COM objects (HNetCfg.FwPolicy2 / HNetCfg.FWRule) and the elevated start.
//!
//! Every call runs on a short-lived thread of its own that initializes COM for itself, so nothing depends on
//! (or disturbs) the apartment of the thread that asked: a UI thread is usually single-threaded-apartment and
//! may already have OLE going.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::time::Duration;

use windows::core::{IUnknown, Interface, BSTR, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE, S_OK, VARIANT_FALSE, VARIANT_TRUE, WAIT_OBJECT_0};
use windows::Win32::NetworkManagement::WindowsFirewall::{
    INetFwPolicy2, INetFwRule, INetFwRules, NetFwPolicy2, NetFwRule, NET_FW_ACTION_ALLOW, NET_FW_PROFILE_TYPE2,
    NET_FW_RULE_DIR_IN,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IDispatch, CLSCTX_ALL, COINIT, COINIT_APARTMENTTHREADED,
    COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED,
};
use windows::Win32::System::Ole::IEnumVARIANT;
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows::Win32::System::Variant::{VARIANT, VT_DISPATCH, VT_EMPTY, VT_UNKNOWN};
use windows::Win32::UI::Shell::{
    ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

use super::{
    evaluate, is_any, program_matcher, AllowElevatedError, FirewallError, FirewallRule, FirewallVerdict, ALLOW_VERB,
    PROFILE_ALL, PROTOCOL_ANY, PROTOCOL_UDP,
};

/// The grouping the rule is filed under in Windows' own firewall console.
const RULE_GROUPING: &str = "HdrCast";

pub fn check(exe_path: &Path) -> FirewallVerdict {
    let exe = exe_path.to_string_lossy().into_owned();
    on_com_thread(COINIT_MULTITHREADED, move || read_verdict(&exe))
        .and_then(Result::ok)
        .unwrap_or(FirewallVerdict::Unknown)
}

pub fn rules_for(exe_path: &Path) -> Option<Vec<FirewallRule>> {
    let exe = exe_path.to_string_lossy().into_owned();
    on_com_thread(COINIT_MULTITHREADED, move || {
        let policy = create_policy()?;
        // SAFETY: a COM method call on a live interface pointer.
        let rules = unsafe { policy.Rules() }?;
        Ok::<_, windows::core::Error>(rules_naming(&rules, &exe)?.into_iter().map(|(_, rule)| rule).collect())
    })?
    .ok()
}

pub fn allow_program(exe_path: &Path, rule_name: &str, description: &str) -> Result<(), FirewallError> {
    let exe = exe_path.to_string_lossy().into_owned();
    let (rule_name, description) = (rule_name.to_string(), description.to_string());
    match on_com_thread(COINIT_MULTITHREADED, move || add_rule(&exe, &rule_name, &description)) {
        Some(Ok(())) => Ok(()),
        Some(Err(e)) => Err(FirewallError(describe(&e))),
        None => Err(FirewallError("the firewall's COM interface could not be started".to_string())),
    }
}

pub fn allow_elevated(exe_path: &Path, timeout: Duration) -> Result<(), AllowElevatedError> {
    let exe = wide(exe_path.as_os_str());
    // u32::MAX itself would mean "wait forever"
    let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1).min(u32::MAX - 1);
    // ShellExecuteEx may hand the launch to shell extensions that need a single-threaded apartment.
    on_com_thread(COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE, move || run_elevated(&exe, timeout_ms))
        .unwrap_or(Err(AllowElevatedError::DidNotStart))
}

/// Runs `work` on a new thread with COM initialized as `apartment`; `None` when that could not be done.
/// `work` must not let a COM object escape: COM is shut down on that thread when it returns.
fn on_com_thread<T: Send + 'static>(apartment: COINIT, work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    std::thread::Builder::new()
        .name("firewall".to_string())
        .spawn(move || {
            // SAFETY: a fresh thread nobody else initialized; balanced by CoUninitialize below.
            if unsafe { CoInitializeEx(None, apartment) }.is_err() {
                return None;
            }
            let result = work();
            // SAFETY: `work` has returned, so every COM object it created is released.
            unsafe { CoUninitialize() };
            Some(result)
        })
        .ok()?
        .join()
        .ok()?
}

fn create_policy() -> windows::core::Result<INetFwPolicy2> {
    // SAFETY: plain COM activation; the result is an owned interface pointer.
    unsafe { CoCreateInstance(&NetFwPolicy2, None, CLSCTX_ALL) }
}

fn read_verdict(exe: &str) -> windows::core::Result<FirewallVerdict> {
    let policy = create_policy()?;
    // SAFETY: COM method calls on a live interface pointer.
    unsafe {
        let active = policy.CurrentProfileTypes()?;
        let (mut enabled, mut block_all) = (0, 0);
        for profile in [1, 2, 4] {
            if policy.get_FirewallEnabled(NET_FW_PROFILE_TYPE2(profile))?.as_bool() {
                enabled |= profile;
            }
            if policy.get_BlockAllInboundTraffic(NET_FW_PROFILE_TYPE2(profile))?.as_bool() {
                block_all |= profile;
            }
        }
        let mine: Vec<FirewallRule> = rules_naming(&policy.Rules()?, exe)?.into_iter().map(|(_, rule)| rule).collect();
        Ok(evaluate(exe, &mine, active, enabled, block_all))
    }
}

/// Every rule that names `exe`, with the COM object it was read from.
fn rules_naming(rules: &INetFwRules, exe: &str) -> windows::core::Result<Vec<(INetFwRule, FirewallRule)>> {
    let is_mine = program_matcher(exe);
    let mut mine = Vec::new();
    for_each_rule(rules, |rule| {
        // One property per rule first: a Windows 11 PC has hundreds of rules.
        // SAFETY: a COM method call on a live interface pointer.
        let application = unsafe { rule.ApplicationName() }?.to_string();
        if is_mine(&application) {
            mine.push((rule.clone(), read_rule(rule, application)?));
        }
        Ok(())
    })?;
    Ok(mine)
}

/// Walks the rule collection through its `_NewEnum`, an IEnumVARIANT that hands out one rule (as an IDispatch in
/// a VARIANT) per call. Anything else is an error, so a check reads as Unknown instead of "no rules": the first
/// C# version read the enumerator wrongly and saw none, which looked exactly like "no rule for HdrCast".
fn for_each_rule(
    rules: &INetFwRules,
    mut each: impl FnMut(&INetFwRule) -> windows::core::Result<()>,
) -> windows::core::Result<()> {
    // SAFETY: COM method calls on live interface pointers; `Next` writes at most one VARIANT into `one`.
    unsafe {
        let items: IEnumVARIANT = rules._NewEnum()?.cast()?;
        loop {
            let mut one = [VARIANT::default()];
            let mut fetched = 0u32;
            let hr = items.Next(&mut one, &mut fetched);
            hr.ok()?;
            if hr != S_OK || fetched == 0 {
                return Ok(()); // S_FALSE: the end of the list
            }
            let item = &one[0];
            let rule: INetFwRule = match item.vt() {
                VT_DISPATCH => IDispatch::try_from(item)?.cast()?,
                VT_UNKNOWN => IUnknown::try_from(item)?.cast()?,
                VT_EMPTY => continue,
                _ => return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::TYPE_E_TYPEMISMATCH)),
            };
            each(&rule)?;
        }
    }
}

fn read_rule(rule: &INetFwRule, application: String) -> windows::core::Result<FirewallRule> {
    let or_any = |value: BSTR| if value.is_empty() { "*".to_string() } else { value.to_string() };
    // SAFETY: COM property reads on a live interface pointer.
    unsafe {
        Ok(FirewallRule {
            name: rule.Name()?.to_string(),
            application,
            enabled: rule.Enabled()?.as_bool(),
            inbound: rule.Direction()? == NET_FW_RULE_DIR_IN,
            allow: rule.Action()? == NET_FW_ACTION_ALLOW,
            protocol: rule.Protocol()?,
            profiles: rule.Profiles()?,
            local_ports: or_any(rule.LocalPorts()?),
            remote_addresses: or_any(rule.RemoteAddresses()?),
        })
    }
}

fn add_rule(exe: &str, rule_name: &str, description: &str) -> windows::core::Result<()> {
    let policy = create_policy()?;
    // SAFETY: COM method calls on live interface pointers.
    unsafe {
        let rules = policy.Rules()?;
        let mut covered = false;
        for (rule, r) in rules_naming(&rules, exe)? {
            let udp = r.protocol == PROTOCOL_UDP || r.protocol == PROTOCOL_ANY;
            if r.enabled && r.inbound && !r.allow && udp {
                rule.SetEnabled(VARIANT_FALSE)?; // switched off rather than deleted: the user can see it
            } else if r.enabled
                && r.inbound
                && r.allow
                && udp
                && r.profiles & PROFILE_ALL == PROFILE_ALL
                && is_any(&r.local_ports)
                && is_any(&r.remote_addresses)
            {
                covered = true;
            }
        }
        if !covered {
            let rule: INetFwRule = CoCreateInstance(&NetFwRule, None, CLSCTX_ALL)?;
            rule.SetName(&BSTR::from(rule_name))?;
            rule.SetDescription(&BSTR::from(description))?;
            rule.SetApplicationName(&BSTR::from(exe))?;
            rule.SetProtocol(PROTOCOL_UDP)?;
            rule.SetDirection(NET_FW_RULE_DIR_IN)?;
            rule.SetAction(NET_FW_ACTION_ALLOW)?;
            rule.SetProfiles(PROFILE_ALL)?;
            rule.SetGrouping(&BSTR::from(RULE_GROUPING))?;
            rule.SetEnabled(VARIANT_TRUE)?;
            rules.Add(&rule)?;
        }
    }
    Ok(())
}

fn run_elevated(exe: &[u16], timeout_ms: u32) -> Result<(), AllowElevatedError> {
    let verb = wide("runas".as_ref());
    let parameters = wide(ALLOW_VERB.as_ref());
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
        return Err(if e.code() == ERROR_CANCELLED.to_hresult() {
            AllowElevatedError::Cancelled // the user said No on the UAC prompt
        } else {
            AllowElevatedError::Os(describe(&e))
        });
    }
    if info.hProcess.is_invalid() {
        return Err(AllowElevatedError::DidNotStart);
    }
    let process = ProcessHandle(info.hProcess);
    // SAFETY: `process` is a process handle we own until it is dropped.
    unsafe {
        if WaitForSingleObject(process.0, timeout_ms) != WAIT_OBJECT_0 {
            return Err(AllowElevatedError::DidNotFinish);
        }
        let mut exit_code = 1u32;
        GetExitCodeProcess(process.0, &mut exit_code).map_err(|e| AllowElevatedError::Os(describe(&e)))?;
        if exit_code == 0 {
            Ok(())
        } else {
            Err(AllowElevatedError::RuleNotAdded)
        }
    }
}

struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from ShellExecuteExW and is closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Zero-terminated UTF-16, for the wide Win32 calls (never the ANSI ones: a path with Chinese or Japanese
/// characters must survive whatever the code page is).
fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

fn describe(e: &windows::core::Error) -> String {
    let message = e.message();
    if message.is_empty() {
        format!("error 0x{:08X}", e.code().0)
    } else {
        format!("{message} (0x{:08X})", e.code().0)
    }
}
