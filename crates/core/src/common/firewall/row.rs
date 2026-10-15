//! The "Firewall" row of the C# apps (`shared/Gui/FirewallUi.cs`, docs/network.md §5) without its controls:
//! what the row says for each verdict, when the Allow button shows, the question asked once per run, and the
//! bookkeeping around the elevated fix. The UI layer runs [`check`] / [`allow_now`] on a
//! background thread and feeds the results in; [`FirewallRow::inbound_allowed`] is the cached answer the P2P
//! code reads.

use std::path::Path;

use super::{allow_elevated, check, AllowElevatedError, FirewallVerdict, ALLOW_TIMEOUT};

/// The description stored with the firewall rule (shown in Windows' own firewall console).
pub const RULE_DESCRIPTION: &str =
    "Lets friends in your HdrCast group connect even when their router changes ports. Added by HdrCast's Allow button; delete it to undo.";

/// The row's text before the first check finished.
pub const CHECKING: &str = "Checking…";
/// The button's caption (it carries the UAC shield).
pub const ALLOW_BUTTON: &str = "Allow…";
/// The row's text while Windows shows its prompt.
pub const WAITING_FOR_WINDOWS: &str = "Waiting for Windows (confirm the prompt)…";
/// Caption of the question [`FirewallRow::offer_once`] returns.
pub const OFFER_TITLE: &str = "HdrCast";

/// `why` for [`FirewallRow::offer_once`] when going live.
pub const WHY_WHEN_HOSTING: &str = "friends whose routers change ports (common in China) cannot connect to you.";
/// `why` for [`FirewallRow::offer_once`] when starting to watch.
pub const WHY_WHEN_WATCHING: &str = "a host whose router changes ports cannot reach you over IPv4.";

/// How loud the status is; the C# row used grey, dark orange and firebrick red.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTone {
    Quiet,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirewallStatus {
    pub text: &'static str,
    pub tone: StatusTone,
    /// The Allow button can fix it (a missing or blocking rule).
    pub show_allow_button: bool,
}

/// What the row shows for a verdict.
pub fn status(verdict: FirewallVerdict) -> FirewallStatus {
    let (text, tone, show_allow_button) = match verdict {
        FirewallVerdict::Allowed => ("Friends can reach this PC through Windows Firewall.", StatusTone::Quiet, false),
        FirewallVerdict::Off => ("Windows Firewall is off.", StatusTone::Quiet, false),
        FirewallVerdict::NotAllowed => {
            ("Only replies get in: friends whose routers change ports cannot connect.", StatusTone::Warning, true)
        }
        FirewallVerdict::Blocked => (
            "Windows Firewall blocks this program (a block rule, e.g. from Cancel on its prompt).",
            StatusTone::Error,
            true,
        ),
        FirewallVerdict::BlockAll => (
            "\"Block all incoming connections\" is on for this network (Windows Security > Firewall): no rule can help.",
            StatusTone::Error,
            false,
        ),
        FirewallVerdict::NotApplicable => ("No Windows Firewall on this system.", StatusTone::Quiet, false),
        FirewallVerdict::Unknown => (
            "Could not read the Windows Firewall settings (another firewall program?).",
            StatusTone::Quiet,
            false,
        ),
    };
    FirewallStatus { text, tone, show_allow_button }
}

/// `Some(true)`: friends' packets get in (a rule, or no firewall); `Some(false)`: they do not; `None`: not known.
pub fn inbound_allowed(verdict: FirewallVerdict) -> Option<bool> {
    match verdict {
        FirewallVerdict::Allowed | FirewallVerdict::Off | FirewallVerdict::NotApplicable => Some(true),
        FirewallVerdict::Unknown => None,
        FirewallVerdict::NotAllowed | FirewallVerdict::Blocked | FirewallVerdict::BlockAll => Some(false),
    }
}

/// The Allow button can fix it (a missing or blocking rule).
pub fn can_fix(verdict: FirewallVerdict) -> bool {
    matches!(verdict, FirewallVerdict::NotAllowed | FirewallVerdict::Blocked)
}

/// The yes/no question of [`FirewallRow::offer_once`]. `why` says what the user is about to miss.
pub fn offer_question(rule_name: &str, why: &str) -> String {
    format!(
        "Windows Firewall only lets in replies to what HdrCast sent, so {why}\n\n\
         Allow {rule_name} through Windows Firewall now? Windows asks for administrator permission once. \
         (You can also do it later with Allow… next to Firewall.)"
    )
}

