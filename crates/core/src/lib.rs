//! What every part of HdrCast shares: addresses and SRT URLs, the group / invite / signed-message formats of
//! protocol v1 (docs/design.md §8), child processes, tool lookup, paths, the Windows Firewall check.
//!
//! A port of the C# `shared/Common` and `shared/Group` (0.2.4). Wire formats and `group.json` are unchanged:
//! friends still run the C# apps, and `tests/vectors/group-v1.json` pins every byte.

pub mod common;
pub mod group;

pub use common::{
    app_paths, arg_quote, base64url, endpoint, firewall, json, line_process, process_job, secret_redactor,
    srt_buffer_sizing, srt_url, tool_locator,
};
pub use group::{config, device, invite, invite_flow, keys, messages, shared_file};

pub use common::endpoint::{EndpointError, EndpointKind, HostPort};
pub use common::firewall::{FirewallRule, FirewallVerdict};
pub use common::line_process::LineProcess;
pub use common::process_job::ProcessJob;
pub use common::srt_buffer_sizing::SrtBufferPlan;
pub use common::srt_url::{SrtRole, SrtTopology, SrtUrlOptions, SrtUrlParts};
pub use group::config::GroupConfig;
pub use group::invite::{Invite, InviteError, InviteProblem};
pub use group::keys::GroupKeys;
pub use group::device::{DeviceKey, Exchange, VerifiedDevice};
pub use group::messages::{DeviceProof, JoinAnswer, JoinRequest, LiveRecord, Payload, SignedEnvelope};
pub use group::shared_file::SharedGroupFile;
