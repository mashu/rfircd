//! AIRC/1: the compact, plain-text application protocol spoken over the air.
//! See `docs/protocol.md` for the normative description.

pub mod frame;
pub mod session;

pub use frame::{encode_fields, flags, AircFrame, Kind};
pub use session::{Dialect, Peer, SessionConfig, Sessions};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AircError {
    #[error("frame too short to be AIRC")]
    Truncated,
    #[error("not an AIRC frame")]
    NotAirc,
    #[error("unknown AIRC message type 0x{0:02X}")]
    UnknownKind(u8),
    #[error("invalid fragment header")]
    BadFragment,
}
