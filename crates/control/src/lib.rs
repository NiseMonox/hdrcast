//! Remote control: a viewer's mouse and keyboard, sent to the host over the punched path.
//!
//! The viewer's side never touches the OS: mpv already owns the window the stream plays in, so its JSON IPC
//! reports the pointer position and every key and button ([`keys`] writes the input section that makes it do
//! so). [`viewer::ViewerControl`] turns those into sealed datagrams, the P2P link carries them next to the SRT
//! packets as a side channel, and [`host::HostControl`] replays them through an [`inject::Injector`] once the
//! host's user has allowed it.
//!
//! Wire format (the side-channel payload, after the link's own header):
//!
//! ```text
//!  0  1   dir      1 viewer -> host, 2 host -> viewer
//!  1  8   counter  big-endian, strictly increasing per direction (the AEAD nonce and the replay guard)
//!  9  ..  ChaCha20-Poly1305(key = sub("ctl/" + sid), nonce = dir || 0 0 0 || counter, aad = "HDRCCTL1")
//! ```
//!
//! Viewer -> host plaintext: version, flags (bit 0 = wants control), pointer x/y (0..=65535 across the picture),
//! pointer-over-picture flag, then the oldest events the host has not acknowledged yet (first sequence number,
//! count, 8 bytes each). Host -> viewer plaintext: version, grant state, last event sequence applied.
//! Pointer positions are latest-wins; keys, buttons and wheel steps are resent until acknowledged, and the host
//! applies them once, in order.

pub mod host;
pub mod inject;
pub mod keys;
pub mod viewer;
pub mod wire;

pub use host::{Grant, HostControl};
pub use inject::{Injector, ScreenRect, SystemInjector};
pub use keys::{Button, Key, Mods, NamedKey};
pub use viewer::{ControlState, ViewerControl};
pub use wire::{Event, EventKind};
