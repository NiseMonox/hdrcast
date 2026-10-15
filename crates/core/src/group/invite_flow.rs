//! The decisions behind the group and invite dialogs of the C# apps (`shared/Gui/InviteUi.cs`), without the
//! dialogs: each function says what to show or ask, in the exact words the old apps used, and the UI layer does
//! the showing and asking. Everything here edits a [`GroupConfig`] the caller then saves through
//! [`SharedGroupFile`](super::shared_file::SharedGroupFile), so a group created or joined in one app is there
//! in the others.

use super::config::{self, GroupConfig, MAX_DISPLAY_NAME_LENGTH};
use super::invite::{self, Invite, InviteError, InviteProblem, MAX_NAME_LENGTH};
use crate::common::truncate_utf16;

/// Caption of every message box.
pub const TITLE: &str = "HdrCast";

/// "Copy invite" with no group configured.
pub const NO_GROUP_YET: &str = "There is no group yet. Create one, or paste an invite from a friend.";
/// "Copy invite" after the message reached the clipboard.
pub const INVITE_COPIED: &str = "The invite is on the clipboard. Paste it to your friends in a chat.\n\n\
                                 Anyone who has it can join the group, so only send it to people you trust.";

pub const CREATE_GROUP_TITLE: &str = "Create a group";
pub const GROUP_NAME_PROMPT: &str = "Group name (what your friends will see):";
pub const DEFAULT_GROUP_NAME: &str = "Friends";
pub const DISPLAY_NAME_PROMPT: &str = "Your name in the group:";

pub const PASTE_INVITE_TITLE: &str = "Paste invite";
pub const PASTE_INVITE_PROMPT: &str = "Paste the whole invite message here:";
/// Longest text the paste box takes.
pub const PASTE_INVITE_MAX_LENGTH: usize = 4000;
/// Longest clipboard text that is looked at.
pub const CLIPBOARD_MAX_LENGTH: usize = 65536;

/// The message people paste into WeChat / LINE / Discord: a line of explanation plus the code.
/// (The wording still names the 0.2.4 programs; friends on that version need it.)
pub fn invite_message(invite: &Invite) -> String {
    format!(
        "HdrCast invite to \"{}\". Copy this whole message, then open HdrCastReceiver \
         (to watch) or HdrCastSender (to stream) and it will offer to join. Keep it among friends: anyone with it can watch.\r\n{}",
        invite.group_name(),
        invite::encode(invite)
    )
}

/// "Copy invite": the text for the clipboard, or [`NO_GROUP_YET`] to show instead. After a successful copy show
/// [`INVITE_COPIED`]; when the clipboard refuses, [`clipboard_failed`].
pub fn invite_message_for(group: &GroupConfig) -> Result<String, &'static str> {
    group.try_get_invite().map(|invite| invite_message(&invite)).ok_or(NO_GROUP_YET)
}

pub fn clipboard_failed(reason: &str) -> String {
    format!("Could not use the clipboard: {reason}")
}

/// The clipboard text that is worth looking at: at most 65536 characters of it.
pub fn clip_clipboard_text(text: &str) -> &str {
    truncate_utf16(text, CLIPBOARD_MAX_LENGTH)
}

/// A one-line text prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPrompt {
    pub title: String,
    pub label: &'static str,
    pub initial: String,
    /// In UTF-16 code units, as the C# text boxes counted.
    pub max_length: usize,
}

/// What "Create group" asks, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateGroupPlan {
    /// A yes/no question to ask first when this PC already is in a group; "no" ends it.
    pub confirm_replace: Option<String>,
    pub group_name: TextPrompt,
    pub display_name: TextPrompt,
}

/// "Create group", step 1: the questions. An empty or cancelled answer to either prompt ends it; otherwise call
/// [`create_group`] with the two answers.
pub fn plan_create_group(current: &GroupConfig) -> CreateGroupPlan {
    let configured = current.is_configured();
    CreateGroupPlan {
        confirm_replace: configured.then(|| {
            format!(
                "This PC is already in the group \"{}\".\n\n\
                 Create a new group anyway? Friends will need the new invite; the old one stops working for you.",
                current.group_name
            )
        }),
        group_name: TextPrompt {
            title: CREATE_GROUP_TITLE.to_string(),
            label: GROUP_NAME_PROMPT,
            initial: if configured { current.group_name.clone() } else { DEFAULT_GROUP_NAME.to_string() },
            max_length: MAX_NAME_LENGTH,
        },
        display_name: display_name_prompt(current, CREATE_GROUP_TITLE.to_string()),
    }
}

/// "Create group", step 2: a brand-new group on the default signaling server, as the config to save (after which
/// the old apps put the invite on the clipboard: [`invite_message_for`]). The error is a sentence to show.
pub fn create_group(current: &GroupConfig, group_name: &str, display_name: &str) -> Result<GroupConfig, InviteProblem> {
    Ok(current.adopt(&Invite::create(group_name, None)?, display_name))
}

