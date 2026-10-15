//! HdrCast's peer-to-peer layer: a UDP path punched between a host and each viewer (IPv6 first, IPv4 with UPnP
//! and learned candidates as the extra), carrying the SRT datagrams of ffmpeg and mpv, which both only ever
//! talk to 127.0.0.1; the client of the signaling server that exchanges the signed metadata; and the host-side
//! fan-out of one encoder to one remux child per viewer (docs/design.md §1, §8).
//!
//! A port of the C# `shared/P2p` (0.2.4). Wire protocol v1 is unchanged: these peers and the C# ones punch,
//! join and stream with each other. New here is the side channel (docs/design.md §8.9): small datagrams of
//! another part of the app (remote control) next to the stream, only ever sent to a peer that announced it
//! understands them.
//!
//! Everything runs on plain threads and blocking sockets. Callbacks are called on those threads (network
//! threads, the timer, a session's own thread): keep them short and do not block in them.

pub mod cancel;
pub mod control_packet;
pub mod echo_protocol;
pub mod host_link;
mod http;
mod link_base;
pub mod local_addresses;
pub mod p2p_diagnostics;
pub mod p2p_host;
pub mod peer_session;
pub mod remux_child;
mod router_http;
pub mod side_channel;
pub mod signaling_client;
pub mod switchable_stream;
pub mod ts_fan_out;
pub mod udp_channel;
pub mod upnp;
mod util;
pub mod viewer_link;
pub mod viewer_session;
mod xml;

pub use cancel::CancelToken;
pub use control_packet::{ControlType, PeerRole};
pub use host_link::{HostLink, HostSessionInfo, HostSideHandler};
pub use link_base::{LinkAddresses, SOURCE_ECHO, SOURCE_ROUTER};
pub use local_addresses::AddressCandidate;
pub use p2p_host::{P2pHost, P2pHostOptions, ViewerInfo, ViewerState};
pub use peer_session::{LinkState, Outgoing, PeerSession};
pub use remux_child::{RemuxChild, RemuxInput};
pub use signaling_client::{LiveEntry, SignalingClient, SignalingError};
pub use switchable_stream::SwitchableStream;
pub use ts_fan_out::TsFanOut;
pub use udp_channel::{mono_clock, ChannelIo, PacketFilter, UdpChannel};
pub use upnp::{PortMappingOptions, PortMappingState, RouterPortMapping, UpnpError, UpnpGateway};
pub use util::LogFn;
pub use viewer_link::{ViewerLink, ViewerSideHandler};
pub use viewer_session::{ViewerSession, ViewerSessionOptions, ViewerSessionState};

/// Why a [`P2pHost`] or a [`ViewerSession`] could not start. The text goes into the app's log after
/// "cannot start the group session: ".
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("no group is configured")]
    NotConfigured,
    #[error("already started")]
    AlreadyStarted,
    /// A socket could not be opened or a thread could not be started.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl From<hdrcast_core::config::NotConfiguredError> for StartError {
    fn from(_: hdrcast_core::config::NotConfiguredError) -> Self {
        StartError::NotConfigured
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The app keeps these in shared state and calls them from its UI thread while the network threads run.
    #[test]
    fn the_public_objects_can_be_shared_between_threads() {
        fn shared<T: Send + Sync>() {}
        shared::<P2pHost>();
        shared::<ViewerSession>();
        shared::<HostLink>();
        shared::<ViewerLink>();
        shared::<SignalingClient>();
        shared::<RouterPortMapping>();
        shared::<SwitchableStream>();
        shared::<TsFanOut>();
        shared::<RemuxChild>();
        shared::<PeerSession>();
        shared::<UdpChannel>();
        shared::<CancelToken>();
        shared::<P2pHostOptions>();
        shared::<ViewerSessionOptions>();
        shared::<LinkAddresses>();
        shared::<SignalingError>();
        shared::<StartError>();
    }
}
