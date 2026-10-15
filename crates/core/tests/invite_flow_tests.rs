//! The decisions behind the invite dialogs (`shared/Gui/InviteUi.cs`), which the C# apps only exercised by hand:
//! what is asked, in which words, and what gets saved.

use hdrcast_core::invite::{self, Invite, InviteError};
use hdrcast_core::invite_flow::{self, ClipboardInviteWatcher, ImportDecision};
use hdrcast_core::{GroupConfig, InviteProblem};

fn fresh() -> GroupConfig {
    GroupConfig::default().normalize()
}

fn member_of(invite: &Invite, me: &str) -> GroupConfig {
    fresh().adopt(invite, me)
}

#[test]
fn the_invite_message_is_one_line_of_explanation_and_the_code() {
    let invite = Invite::create("周末开黑", None).unwrap();
    let message = invite_flow::invite_message(&invite);
    assert_eq!(
        message,
        format!(
            "HdrCast invite to \"周末开黑\". Copy this whole message, then open HdrCastReceiver (to watch) or \
             HdrCastSender (to stream) and it will offer to join. Keep it among friends: anyone with it can watch.\r\n{}",
            invite::encode(&invite)
        )
    );
    // the message is what gets pasted back in: the code is found in it
    assert_eq!(invite::try_extract(&message), Ok(invite.clone()));

    assert_eq!(invite_flow::invite_message_for(&member_of(&invite, "me")), Ok(message));
    assert_eq!(
        invite_flow::invite_message_for(&fresh()),
        Err("There is no group yet. Create one, or paste an invite from a friend.")
    );
    assert_eq!(invite_flow::clipboard_failed("busy"), "Could not use the clipboard: busy");
    assert_eq!(
        invite_flow::INVITE_COPIED,
        "The invite is on the clipboard. Paste it to your friends in a chat.\n\nAnyone who has it can join the group, so only send it to people you trust."
    );
}

#[test]
fn creating_a_first_group_asks_two_names_and_nothing_else() {
    let current = fresh();
    let plan = invite_flow::plan_create_group(&current);
    assert_eq!(plan.confirm_replace, None);
    assert_eq!(plan.group_name.title, "Create a group");
    assert_eq!(plan.group_name.label, "Group name (what your friends will see):");
    assert_eq!(plan.group_name.initial, "Friends");
    assert_eq!(plan.group_name.max_length, 40);
    assert_eq!(plan.display_name.title, "Create a group");
    assert_eq!(plan.display_name.label, "Your name in the group:");
    assert_eq!(plan.display_name.initial, current.display_name);
    assert_eq!(plan.display_name.max_length, 24);

    let created = invite_flow::create_group(&current, " 朋友们 ", " 小明 ").unwrap();
    assert!(created.is_configured());
    assert_eq!(created.group_name, "朋友们");
    assert_eq!(created.display_name, "小明");
    assert_eq!(created.signal_url, invite::DEFAULT_SIGNAL_URL);
    assert_eq!(created.peer_id, current.peer_id);
    assert_eq!(created.version, 1);
}

#[test]
fn creating_a_second_group_asks_first_and_names_the_old_one() {
    let current = member_of(&Invite::create("Old gang", None).unwrap(), "me");
    let plan = invite_flow::plan_create_group(&current);
    assert_eq!(
        plan.confirm_replace.as_deref(),
        Some(
            "This PC is already in the group \"Old gang\".\n\nCreate a new group anyway? Friends will need the new invite; the old one stops working for you."
        )
    );
    assert_eq!(plan.group_name.initial, "Old gang");
    assert_eq!(plan.display_name.initial, "me");

    let created = invite_flow::create_group(&current, "New gang", "me").unwrap();
    assert_ne!(created.group_id, current.group_id);
    assert_ne!(created.group_key, current.group_key);
}

