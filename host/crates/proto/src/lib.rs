//! Wire protocol between the traffic-police capture runtime ("device") and the host.
//!
//! `docs/PROTOCOL.md` is the source of truth; this crate implements its framing ([`frame`]) and
//! its JSON messages ([`msg`]).

pub mod frame;
pub mod msg;

pub use frame::{BodyChunk, BodyDir, Decoder, Frame, FrameError};
pub use msg::{DeviceMsg, Headers, HostMsg};

/// The protocol version this build speaks (PROTOCOL.md §1).
pub const PROTOCOL_VERSION: u32 = 1;