/// The app-log line after an attempt to add the rule.
pub fn allow_log_line(exe_path: &Path, result: &Result<(), AllowElevatedError>) -> String {
    match result {
        Ok(()) => format!("[app] Windows Firewall: friends' UDP may now reach {}", exe_path.display()),
        Err(error) => format!("[app] Windows Firewall rule not added: {error}"),
    }
}

/// The blocking part of the Allow button: runs `<exe> firewall-allow` elevated, waits for it, then reads the
/// settings again. Call it from a background thread, between [`FirewallRow::begin_allow`] and
/// [`FirewallRow::finish_allow`].
pub fn allow_now(exe_path: &Path) -> (Result<(), AllowElevatedError>, FirewallVerdict) {
    let result = allow_elevated(exe_path, ALLOW_TIMEOUT);
    (result, check(exe_path))
}

/// The row's state between UI events. Not thread-safe by itself: keep it where the UI thread owns it (or behind
/// a mutex) and hand the blocking work to a background thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirewallRow {
    verdict: FirewallVerdict,
    busy: bool,
    offered: bool,
}

impl Default for FirewallRow {
    fn default() -> Self {
        FirewallRow::new()
    }
}

impl FirewallRow {
    pub fn new() -> FirewallRow {
        FirewallRow { verdict: FirewallVerdict::Unknown, busy: false, offered: false }
    }

    pub fn verdict(&self) -> FirewallVerdict {
        self.verdict
    }

    /// See [`inbound_allowed`].
    pub fn inbound_allowed(&self) -> Option<bool> {
        inbound_allowed(self.verdict)
    }

    /// See [`can_fix`].
    pub fn can_fix(&self) -> bool {
        can_fix(self.verdict)
    }

    /// The elevated fix is running (the button is disabled, the row says [`WAITING_FOR_WINDOWS`]).
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// True when a background [`check`] should be started now (at start, and after going live, in
    /// case the settings changed); false while the fix is running, which ends with a check of its own.
    pub fn should_refresh(&self) -> bool {
        !self.busy
    }

    /// A check finished: remember the verdict and say what the row shows now.
    pub fn show(&mut self, verdict: FirewallVerdict) -> FirewallStatus {
        self.verdict = verdict;
        status(verdict)
    }

    /// Asks once (per app run) whether to add the rule, when it matters: going live / watching with a firewall
    /// that only lets replies in. `Some` is the question to ask (caption [`OFFER_TITLE`], yes/no); on "yes" do
    /// what the Allow button does.
    pub fn offer_once(&mut self, rule_name: &str, why: &str) -> Option<String> {
        if self.offered || !self.can_fix() || self.busy {
            return None;
        }
        self.offered = true;
        Some(offer_question(rule_name, why))
    }

    /// The Allow button was pressed (or the offer accepted). True: disable the button, show
    /// [`WAITING_FOR_WINDOWS`] and run [`allow_now`] in the background; false: it is already running.
    pub fn begin_allow(&mut self) -> bool {
        if self.busy {
            return false;
        }
        self.busy = true;
        true
    }

