//! The viewer's side: what mpv is told, what it reports back over its JSON IPC, why it exited, and
//! `receiver.json` (a port of the C# `shared/Receiver`); and the guard that keeps damaged pictures off the screen.

pub mod mpv_command;
pub mod mpv_ipc;
pub mod mpv_log;
pub mod settings;
pub mod ts_guard;

pub use mpv_command::{MpvLaunchPlan, MpvPlanError};
pub use mpv_ipc::{MpvIpcMessage, MpvIpcMessageKind, MpvStatusModel, MpvVideoParams};
pub use mpv_log::MpvExitReason;
pub use settings::{DisplayMode, HwDecMode, LatencyPreset, ReceiverFrontend, ReceiverSettings};
pub use ts_guard::{GuardEvent, GuardStats, TsGuard};
