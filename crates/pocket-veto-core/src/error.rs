//! Domain error types for pocket-veto.
//!
//! All fallible `pocket-veto-core` operations return [`Result<T, CoreError>`]
//! (see [`Result`]). The error enum is implemented with `thiserror` so that
//! each variant carries a typed payload and a human-readable `Display`
//! representation. Library code never `panic!`s on expected failures — it
//! propagates them via `?`.
//!
//! `CoreError` is fully structured: no variant carries a free-form `String`
//! message. Protocol failures are categorized by [`ProtocolError`] and
//! normalization failures by [`NormalizeError`], both typed sub-enums. This
//! lets callers match on a specific cause (e.g. EOF vs. oversized frame)
//! without parsing error text.

use std::io;

use thiserror::Error;

/// The unified error type for all `pocket-veto-core` operations.
///
/// Variants are grouped by subsystem. Each wraps enough typed context to
/// diagnose the failure without leaking internal structure to callers that
/// only need the category.
///
/// Note: only [`CoreError::Io`] carries an automatic `From<io::Error>` impl.
/// Config-layer I/O failures are surfaced as [`CoreError::ConfigIo`] with the
/// offending [`PathBuf`](std::path::PathBuf) and the underlying `io::Error`,
/// so the two `io::Error` sources remain distinguishable without a duplicate
/// `From` impl.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Config file could not be read or written (path / permission issue).
    /// Carries the offending path and the underlying `io::Error`.
    #[error("config io at {path}: {source}")]
    ConfigIo {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Config file could not be parsed as TOML or deserialized into `Config`.
    #[error("config parse: {0}")]
    ConfigParse(#[from] toml::de::Error),
    /// Config file could not be serialized back to TOML.
    #[error("config serialize: {0}")]
    ConfigSerialize(#[from] toml::ser::Error),
    /// `SQLite` / rusqlite failure (open, prepare, execute, query).
    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),
    /// Wire-protocol failure: oversized frame, incomplete frame, length
    /// overflow, or invalid JSON. See [`ProtocolError`] for the causes.
    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),
    /// Input normalization failure: unknown host casing, unrecognized event
    /// name, missing required field, or out-of-range value. See
    /// [`NormalizeError`] for the causes. `field` names the input field the
    /// normalizer was looking at.
    #[error("normalize at {field}: {kind}")]
    Normalize {
        kind: NormalizeError,
        field: &'static str,
    },
    /// Generic `std::io` error not attributable to the config file (e.g. a
    /// pipe read on the hook stdin path). This is the only variant with an
    /// automatic `From<io::Error>` impl.
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// Wire-protocol failure category.
///
/// Used by [`CoreError::Protocol`]. Splitting protocol failures into a
/// dedicated enum lets callers (e.g. the BT bridge) distinguish "need more
/// bytes" ([`ProtocolError::UnexpectedEof`]) from unrecoverable failures
/// without parsing error text.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// The encoded or declared frame exceeds
    /// [`MAX_FRAME_SIZE`](crate::protocol::MAX_FRAME_SIZE).
    #[error("frame too large: {size} bytes (max {max})")]
    OversizedFrame { size: usize, max: usize },
    /// The buffer does not yet contain a complete frame; the caller should
    /// keep reading. `expected` is the total number of bytes needed.
    #[error("incomplete frame: need {expected} byte(s)")]
    UnexpectedEof { expected: usize },
    /// A length-prefix arithmetic check overflowed `usize`.
    #[error("frame length overflow: {0}")]
    LengthOverflow(&'static str),
    /// JSON serialization/deserialization failure.
    #[error("json: {0}")]
    Invalid(#[from] serde_json::Error),
}

/// Input-normalization failure category.
///
/// Used by [`CoreError::Normalize`]. Each variant carries the dynamic part of
/// the failure; the `field` context lives on [`CoreError::Normalize`].
#[derive(Debug, Error)]
pub enum NormalizeError {
    /// `hook_event_name` was missing or not a string.
    #[error("missing or non-string `hook_event_name`")]
    MissingEventName,
    /// `hook_event_name` was present but empty.
    #[error("`hook_event_name` is empty")]
    EmptyEventName,
    /// The first character of `hook_event_name` was neither upper- nor
    /// lower-case, so the host could not be determined.
    #[error("cannot determine host from `hook_event_name` = {value:?}")]
    AmbiguousHost { value: String },
    /// A host value did not match a known casing.
    #[error("unknown host casing: {value}")]
    UnknownHost { value: String },
    /// An event name was not recognized for the detected host.
    #[error("unrecognized event name: {value}")]
    UnknownEvent { value: String },
    /// A required field was missing from the payload.
    #[error("missing required field")]
    MissingField,
    /// An enum value from the DB/wire did not match any known variant.
    #[error("unknown enum value: {value}")]
    UnknownEnum { value: String },
    /// Neither a home directory nor a config directory could be located.
    #[error("cannot locate a home or config directory for pocket-veto config")]
    NoConfigDir,
    /// The resolved config path has no parent directory.
    #[error("config path has no parent directory")]
    NoParentDir,
    /// The config file does not exist at the resolved path.
    #[error("config file not found at {path}; run `pocket-veto init`")]
    ConfigNotFound { path: String },
    /// A numeric value was outside its allowed range.
    #[error("value {value} out of range [{min}, {max}]")]
    OutOfRange { value: i64, min: i64, max: i64 },
}

impl CoreError {
    /// Returns `true` if this is a protocol EOF (caller should keep reading).
    ///
    /// Maps to [`ProtocolError::UnexpectedEof`]. The BT bridge relies on this
    /// to decide whether to keep buffering or treat the stream as desynced.
    #[must_use]
    pub fn is_protocol_eof(&self) -> bool {
        matches!(self, Self::Protocol(ProtocolError::UnexpectedEof { .. }))
    }
}

/// Convenience conversion so call sites can use `?` directly on a
/// `serde_json` result (e.g. `serde_json::to_vec(msg)?`) inside a function
/// returning `Result<_, CoreError>`. `serde_json::Error` is wrapped by
/// [`ProtocolError::Invalid`] and then by [`CoreError::Protocol`]. This
/// bridges the two-step `From` chain (`serde_json::Error` -> `ProtocolError`
/// -> `CoreError`) that `?` does not compose automatically.
impl From<serde_json::Error> for CoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Protocol(ProtocolError::Invalid(e))
    }
}

