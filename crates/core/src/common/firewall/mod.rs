//! Whether Windows Firewall lets friends reach this program, and the one-time fix (docs/network.md §5).
//!
//! Without a rule Windows only lets in packets from an address and port the program sent to; a friend behind a
//! router that picks a new port per destination arrives from a port we never sent to. The rule is per program
//! path, so a new version in a new folder needs its own. Reading needs no rights; adding the rule runs this exe
//! elevated.
//!
//! The rule evaluation is plain data in, verdict out, and runs (and is tested) on every platform. Only the
//! functions that talk to the firewall are Windows-only; elsewhere [`check`] answers
//! [`FirewallVerdict::NotApplicable`].

use std::path::Path;
use std::time::Duration;

pub mod row;
#[cfg(windows)]
mod windows;

pub const PROFILE_ALL: i32 = 0x7FFF_FFFF;
pub const PROTOCOL_UDP: i32 = 17;
pub const PROTOCOL_ANY: i32 = 256;
/// The command-line verb the app answers with "add the rule for me and exit" (run elevated).
pub const ALLOW_VERB: &str = "firewall-allow";
/// How long [`allow_elevated`] waits for the elevated copy (the user may take a while over the UAC prompt).
pub const ALLOW_TIMEOUT: Duration = Duration::from_secs(180);

/// One Windows Firewall rule, reduced to what decides whether a friend's UDP reaches HdrCast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirewallRule {
    pub name: String,
    pub application: String,
    pub enabled: bool,
    pub inbound: bool,
    pub allow: bool,
    /// 6 TCP, 17 UDP, 256 any.
    pub protocol: i32,
    /// NET_FW_PROFILE2 bits: 1 domain, 2 private, 4 public.
    pub profiles: i32,
    pub local_ports: String,
    pub remote_addresses: String,
}