#[test]
fn a_group_name_that_cannot_be_an_invite_is_refused_with_a_sentence() {
    let error = invite_flow::create_group(&fresh(), &"x".repeat(41), "me").unwrap_err();
    assert_eq!(error, InviteProblem::GroupName);
    assert_eq!(error.to_string(), "the group name must be 1-40 characters");
    assert!(invite_flow::create_group(&fresh(), "   ", "me").is_err());
}

#[test]
fn importing_text_without_a_usable_invite_says_why() {
    let current = fresh();
    for (text, error, message) in [
        ("just chatting", InviteError::NotFound, "No HdrCast invite found (it starts with \"hdrcast:\")."),
        ("hdrcast:eyJ2Ijox", InviteError::Incomplete, "The invite is cut off. Copy the whole message again."),
        (
            "hdrcast:eyJ2Ijox.AAAAAAAA",
            InviteError::Damaged,
            "The invite is damaged (checksum mismatch). Ask for it again, or copy the whole message.",
        ),
    ] {
        assert_eq!(
            invite_flow::decide_import(&current, text),
            ImportDecision::Invalid { error, message: message.to_string() },
            "{text}"
        );
    }
    assert_eq!(InviteError::NewerVersion.describe(), "This invite comes from a newer HdrCast. Update the app.");
    assert_eq!(InviteError::Invalid.describe(), "This invite is not valid.");
}

#[test]
fn a_first_invite_only_asks_for_the_name() {
    let current = fresh();
    let invite = Invite::create("Friends", None).unwrap();
    let ImportDecision::Join { invite: found, confirm_switch, name } =
        invite_flow::decide_import(&current, &invite_flow::invite_message(&invite))
    else {
        panic!("expected a join");
    };
    assert_eq!(found, invite);
    assert_eq!(confirm_switch, None);
    assert_eq!(name.title, "Join \"Friends\"");
    assert_eq!(name.label, "Your name in the group:");
    assert_eq!(name.initial, invite_flow::default_display_name(&current));
    assert_eq!(name.max_length, 24);

    let joined = invite_flow::join(&current, &found, "小红");
    assert_eq!(joined.to_invite(), Ok(invite));
    assert_eq!(joined.display_name, "小红");
    assert_eq!(joined.peer_id, current.peer_id);
}

#[test]
fn the_invite_of_the_group_this_pc_is_in_changes_nothing() {
    let invite = Invite::create("Friends", None).unwrap();
    let current = member_of(&invite, "me");
    assert_eq!(
        invite_flow::decide_import(&current, &invite::encode(&invite)),
        ImportDecision::AlreadyMember { invite, message: "This PC is already in \"Friends\".".to_string() }
    );
}

#[test]
fn another_groups_invite_asks_before_leaving_the_current_one() {
    let current = member_of(&Invite::create("Old gang", None).unwrap(), "me");
    let other = Invite::create("New gang", None).unwrap();
    let ImportDecision::Join { confirm_switch, name, .. } = invite_flow::decide_import(&current, &invite::encode(&other))
    else {
        panic!("expected a join");
    };
    assert_eq!(confirm_switch.as_deref(), Some("Leave \"Old gang\" and join \"New gang\"?"));
    assert_eq!(name.initial, "me");
}

#[test]
fn the_same_group_id_with_another_key_is_another_group() {
    // A re-made group, or somebody guessing: the id alone does not make it "already a member".
    let mine = Invite::create("Friends", None).unwrap();
    let current = member_of(&mine, "me");
    let impostor = Invite::new(mine.signal_url(), mine.group_id(), &[9u8; 32], "Friends").unwrap();
    assert!(matches!(
        invite_flow::decide_import(&current, &invite::encode(&impostor)),
        ImportDecision::Join { confirm_switch: Some(_), .. }
    ));
}

