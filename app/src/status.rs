//! The snapshot the page redraws from, a few times a second.

use serde_json::{json, Value};

use crate::state::{self, AppState};
use crate::{live, share, watch};

pub fn snapshot(app: &AppState) -> Value {
    // The sharing and watching parts first, without the settings lock: their callbacks (is the firewall open?
    // is remote control offered?) take that lock themselves.
    let (share, watch, live) = (share::status(app), watch::status(app), live::status(app));
    let inner = app.inner.lock().unwrap();
    let candidates = state::device_candidates(app, &inner.access);
    json!({
        "share": share,
        "plan": state::plan_view(&inner.sender, &inner.outputs),
        "watch": watch,
        "live": live,
        "group": state::group_view(inner.group.current()),
        "firewall": state::firewall_view(inner.firewall),
        // the page fetches the rules again when this changes behind its back
        "accessRev": inner.access_rev,
        "deviceCandidates": candidates,
    })
}
