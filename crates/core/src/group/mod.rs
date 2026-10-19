//! Port of `shared/Group` (protocol v1, docs/design.md §8) and of the logic behind the old invite dialogs.

pub mod config;
pub mod device;
pub mod invite;
pub mod invite_flow;
pub mod keys;
pub mod messages;
pub mod shared_file;

/// Unix milliseconds, the `t` of every message.
pub fn now_ms() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => since.as_millis() as i64,
        Err(before) => -(before.duration().as_millis() as i64),
    }
}