    /// [`allow_now`] returned: the button works again; the result is the line for the app log and what the row
    /// shows now.
    pub fn finish_allow(
        &mut self,
        exe_path: &Path,
        result: &Result<(), AllowElevatedError>,
        verdict: FirewallVerdict,
    ) -> (String, FirewallStatus) {
        self.busy = false;
        (allow_log_line(exe_path, result), self.show(verdict))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [FirewallVerdict; 7] = [
        FirewallVerdict::Unknown,
        FirewallVerdict::Off,
        FirewallVerdict::Allowed,
        FirewallVerdict::NotAllowed,
        FirewallVerdict::Blocked,
        FirewallVerdict::BlockAll,
        FirewallVerdict::NotApplicable,
    ];

    #[test]
    fn the_button_shows_exactly_when_a_rule_can_fix_it() {
        for v in ALL {
            let s = status(v);
            assert_eq!(s.show_allow_button, can_fix(v), "{v:?}");
            assert!(!s.text.is_empty());
            // "not allowed" is never quiet, and what is fine is never loud
            assert_eq!(s.tone != StatusTone::Quiet, inbound_allowed(v) == Some(false), "{v:?}");
        }
        assert_eq!(status(FirewallVerdict::NotAllowed).tone, StatusTone::Warning);
        assert_eq!(status(FirewallVerdict::Blocked).tone, StatusTone::Error);
    }

    #[test]
    fn the_p2p_code_gets_yes_no_or_unknown() {
        assert_eq!(inbound_allowed(FirewallVerdict::Allowed), Some(true));
        assert_eq!(inbound_allowed(FirewallVerdict::Off), Some(true));
        assert_eq!(inbound_allowed(FirewallVerdict::NotApplicable), Some(true));
        assert_eq!(inbound_allowed(FirewallVerdict::NotAllowed), Some(false));
        assert_eq!(inbound_allowed(FirewallVerdict::Blocked), Some(false));
        assert_eq!(inbound_allowed(FirewallVerdict::BlockAll), Some(false));
        assert_eq!(inbound_allowed(FirewallVerdict::Unknown), None);
        assert_eq!(FirewallRow::new().inbound_allowed(), None, "nothing is known before the first check");
    }

    #[test]
    fn the_question_is_asked_once_and_only_when_it_helps() {
        let mut row = FirewallRow::new();
        assert_eq!(row.offer_once("HdrCast", WHY_WHEN_HOSTING), None, "nothing known yet");
        row.show(FirewallVerdict::Allowed);
        assert_eq!(row.offer_once("HdrCast", WHY_WHEN_HOSTING), None);
        row.show(FirewallVerdict::BlockAll);
        assert_eq!(row.offer_once("HdrCast", WHY_WHEN_HOSTING), None, "no rule can help");

        row.show(FirewallVerdict::NotAllowed);
        let question = row.offer_once("HdrCast Sender", WHY_WHEN_HOSTING).unwrap();
        assert_eq!(
            question,
            "Windows Firewall only lets in replies to what HdrCast sent, so friends whose routers change ports \
             (common in China) cannot connect to you.\n\nAllow HdrCast Sender through Windows Firewall now? Windows \
             asks for administrator permission once. (You can also do it later with Allow… next to Firewall.)"
        );
        assert_eq!(row.offer_once("HdrCast Sender", WHY_WHEN_WATCHING), None, "once per run");
    }

    #[test]
    fn nothing_is_offered_or_refreshed_while_the_fix_runs() {
        let mut row = FirewallRow::new();
        row.show(FirewallVerdict::Blocked);
        assert!(row.should_refresh());
        assert!(row.begin_allow());
        assert!(row.is_busy());
        assert!(!row.begin_allow(), "a second press does nothing");
        assert!(!row.should_refresh());
        assert_eq!(row.offer_once("HdrCast", WHY_WHEN_WATCHING), None);

        let exe = Path::new("HdrCast.exe");
        let (log, shown) = row.finish_allow(exe, &Err(AllowElevatedError::Cancelled), FirewallVerdict::Blocked);
        assert_eq!(log, "[app] Windows Firewall rule not added: cancelled");
        assert!(shown.show_allow_button);
        assert!(!row.is_busy());
        // the question was never asked, so it still can be
        assert!(row.offer_once("HdrCast", WHY_WHEN_WATCHING).is_some());

        assert!(row.begin_allow());
        let (log, shown) = row.finish_allow(exe, &Ok(()), FirewallVerdict::Allowed);
        assert_eq!(log, "[app] Windows Firewall: friends' UDP may now reach HdrCast.exe");
        assert_eq!(shown, status(FirewallVerdict::Allowed));
        assert_eq!(row.inbound_allowed(), Some(true));
    }

    #[test]
    fn the_short_reasons_read_well_in_the_log() {
        let exe = Path::new("x");
        for (error, text) in [
            (AllowElevatedError::Cancelled, "cancelled"),
            (AllowElevatedError::DidNotStart, "it did not start"),
            (AllowElevatedError::DidNotFinish, "it did not finish"),
            (AllowElevatedError::RuleNotAdded, "adding the rule failed (see the log)"),
            (AllowElevatedError::Os("Access is denied.".into()), "Access is denied."),
        ] {
            assert_eq!(allow_log_line(exe, &Err(error)), format!("[app] Windows Firewall rule not added: {text}"));
        }
    }
}