impl Default for FirewallRule {
    fn default() -> Self {
        FirewallRule {
            name: String::new(),
            application: String::new(),
            enabled: false,
            inbound: false,
            allow: false,
            protocol: PROTOCOL_ANY,
            profiles: PROFILE_ALL,
            local_ports: "*".to_string(),
            remote_addresses: "*".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FirewallVerdict {
    /// The settings could not be read (another firewall product owns them, or COM failed).
    Unknown = 0,
    /// Windows Firewall is off for the networks in use.
    Off = 1,
    /// An allow rule lets UDP from any address and port reach the program.
    Allowed = 2,
    /// No such rule: only answers to what the program sent get in.
    NotAllowed = 3,
    /// A block rule for the program (e.g. from answering Cancel on Windows' own prompt) beats any allow rule.
    Blocked = 4,
    /// "Block all incoming connections" is on for a network in use: no rule can help.
    BlockAll = 5,
    /// Not Windows: there is no Windows Firewall to ask, and nothing here stands in the way.
    NotApplicable = 6,
}

/// Why the rule could not be added (the text is what goes into the log).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct FirewallError(pub String);

/// Why running the elevated copy did not put the rule in place: a short reason for the log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AllowElevatedError {
    /// The user said No on the UAC prompt.
    #[error("cancelled")]
    Cancelled,
    #[error("it did not start")]
    DidNotStart,
    #[error("it did not finish")]
    DidNotFinish,
    #[error("adding the rule failed (see the log)")]
    RuleNotAdded,
    #[error("{0}")]
    Os(String),
}

/// Pure: what the rules mean for unsolicited UDP to `exe_path` on the active profiles.
pub fn evaluate(
    exe_path: &str,
    rules: &[FirewallRule],
    active_profiles: i32,
    enabled_profiles: i32,
    block_all_profiles: i32,
) -> FirewallVerdict {
    let guarded = active_profiles & enabled_profiles;
    if guarded == 0 {
        return FirewallVerdict::Off;
    }
    if guarded & block_all_profiles != 0 {
        return FirewallVerdict::BlockAll;
    }
    let mut covered = 0;
    let mut blocked = false;
    for r in rules {
        if !r.enabled || !r.inbound || (r.protocol != PROTOCOL_UDP && r.protocol != PROTOCOL_ANY) {
            continue;
        }
        if r.profiles & guarded == 0 || !same_program(&r.application, exe_path) {
            continue;
        }
        if !r.allow {
            blocked = true;
        } else if is_any(&r.local_ports) && is_any(&r.remote_addresses) {
            covered |= r.profiles; // our port is random
        }
    }
    if blocked {
        return FirewallVerdict::Blocked;
    }
    if covered & guarded == guarded {
        FirewallVerdict::Allowed
    } else {
        FirewallVerdict::NotAllowed
    }
}

/// The same file, however the rule spells it (case, environment variables, relative parts).
pub fn same_program(rule_application: &str, exe_path: &str) -> bool {
    program_matcher(exe_path)(rule_application)
}

/// [`same_program`] against one executable, for walking hundreds of rules.
fn program_matcher(exe_path: &str) -> impl Fn(&str) -> bool {
    let exe = normalize_path(exe_path);
    move |rule_application: &str| {
        // rules for every program are not ours to judge
        !rule_application.trim().is_empty() && eq_ignore_case(&normalize_path(rule_application), &exe)
    }
}

fn normalize_path(path: &str) -> String {
    let expanded = expand_environment_variables(path.trim().trim_matches('"'));
    full_path(&expanded).replace('/', "\\").trim_end_matches('\\').to_string()
}

/// %NAME% → the variable's value, as Windows spells rule paths ("%SystemRoot%\system32\svchost.exe").
/// An unknown name stays as written.
fn expand_environment_variables(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let value = after.find('%').and_then(|end| {
            let name = &after[..end];
            let usable = !name.is_empty() && !name.contains(['=', '\0']);
            usable.then(|| std::env::var_os(name)).flatten().map(|value| (value, end))
        });
        match value {
            Some((value, end)) => {
                out.push_str(&value.to_string_lossy());
                rest = &after[end + 1..];
            }
            None => {
                // not a variable: keep the '%'; the next one may still open a name
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Absolute, with "." and ".." resolved, the way Windows does it (no file is touched). Elsewhere paths are
/// compared as written: a Windows rule path means nothing to another system's path rules.
fn full_path(path: &str) -> String {
    if cfg!(windows) {
        if let Ok(full) = std::path::absolute(path) {
            return full.to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

/// Ordinal comparison ignoring case, character by character (file names differ by simple case mapping only:
/// "ß" is not "SS").
fn eq_ignore_case(a: &str, b: &str) -> bool {
    fn upper(c: char) -> char {
        let mut mapped = c.to_uppercase();
        match (mapped.next(), mapped.next()) {
            (Some(u), None) => u,
            _ => c,
        }
    }
    a.chars().map(upper).eq(b.chars().map(upper))
}

fn is_any(value: &str) -> bool {
    value.is_empty() || value == "*" || value.eq_ignore_ascii_case("Any")
}

/// Reads the live settings; [`FirewallVerdict::Unknown`] when they cannot be read. Blocks for a moment (a
/// Windows 11 PC has hundreds of rules): call it from a background thread.
pub fn check(exe_path: &Path) -> FirewallVerdict {
    #[cfg(windows)]
    {
        windows::check(exe_path)
    }
    #[cfg(not(windows))]
    {
        let _ = exe_path;
        FirewallVerdict::NotApplicable
    }
}

/// Every rule (inbound or outbound, any state) that names `exe_path`; `None` when unreadable or not Windows.
pub fn rules_for(exe_path: &Path) -> Option<Vec<FirewallRule>> {
    #[cfg(windows)]
    {
        windows::rules_for(exe_path)
    }
    #[cfg(not(windows))]
    {
        let _ = exe_path;
        None
    }
}

/// Needs administrator rights: switches off inbound block rules for `exe_path` and adds an allow rule (UDP, every
/// network type) unless one already covers it.
pub fn allow_program(exe_path: &Path, rule_name: &str, description: &str) -> Result<(), FirewallError> {
    #[cfg(windows)]
    {
        windows::allow_program(exe_path, rule_name, description)
    }
    #[cfg(not(windows))]
    {
        let _ = (exe_path, rule_name, description);
        Err(FirewallError("Windows Firewall only exists on Windows".to_string()))
    }
}

/// Runs "`exe_path` firewall-allow" as administrator (Windows shows its UAC prompt) and waits up to `timeout`
/// ([`ALLOW_TIMEOUT`]). `Ok` when the rule is in place.
pub fn allow_elevated(exe_path: &Path, timeout: Duration) -> Result<(), AllowElevatedError> {
    #[cfg(windows)]
    {
        windows::allow_elevated(exe_path, timeout)
    }
    #[cfg(not(windows))]
    {
        let _ = (exe_path, timeout);
        Err(AllowElevatedError::Os("Windows Firewall only exists on Windows".to_string()))
    }
}

/// What `main` returns, and logs, when the app was started as `<exe> firewall-allow`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowVerbOutcome {
    /// 0 when the rule is in place; [`allow_elevated`] reads anything else as failure.
    pub exit_code: i32,
    /// For the app log (the elevated copy has no window to say it in).
    pub log_line: String,
}

/// The whole of the `firewall-allow` verb: adds the rule for the running executable. The Allow button starts
/// this elevated (Windows shows its UAC prompt): add the rule, no window, exit.
pub fn run_allow_verb(rule_name: &str, description: &str) -> AllowVerbOutcome {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            return AllowVerbOutcome { exit_code: 1, log_line: format!("firewall: could not add the rule: {e}") };
        }
    };
    match allow_program(&exe, rule_name, description) {
        Ok(()) => AllowVerbOutcome { exit_code: 0, log_line: format!("firewall: inbound UDP allowed for {}", exe.display()) },
        Err(error) => AllowVerbOutcome {
            exit_code: 1,
            log_line: format!("firewall: could not add the rule for {}: {error}", exe.display()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_variables_and_stray_percent_signs_stay_as_written() {
        // PATH is the one variable every platform's test run has (setting one here would race other tests).
        if let Ok(path) = std::env::var("PATH") {
            assert_eq!(expand_environment_variables(r"%PATH%\a.exe"), format!(r"{path}\a.exe"));
            assert_eq!(expand_environment_variables("a%PATH%b%PATH%"), format!("a{path}b{path}"));
        }
        assert_eq!(expand_environment_variables(r"%HDRCAST_NO_SUCH_VAR%\a.exe"), r"%HDRCAST_NO_SUCH_VAR%\a.exe");
        assert_eq!(expand_environment_variables("100% sure"), "100% sure");
        assert_eq!(expand_environment_variables("%%"), "%%");
        assert_eq!(expand_environment_variables("%=%"), "%=%");
        assert_eq!(expand_environment_variables(""), "");
    }

    #[test]
    fn paths_match_whatever_their_spelling() {
        let exe = r"D:\HdrCast-0.2.4\HdrCastSender.exe";
        assert!(same_program(exe, exe));
        assert!(same_program(r#"  "d:/hdrcast-0.2.4/HDRCASTSENDER.EXE"  "#, exe));
        assert!(same_program(r"D:\HdrCast-0.2.4\HdrCastSender.exe\", exe));
        assert!(!same_program(r"D:\HdrCast-0.2.4\HdrCastReceiver.exe", exe));
        assert!(!same_program("", exe));
        assert!(!same_program("   ", exe));
        // simple case mapping only, like the file system: ß is not SS
        assert!(same_program(r"D:\Ärger\a.exe", r"d:\ärger\A.EXE"));
        assert!(!same_program(r"D:\straße\a.exe", r"D:\STRASSE\a.exe"));
    }

    #[cfg(windows)]
    #[test]
    fn relative_parts_are_resolved_on_windows() {
        assert!(same_program(r"D:\HdrCast\tools\..\HdrCast.exe", r"D:\HdrCast\HdrCast.exe"));
        assert!(same_program(r"D:\HdrCast\.\HdrCast.exe", r"D:\HdrCast\HdrCast.exe"));
    }

    #[test]
    fn any_is_spelled_three_ways() {
        assert!(is_any(""));
        assert!(is_any("*"));
        assert!(is_any("ANY"));
        assert!(!is_any("5000"));
        assert!(!is_any("LocalSubnet"));
    }

    #[cfg(not(windows))]
    #[test]
    fn there_is_no_windows_firewall_elsewhere() {
        let exe = Path::new("/Applications/HdrCast.app/Contents/MacOS/HdrCast");
        assert_eq!(check(exe), FirewallVerdict::NotApplicable);
        assert_eq!(rules_for(exe), None);
        assert!(allow_program(exe, "HdrCast", "d").is_err());
        assert!(allow_elevated(exe, Duration::from_secs(1)).is_err());
    }
}