/// "Paste invite": true when `clipboard_text` already holds an invite, so [`decide_import`] can be called with
/// it straight away; false means ask the user to paste the message ([`PASTE_INVITE_TITLE`],
/// [`PASTE_INVITE_PROMPT`], a multi-line box of [`PASTE_INVITE_MAX_LENGTH`]) and import what they paste.
pub fn clipboard_holds_invite(clipboard_text: &str) -> bool {
    invite::try_extract(clip_clipboard_text(clipboard_text)).is_ok()
}

/// What importing a piece of text comes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportDecision {
    /// No usable invite in the text: show `message` as a warning.
    Invalid { error: InviteError, message: String },
    /// The invite is for the group (same id and key) this PC is already in: show `message`, nothing to do.
    AlreadyMember { invite: Invite, message: String },
    /// A group to join. Ask `confirm_switch` first when it is set (yes/no; "no" ends it), then `name`; an empty
    /// or cancelled name ends it; otherwise save [`join`]`(current, &invite, answer)`.
    Join { invite: Invite, confirm_switch: Option<String>, name: TextPrompt },
}

/// Decodes `text` and decides what the import dialog does next.
pub fn decide_import(current: &GroupConfig, text: &str) -> ImportDecision {
    let invite = match invite::try_extract(text) {
        Ok(invite) => invite,
        Err(error) => return ImportDecision::Invalid { error, message: error.describe() },
    };
    if let Some(mine) = current.try_get_invite() {
        if mine.group_id() == invite.group_id() && mine.group_key() == invite.group_key() {
            let message = format!("This PC is already in \"{}\".", invite.group_name());
            return ImportDecision::AlreadyMember { invite, message };
        }
    }
    let confirm_switch = current
        .is_configured()
        .then(|| format!("Leave \"{}\" and join \"{}\"?", current.group_name, invite.group_name()));
    let name = display_name_prompt(current, format!("Join \"{}\"", invite.group_name()));
    ImportDecision::Join { invite, confirm_switch, name }
}

/// The config to save once the user confirmed and gave a name (keeps this install's peer id and preferences).
pub fn join(current: &GroupConfig, invite: &Invite, display_name: &str) -> GroupConfig {
    current.adopt(invite, display_name)
}

/// The name the prompts start with: the one in use, else the operating system's user name.
pub fn default_display_name(current: &GroupConfig) -> String {
    if current.display_name.trim().is_empty() {
        config::clean_display_name(&config::os_user_name())
    } else {
        current.display_name.clone()
    }
}

fn display_name_prompt(current: &GroupConfig, title: String) -> TextPrompt {
    TextPrompt {
        title,
        label: DISPLAY_NAME_PROMPT,
        initial: default_display_name(current),
        max_length: MAX_DISPLAY_NAME_LENGTH,
    }
}

/// An invite found on the clipboard that the user has not been asked about yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardOffer {
    pub invite: Invite,
    /// The clipboard text the invite came from; "yes" continues with [`decide_import`] on it.
    pub text: String,
    /// The yes/no question to ask.
    pub question: String,
}

impl ClipboardOffer {
    /// "No": remember the invite so it is not offered again. Save the returned config.
    pub fn dismiss(&self, current: &GroupConfig) -> GroupConfig {
        GroupConfig { dismissed_fingerprint: self.invite.fingerprint(), ..current.clone() }
    }
}

/// Offers to join when the clipboard holds an invite and a window is activated (the friend just copied the
/// message from a chat and switched to HdrCast). Reads the clipboard only when its sequence number changed,
/// asks once per invite, and can be switched off in group.json ("detectClipboard").
#[derive(Debug, Default)]
pub struct ClipboardInviteWatcher {
    // Starts at 0, so an invite copied before the app was opened is offered on first activation.
    last_sequence: u64,
    busy: bool,
}

impl ClipboardInviteWatcher {
    pub fn new() -> ClipboardInviteWatcher {
        ClipboardInviteWatcher::default()
    }

    /// Call when a window is activated. `sequence` is the system's clipboard change counter
    /// (GetClipboardSequenceNumber, NSPasteboard.changeCount); `read_text` is only called when it moved, and
    /// should return "" when the clipboard holds no text or another program has it open.
    ///
    /// `Some` = ask the offer's question; until [`finished`](Self::finished) is called no other offer is made.
    pub fn check(&mut self, group: &GroupConfig, sequence: u64, read_text: impl FnOnce() -> String) -> Option<ClipboardOffer> {
        if self.busy || !group.detect_clipboard_invites || sequence == self.last_sequence {
            return None;
        }
        let text = read_text();
        self.last_sequence = sequence;
        let text = clip_clipboard_text(&text);
        let invite = invite::try_extract(text).ok()?;
        if group.try_get_invite().is_some_and(|mine| mine.group_id() == invite.group_id()) {
            return None;
        }
        if invite.fingerprint() == group.dismissed_fingerprint {
            return None;
        }

        self.busy = true;
        let question =
            format!("The clipboard holds an HdrCast invite to \"{}\".\n\nJoin this group?", invite.group_name());
        Some(ClipboardOffer { invite, text: text.to_string(), question })
    }

    /// The offer was answered (and whatever followed is done): offers may be made again.
    pub fn finished(&mut self) {
        self.busy = false;
    }
}