/// A type alias used throughout `pocket-veto-core` for fallible operations.
pub type Result<T> = std::result::Result<T, CoreError>;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unreachable,
    clippy::unwrap_in_result,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use std::io;
    use std::path::PathBuf;

    #[test]
    fn protocol_eof_marks_eof() {
        let err = CoreError::Protocol(ProtocolError::UnexpectedEof { expected: 4 });
        assert!(err.is_protocol_eof());
        assert!(err.to_string().contains("incomplete frame"));
    }

    #[test]
    fn protocol_non_eof_is_not_eof() {
        let err = CoreError::Protocol(ProtocolError::OversizedFrame {
            size: 1024,
            max: 1023,
        });
        assert!(!err.is_protocol_eof());
        assert!(err.to_string().contains("frame too large"));
    }

    #[test]
    fn normalize_error_displays_message() {
        let err = CoreError::Normalize {
            kind: NormalizeError::UnknownHost {
                value: "unknown".to_string(),
            },
            field: "host",
        };
        assert!(err.to_string().contains("unknown"));
    }

    #[test]
    fn config_io_carries_path_and_source() {
        let source = io::Error::new(io::ErrorKind::NotFound, "no such file");
        let err = CoreError::ConfigIo {
            path: PathBuf::from("/x/config.toml"),
            source,
        };
        let s = err.to_string();
        assert!(s.contains("config io"));
        assert!(s.contains("no such file"));
    }
}
