//! Bluetooth transport for pocket-veto, with cfg-gated platform backends.
//!
//! # Layout
//!
//! - [`bridge`] — the platform-agnostic [`BtTransport`] trait and [`BtBridge`]
//!   reconnect/heartbeat/replay loop. This is the only module the server links
//!   against; it depends solely on the trait, not on any platform backend.
//! - [`frame`] — the shared length-prefix framing helpers used by every
//!   backend so the wire format `[4-byte big-endian u32 length][JSON payload]`
//!   is defined in one place.
//! - `mock` — an in-memory [`BtTransport`] + `MockPeer` pair for tests and dev
//!   runs (no radio required). Compiled when `cfg(test)` or the `mock` cargo
//!   feature is enabled (referenced here as a plain backtick because the
//!   module is `#[cfg]`-gated and absent without that feature).
//! - `linux` — bluer-based RFCOMM backend. Only compiled on
//!   `target_os = "linux"` **and** with the `linux-bt` cargo feature enabled
//!   (the feature pulls in `bluer`, which needs `libdbus-1-dev` at build
//!   time).
//! - `windows` — serialport-based COM-port backend. Only compiled on
//!   `target_os = "windows"`.
//! - `macos` — intentional `compile_error!` stub. The non-BT parts of the
//!   binary still compile and run on macOS; only the Bluetooth backend is
//!   out of v1 scope.
//!
//! # Prelude
//!
//! `use pocket_veto_bt::prelude::*;` brings the [`BtTransport`]
//! trait into scope — everything needed to write code that is generic over a
//! Bluetooth transport.

pub mod bridge;
pub mod frame;

/// In-memory mock transport. Gated on `cfg(test)` (so the crate's own unit
/// tests see it) **or** the `mock` cargo feature (so downstream crates'
/// tests/dev builds can use it via a `[dev-dependencies]` entry with
/// `features = ["mock"]`). A plain `cargo build -p pocket-veto-bt` does not
/// compile this module.
#[cfg(any(test, feature = "mock"))]
pub mod mock;

// The Linux BlueZ backend requires the `linux-bt` feature (and libdbus-1-dev
// system headers). Without the feature, `bluer` is not pulled in and this
// module is not compiled, keeping the workspace buildable in bare environments.
#[cfg(all(target_os = "linux", feature = "linux-bt"))]
pub mod linux;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
pub mod macos;

/// Convenience re-exports for code that is generic over a Bluetooth transport.
///
/// `use pocket_veto_bt::prelude::*;` brings the [`bridge::BtTransport`] trait into
/// scope. The bridge struct itself ([`bridge::BtBridge`]) is intentionally
/// not re-exported here because callers usually want to name it explicitly
/// with its type parameter.
pub mod prelude {
    pub use crate::bridge::BtTransport;
}

pub use bridge::{BtBridge, BtTransport};
pub use frame::{
    build_frame, read_length_prefixed, read_length_prefixed_sync, write_length_prefixed,
    write_length_prefixed_sync,
};