#[test]
fn only_the_first_65536_characters_of_the_clipboard_are_looked_at() {
    let code = invite::encode(&Invite::create("Friends", None).unwrap());
    assert!(invite_flow::clipboard_holds_invite(&format!("看这个 {code} 谢谢")));
    assert!(!invite_flow::clipboard_holds_invite("nothing here"));
    assert!(!invite_flow::clipboard_holds_invite(""));
    let buried = format!("{}{code}", "x".repeat(invite_flow::CLIPBOARD_MAX_LENGTH));
    assert!(!invite_flow::clipboard_holds_invite(&buried));
    assert_eq!(invite_flow::clip_clipboard_text(&buried).len(), invite_flow::CLIPBOARD_MAX_LENGTH);
    assert_eq!(invite_flow::PASTE_INVITE_TITLE, "Paste invite");
    assert_eq!(invite_flow::PASTE_INVITE_PROMPT, "Paste the whole invite message here:");
    assert_eq!(invite_flow::PASTE_INVITE_MAX_LENGTH, 4000);
}

// ---- ClipboardInviteWatcher ---------------------------------------------------------------------

#[test]
fn a_copied_invite_is_offered_once_per_clipboard_change() {
    let invite = Invite::create("Friends", None).unwrap();
    let text = invite_flow::invite_message(&invite);
    let group = fresh();
    let mut watcher = ClipboardInviteWatcher::new();

    // copied before the app was opened: offered on the first activation
    let offer = watcher.check(&group, 41, || text.clone()).expect("an offer");
    assert_eq!(offer.invite, invite);
    assert_eq!(offer.text, text);
    assert_eq!(offer.question, "The clipboard holds an HdrCast invite to \"Friends\".\n\nJoin this group?");

    // while the question is open nothing else is offered, and the clipboard is not even read
    assert!(watcher.check(&group, 42, || panic!("read while busy")).is_none());
    watcher.finished();
    // the same clipboard content again: not read, not offered
    assert!(watcher.check(&group, 41, || panic!("read without a change")).is_none());
    // a new copy of the same invite is a new clipboard change: offered again (unless dismissed, see below)
    assert!(watcher.check(&group, 43, || text.clone()).is_some());
}

#[test]
fn saying_no_is_remembered_and_saying_yes_joins() {
    let invite = Invite::create("Friends", None).unwrap();
    let text = invite::encode(&invite);
    let group = fresh();
    let mut watcher = ClipboardInviteWatcher::new();

    let offer = watcher.check(&group, 1, || text.clone()).unwrap();
    let dismissed = offer.dismiss(&group);
    watcher.finished();
    assert_eq!(dismissed.dismissed_fingerprint, invite.fingerprint());
    assert_eq!(GroupConfig { dismissed_fingerprint: String::new(), ..dismissed.clone() }, group, "nothing else changes");
    assert!(watcher.check(&dismissed, 2, || text.clone()).is_none(), "asked once, not every time");

    // a different invite is still offered
    let other = invite::encode(&Invite::create("Others", None).unwrap());
    let offer = watcher.check(&dismissed, 3, || other.clone()).expect("another invite");
    // "yes" goes on to the import dialog with the clipboard text
    assert!(matches!(invite_flow::decide_import(&dismissed, &offer.text), ImportDecision::Join { .. }));
}

#[test]
fn nothing_is_offered_for_the_own_group_without_an_invite_or_when_switched_off() {
    let invite = Invite::create("Friends", None).unwrap();
    let text = invite::encode(&invite);
    let mut watcher = ClipboardInviteWatcher::new();

    let member = member_of(&invite, "me");
    assert!(watcher.check(&member, 1, || text.clone()).is_none(), "already in this group");
    assert!(watcher.check(&fresh(), 2, || "no invite here".to_string()).is_none());
    assert!(watcher.check(&fresh(), 3, String::new).is_none(), "an unreadable clipboard reads as empty");

    let off = GroupConfig { detect_clipboard_invites: false, ..fresh() };
    assert!(watcher.check(&off, 4, || panic!("read although switched off")).is_none());
    // switched on again: the change that was skipped is looked at now
    assert!(watcher.check(&fresh(), 4, || text.clone()).is_some());
}
