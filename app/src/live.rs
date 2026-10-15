//! Who in the group is live right now: asked from the signaling server every 10 s while the app is open.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use hdrcast_p2p::{p2p_diagnostics, CancelToken, LiveEntry, SignalingClient};
use serde_json::{json, Value};

use crate::state::AppState;

const POLL_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Shown {
    hosts: Vec<LiveEntry>,
    error: Option<String>,
    loaded: bool,
}

#[derive(Default)]
pub struct Live {
    shown: Mutex<Shown>,
    /// Cancels the poller that is running; a new group or a manual refresh starts a fresh one.
    poller: Mutex<Option<CancelToken>>,
}

/// (Re)starts polling for the group that is configured now. Without a group nothing is polled.
pub fn restart(app: &Arc<AppState>) {
    let token = CancelToken::new();
    if let Some(old) = app.live.poller.lock().unwrap().replace(token.clone()) {
        old.cancel();
    }
    let group = app.group();
    if !group.is_configured() {
        *app.live.shown.lock().unwrap() = Shown::default();
        return;
    }
    let Ok(client) = SignalingClient::for_group(&group) else { return };
    let me = group.peer_id.clone();
    let app = Arc::downgrade(app);
    thread::Builder::new()
        .name("live-poll".into())
        .spawn(move || {
            while !token.is_cancelled() {
                let result = client.get_live(&token);
                let Some(app) = app.upgrade() else { return };
                if token.is_cancelled() {
                    return;
                }
                {
                    let mut shown = app.live.shown.lock().unwrap();
                    match result {
                        Ok(mut hosts) => {
                            hosts.retain(|h| h.record.host_id != me); // not ourselves
                            shown.hosts = hosts;
                            shown.error = None;
                            shown.loaded = true;
                        }
                        Err(e) if e.is_cancelled() => return,
                        // 404: nobody has gone live in this group yet, so the server does not know it
                        Err(e) if e.status() == 404 => {
                            shown.hosts.clear();
                            shown.error = None;
                            shown.loaded = true;
                        }
                        Err(e) => {
                            shown.error =
                                Some(if e.is_unreachable() { p2p_diagnostics::SIGNALING_DOWN.to_string() } else { e.to_string() });
                            shown.loaded = true;
                        }
                    }
                }
                drop(app);
                token.wait(POLL_INTERVAL);
            }
        })
        .ok();
}

/// The group changed: forget the old group's list and poll the new one.
pub fn group_changed(app: &Arc<AppState>) {
    *app.live.shown.lock().unwrap() = Shown::default();
    restart(app);
}

pub fn name_of(app: &AppState, host_id: &str) -> Option<String> {
    app.live.shown.lock().unwrap().hosts.iter().find(|h| h.record.host_id == host_id).map(|h| h.record.name.clone())
}

pub fn status(app: &AppState) -> Value {
    let shown = app.live.shown.lock().unwrap();
    let hosts: Vec<Value> = shown
        .hosts
        .iter()
        .map(|h| {
            let r = &h.record;
            json!({
                "hostId": r.host_id,
                "name": r.name,
                "sinceMs": r.since_ms,
                "mbps": r.mbps,
                "viewers": r.viewers,
                "maxViewers": r.max_viewers,
                "codec": r.codec,
                "ipv6": p2p_diagnostics::has_ipv6(&r.candidates),
                "openIpv4": r.open_ipv4,
                "control": h.remote_control(),
            })
        })
        .collect();
    json!({ "hosts": hosts, "error": shown.error, "loaded": shown.loaded })
}
