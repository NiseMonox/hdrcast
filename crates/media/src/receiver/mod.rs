//! The viewer's side: what mpv is told, what it reports back over its JSON IPC, why it exited, and
//! `receiver.json`. A port of the C# `shared/Receiver`.

pub mod mpv_command;
pub mod mpv_ipc;
pub mod mpv_log;
pub mod settings;

pub use mpv_command::{MpvLaunchPlan, MpvPlanError};
pub use mpv_ipc::{MpvIpcMessage, MpvIpcMessageKind, MpvStatusModel, MpvVideoParams};
pub use mpv_log::MpvExitReason;
pub use settings::{DisplayMode, HwDecMode, LatencyPreset, ReceiverFrontend, ReceiverSettings};
