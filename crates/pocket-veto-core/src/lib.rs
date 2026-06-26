//! Shared protocol, config, db, events, and normalization for pocket-veto.
//!
//! The [`pub mod`](self) declarations below expose each subsystem as a nested
//! path (e.g. `pocket_veto_core::protocol::Host`). The `pub use` facade that
//! follows re-exports the public API at the crate root as well, so downstream
//! crates can also import directly from `pocket_veto_core::` (e.g.
//! `pocket_veto_core::Host`). The facade is additive: both spellings work.

pub mod approvals;
pub mod config;
pub mod db;
pub mod error;
pub mod events;
pub mod normalize;
pub mod output;
pub mod protocol;

// ---------------------------------------------------------------------------
// Crate-root facade (additive re-exports of the public API)
// ---------------------------------------------------------------------------

pub use approvals::ApprovalWaiters;
pub use config::Config;
pub use db::Db;
pub use error::{CoreError, NormalizeError, ProtocolError, Result};
pub use events::{EventBus, EventMessage};
pub use normalize::{CanonicalEvent, InternalEvent};
pub use output::{HookOutput, fail_closed_stdout, to_stdout};
pub use protocol::{
    AgentId, AgentStatus, ApprovalId, ClientMessage, ComPort, Decision, EventKind, Host,
    MAX_FRAME_SIZE, RfcommChannel, ServerMessage, SessionId, SubscriptionFilter, TimestampMs,
    Token, decode_client_message, decode_frame, decode_message, encode_client_message,
    encode_frame, encode_message,
};
