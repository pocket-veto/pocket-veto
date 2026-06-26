//! Wire protocol between the PC server and the Android phone (and any other
//! transport that speaks the same frames).
//!
//! # Frame format
//!
//! Every message is encoded as a length-prefixed JSON blob:
//!
//! ```text
//! [4 bytes: big-endian u32 length N][N bytes: UTF-8 JSON]
//! ```
//!
//! Frames are capped at [`MAX_FRAME_SIZE`] (1 MiB). The codec is symmetric:
//! [`encode_frame`] / [`decode_frame`] operate on arbitrary [`Serialize`]
//! values, while [`encode_message`] / [`decode_message`] and their
//! `ClientMessage` counterparts are typed wrappers used by the bridge.
//!
//! # Timestamps
//!
//! All timestamps are `i64` unix milliseconds. `i64` (not `u64`) is used so
//! that the wire type matches the `SQLite` `INTEGER` column used by
//! [`crate::db`], and so that sentinel values like `-1` can represent
//! "unknown" without an extra `Option` wrapper on the wire.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::error::{CoreError, NormalizeError, ProtocolError, Result};

/// Maximum frame size (length prefix + payload): 1 MiB, generous for any
/// single agent event payload.
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// Length of the big-endian u32 length prefix, in bytes.
const LENGTH_PREFIX_BYTES: usize = 4;

// ---------------------------------------------------------------------------
// Host / event-kind / status / decision enums
// ---------------------------------------------------------------------------

/// Which agent host produced an event. Serialized as lowercase `cursor` /
/// `claude` on the wire so the Android side can switch on a stable string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Host {
    Cursor,
    Claude,
}

/// Coarse classification of an agent event, mirrored on the Android side.
/// Wire tags are `snake_case`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    ToolCall,
    Thought,
    Response,
    FileEdit,
    Shell,
    ApprovalRequest,
    ApprovalDecision,
}

/// Lifecycle status of an agent, stored in the `agents.status` column and
/// emitted in [`ServerMessage::AgentEnd`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Running,
    AwaitingApproval,
    Completed,
    Aborted,
    Error,
}

/// A decision on an approval request. The wire set includes `Defer` (the
/// phone may defer to let another approval fire first); the hook output path
/// only ever emits `Allow`/`Deny`/`Ask`, but reuses this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
    Ask,
    Defer,
}

// ---------------------------------------------------------------------------
// Enum DB/wire string + SQL conversions
// ---------------------------------------------------------------------------

// Binds an enum to its stable DB/wire string and the inverse parse, plus the
// `rusqlite` `ToSql`/`FromSql` glue. Each enum below gets:
// - `to_db_str(self) -> &'static str` (matches the serde rename)
// - `from_db_str(&str) -> Result<Self, CoreError>` (unknown -> Normalize)
// - `Display` / `FromStr` delegating to the above
// - `ToSql` (TEXT) / `FromSql` (TEXT -> from_db_str)

impl Host {
    #[must_use]
    pub fn to_db_str(self) -> &'static str {
        match self {
            Host::Cursor => "cursor",
            Host::Claude => "claude",
        }
    }

    /// Parse a DB/wire string back into a [`Host`].
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Normalize`] with [`NormalizeError::UnknownEnum`]
    /// when `s` is not a known host tag.
    pub fn from_db_str(s: &str) -> Result<Self> {
        match s {
            "cursor" => Ok(Host::Cursor),
            "claude" => Ok(Host::Claude),
            other => Err(CoreError::Normalize {
                kind: NormalizeError::UnknownEnum {
                    value: other.to_string(),
                },
                field: "host",
            }),
        }
    }
}

impl std::fmt::Display for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.to_db_str())
    }
}

impl std::str::FromStr for Host {
    type Err = CoreError;
    fn from_str(s: &str) -> Result<Self> {
        Self::from_db_str(s)
    }
}

impl ToSql for Host {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Text(
            self.to_db_str().as_bytes(),
        )))
    }
}

impl FromSql for Host {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value {
            ValueRef::Text(bytes) => {
                let s = std::str::from_utf8(bytes)?;
                Self::from_db_str(s).map_err(FromSqlError::other)
            }
            ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) | ValueRef::Blob(_) => {
                Err(FromSqlError::InvalidType)
            }
        }
    }
}

impl EventKind {
    #[must_use]
    pub fn to_db_str(self) -> &'static str {
        match self {
            EventKind::ToolCall => "tool_call",
            EventKind::Thought => "thought",
            EventKind::Response => "response",
            EventKind::FileEdit => "file_edit",
            EventKind::Shell => "shell",
            EventKind::ApprovalRequest => "approval_request",
            EventKind::ApprovalDecision => "approval_decision",
        }
    }

    /// Parse a DB/wire string back into an [`EventKind`].
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Normalize`] with [`NormalizeError::UnknownEnum`]
    /// when `s` is not a known event-kind tag.
    pub fn from_db_str(s: &str) -> Result<Self> {
        match s {
            "tool_call" => Ok(EventKind::ToolCall),
            "thought" => Ok(EventKind::Thought),
            "response" => Ok(EventKind::Response),
            "file_edit" => Ok(EventKind::FileEdit),
            "shell" => Ok(EventKind::Shell),
            "approval_request" => Ok(EventKind::ApprovalRequest),
            "approval_decision" => Ok(EventKind::ApprovalDecision),
            other => Err(CoreError::Normalize {
                kind: NormalizeError::UnknownEnum {
                    value: other.to_string(),
                },
                field: "event_kind",
            }),
        }
    }
}

impl std::fmt::Display for EventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.to_db_str())
    }
}

impl std::str::FromStr for EventKind {
    type Err = CoreError;
    fn from_str(s: &str) -> Result<Self> {
        Self::from_db_str(s)
    }
}

impl ToSql for EventKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Text(
            self.to_db_str().as_bytes(),
        )))
    }
}

impl FromSql for EventKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value {
            ValueRef::Text(bytes) => {
                let s = std::str::from_utf8(bytes)?;
                Self::from_db_str(s).map_err(FromSqlError::other)
            }
            ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) | ValueRef::Blob(_) => {
                Err(FromSqlError::InvalidType)
            }
        }
    }
}

impl AgentStatus {
    #[must_use]
    pub fn to_db_str(self) -> &'static str {
        match self {
            AgentStatus::Running => "running",
            AgentStatus::AwaitingApproval => "awaiting_approval",
            AgentStatus::Completed => "completed",
            AgentStatus::Aborted => "aborted",
            AgentStatus::Error => "error",
        }
    }

    /// Parse a DB/wire string back into an [`AgentStatus`].
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Normalize`] with [`NormalizeError::UnknownEnum`]
    /// when `s` is not a known status tag.
    pub fn from_db_str(s: &str) -> Result<Self> {
        match s {
            "running" => Ok(AgentStatus::Running),
            "awaiting_approval" => Ok(AgentStatus::AwaitingApproval),
            "completed" => Ok(AgentStatus::Completed),
            "aborted" => Ok(AgentStatus::Aborted),
            "error" => Ok(AgentStatus::Error),
            other => Err(CoreError::Normalize {
                kind: NormalizeError::UnknownEnum {
                    value: other.to_string(),
                },
                field: "agent_status",
            }),
        }
    }
}

impl std::fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.to_db_str())
    }
}

impl std::str::FromStr for AgentStatus {
    type Err = CoreError;
    fn from_str(s: &str) -> Result<Self> {
        Self::from_db_str(s)
    }
}

impl ToSql for AgentStatus {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Text(
            self.to_db_str().as_bytes(),
        )))
    }
}

impl FromSql for AgentStatus {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value {
            ValueRef::Text(bytes) => {
                let s = std::str::from_utf8(bytes)?;
                Self::from_db_str(s).map_err(FromSqlError::other)
            }
            ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) | ValueRef::Blob(_) => {
                Err(FromSqlError::InvalidType)
            }
        }
    }
}

impl Decision {
    #[must_use]
    pub fn to_db_str(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
            Decision::Ask => "ask",
            Decision::Defer => "defer",
        }
    }

    /// Map a decision to its past-tense DB `approvals.status` string
    /// (`allowed` / `denied` / `ask` / `deferred`) — the form persisted in the
    /// `approvals.status` column. Distinct from [`Decision::to_db_str`], which
    /// yields the bare wire form (`allow`/`deny`/`ask`/`defer`). Centralized
    /// here so the bridge and the HTTP server share one mapping.
    #[must_use]
    pub fn to_approval_status(self) -> &'static str {
        match self {
            Decision::Allow => "allowed",
            Decision::Deny => "denied",
            Decision::Ask => "ask",
            Decision::Defer => "deferred",
        }
    }

    /// Parse a DB/wire string back into a [`Decision`].
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Normalize`] with [`NormalizeError::UnknownEnum`]
    /// when `s` is not a known decision tag.
    pub fn from_db_str(s: &str) -> Result<Self> {
        match s {
            "allow" => Ok(Decision::Allow),
            "deny" => Ok(Decision::Deny),
            "ask" => Ok(Decision::Ask),
            "defer" => Ok(Decision::Defer),
            other => Err(CoreError::Normalize {
                kind: NormalizeError::UnknownEnum {
                    value: other.to_string(),
                },
                field: "decision",
            }),
        }
    }
}

impl std::fmt::Display for Decision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.to_db_str())
    }
}

impl std::str::FromStr for Decision {
    type Err = CoreError;
    fn from_str(s: &str) -> Result<Self> {
        Self::from_db_str(s)
    }
}

impl ToSql for Decision {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Text(
            self.to_db_str().as_bytes(),
        )))
    }
}

impl FromSql for Decision {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value {
            ValueRef::Text(bytes) => {
                let s = std::str::from_utf8(bytes)?;
                Self::from_db_str(s).map_err(FromSqlError::other)
            }
            ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) | ValueRef::Blob(_) => {
                Err(FromSqlError::InvalidType)
            }
        }
    }
}

/// Optional subscription filter sent by the phone in `Subscribe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionFilter {
    /// If `Some`, only events for the listed agent ids are forwarded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_ids: Option<Vec<String>>,
    /// If `Some`, only events of the listed kinds are forwarded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<EventKind>>,
}

// ---------------------------------------------------------------------------
// Message enums (PC -> Phone, Phone -> PC)
// ---------------------------------------------------------------------------

/// Messages flowing PC -> Phone. The serde tag is `type` and variant names
/// are `snake_case`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// A new agent session has started.
    AgentStart {
        agent_id: String,
        session_id: String,
        host: Host,
        name: String,
        workspace: String,
        started_at: i64,
    },
    /// A non-blocking progress event from an agent.
    AgentEvent {
        agent_id: String,
        kind: EventKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
        payload: serde_json::Value,
        ts: i64,
    },
    /// A blocking approval request the phone must answer before the agent
    /// may proceed.
    ApprovalRequest {
        approval_id: String,
        agent_id: String,
        tool: String,
        summary: String,
        detail: String,
        timeout_at: i64,
    },
    /// An agent session has ended.
    AgentEnd {
        agent_id: String,
        ended_at: i64,
        status: AgentStatus,
    },
    /// Server heartbeat; the phone replies with `HeartbeatAck`.
    Heartbeat { ts: i64 },
}

/// Messages flowing Phone -> PC. Same serde scheme as [`ServerMessage`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Subscribe to the live event stream, optionally filtered.
    Subscribe {
        #[serde(skip_serializing_if = "Option::is_none")]
        filter: Option<SubscriptionFilter>,
    },
    /// The phone's decision on a pending approval.
    ApprovalDecision {
        approval_id: String,
        decision: Decision,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// Acknowledgement of a server [`ServerMessage::Heartbeat`].
    HeartbeatAck { ts: i64 },
}

// ---------------------------------------------------------------------------
// Domain newtypes (wire-transparent)
// ---------------------------------------------------------------------------

/// Generates a wire-transparent `String`-backed newtype with the standard
/// conversions: `Display`, `AsRef<str>`, `From<String>`, `From<Newtype> for
/// String`, infallible `FromStr`, and `rusqlite` `ToSql`/`FromSql` delegating
/// to `String`. Deduped via a private macro rather than copy-pasting the same
/// ~50 lines five times — within-crate dedup via shared macro.
macro_rules! string_newtype {
    (
        $(#[doc = $doc:literal])*
        $vis:vis struct $name:ident(pub String);
    ) => {
        $(#[doc = $doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        $vis struct $name(pub String);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.0
            }
        }

        impl std::str::FromStr for $name {
            type Err = std::convert::Infallible;
            fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
                Ok(Self(s.to_string()))
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                self.0.to_sql()
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                String::column_result(value).map(Self)
            }
        }
    };
}

string_newtype! {
    /// Wire-transparent newtype identifying an agent session.
    pub struct AgentId(pub String);
}

string_newtype! {
    /// Wire-transparent newtype identifying an approval request.
    pub struct ApprovalId(pub String);
}

string_newtype! {
    /// Wire-transparent newtype identifying a session.
    pub struct SessionId(pub String);
}

string_newtype! {
    /// Wire-transparent newtype for a Windows SPP virtual COM port (e.g.
    /// `"COM3"`).
    pub struct ComPort(pub String);
}

string_newtype! {
    /// Wire-transparent newtype for a bearer token. `Display` writes the inner
    /// string verbatim; redact it before logging. Use [`Token::masked`] for a
    /// safe-to-log form.
    pub struct Token(pub String);
}

impl Token {
    /// Return a safe-to-log masked form: `****` followed by the last four
    /// characters. Uses `char`-based slicing (never byte slicing) so it is
    /// panic-free on any input, including empty or multi-byte strings.
    #[must_use]
    pub fn masked(&self) -> String {
        let chars: Vec<char> = self.0.chars().collect();
        let start = chars.len().saturating_sub(4);
        let tail: String = chars[start..].iter().collect();
        format!("****{tail}")
    }
}

/// RFCOMM channel number. Valid range is `1..=30` (Bluetooth RFCOMM).
/// Serialized as a plain JSON number (the `pub u8` field serializes
/// directly; no `#[serde(transparent)]` is needed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RfcommChannel(pub u8);

impl std::fmt::Display for RfcommChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl RfcommChannel {
    /// Construct a channel, validating the `1..=30` range.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Normalize`] with [`NormalizeError::OutOfRange`]
    /// when `channel` is outside `1..=30`.
    pub fn new(channel: u8) -> Result<Self> {
        Self::try_from(channel)
    }
}

impl std::convert::TryFrom<u8> for RfcommChannel {
    type Error = CoreError;
    fn try_from(channel: u8) -> Result<Self> {
        if (1..=30).contains(&channel) {
            Ok(Self(channel))
        } else {
            Err(CoreError::Normalize {
                kind: NormalizeError::OutOfRange {
                    value: i64::from(channel),
                    min: 1,
                    max: 30,
                },
                field: "rfcomm_channel",
            })
        }
    }
}

impl ToSql for RfcommChannel {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

impl FromSql for RfcommChannel {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        u8::column_result(value).map(Self)
    }
}

/// Unix-epoch milliseconds. Serialized as a JSON number; stored as a
/// `SQLite` `INTEGER` (so the wire type matches the column type).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TimestampMs(pub i64);

impl std::fmt::Display for TimestampMs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<i64> for TimestampMs {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

impl TimestampMs {
    /// Current unix-epoch milliseconds.
    ///
    /// Falls back to `0` only if the system clock is before `UNIX_EPOCH`
    /// (effectively never on a functioning host) or the elapsed millis
    /// overflow `i64` (geologically impossible). `unwrap_or`/`map_or` (not
    /// `unwrap`) keep this panic-free.
    #[must_use]
    pub fn now() -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0));
        Self(ms)
    }
}

impl ToSql for TimestampMs {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

impl FromSql for TimestampMs {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        i64::column_result(value).map(Self)
    }
}

// ---------------------------------------------------------------------------
// Frame codec
// ---------------------------------------------------------------------------

/// Serialize `msg` to JSON and prefix a 4-byte big-endian length.
///
/// # Errors
///
/// Returns [`CoreError::Protocol`] if the JSON serialization fails, if the
/// encoded frame (prefix + payload) would exceed [`MAX_FRAME_SIZE`], or if
/// the payload length does not fit in a `u32`.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(msg)?;

    let total = LENGTH_PREFIX_BYTES
        .checked_add(payload.len())
        .ok_or_else(|| {
            CoreError::Protocol(ProtocolError::LengthOverflow(
                "prefix + payload overflows usize",
            ))
        })?;

    if total > MAX_FRAME_SIZE {
        return Err(CoreError::Protocol(ProtocolError::OversizedFrame {
            size: total,
            max: MAX_FRAME_SIZE,
        }));
    }

    let mut out = Vec::with_capacity(total);
    let len = u32::try_from(payload.len()).map_err(|_e| {
        CoreError::Protocol(ProtocolError::LengthOverflow("payload length exceeds u32"))
    })?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Decode a single length-prefixed frame from `buf`.
///
/// Returns `(bytes_consumed, parsed_json_value)`. If `buf` does not yet
/// contain a full frame (missing length prefix or missing payload bytes),
/// returns [`ProtocolError::UnexpectedEof`] (via [`CoreError::Protocol`]) so
/// the caller can keep reading. Validates the declared length against
/// [`MAX_FRAME_SIZE`].
///
/// # Errors
///
/// - [`ProtocolError::UnexpectedEof`] when `buf` does not yet contain a full
///   frame (caller should keep reading).
/// - [`CoreError::Protocol`] if the declared length exceeds
///   [`MAX_FRAME_SIZE`] or the payload is not valid JSON.
pub fn decode_frame(buf: &[u8]) -> Result<(usize, serde_json::Value)> {
    if buf.len() < LENGTH_PREFIX_BYTES {
        return Err(CoreError::Protocol(ProtocolError::UnexpectedEof {
            expected: LENGTH_PREFIX_BYTES,
        }));
    }
    let mut len_bytes = [0u8; LENGTH_PREFIX_BYTES];
    len_bytes.copy_from_slice(&buf[..LENGTH_PREFIX_BYTES]);
    // justification: u32→usize is lossless on the 64-bit targets this crate
    // ships on (Linux/macOS/Windows x86_64+arm64); `declared` is immediately
    // bounds-checked via checked_add against the buffer below.
    #[allow(clippy::as_conversions)]
    let declared = u32::from_be_bytes(len_bytes) as usize;

    let total = LENGTH_PREFIX_BYTES.checked_add(declared).ok_or_else(|| {
        CoreError::Protocol(ProtocolError::LengthOverflow(
            "prefix + declared length overflows usize",
        ))
    })?;

    if total > MAX_FRAME_SIZE {
        return Err(CoreError::Protocol(ProtocolError::OversizedFrame {
            size: total,
            max: MAX_FRAME_SIZE,
        }));
    }
    if buf.len() < total {
        // Not enough bytes yet; caller should keep reading.
        return Err(CoreError::Protocol(ProtocolError::UnexpectedEof {
            expected: total,
        }));
    }

    let payload = &buf[LENGTH_PREFIX_BYTES..total];
    let value: serde_json::Value = serde_json::from_slice(payload)?;
    Ok((total, value))
}

/// Typed wrapper: encode a [`ServerMessage`] to a frame.
///
/// # Errors
///
/// Propagates any error from [`encode_frame`].
pub fn encode_message(msg: &ServerMessage) -> Result<Vec<u8>> {
    encode_frame(msg)
}

/// Typed wrapper: decode a [`ServerMessage`] from a frame buffer.
/// Returns `(bytes_consumed, message)`.
///
/// # Errors
///
/// Propagates any error from [`decode_frame`], plus [`CoreError::Protocol`]
/// if the parsed JSON does not deserialize into [`ServerMessage`].
pub fn decode_message(buf: &[u8]) -> Result<(usize, ServerMessage)> {
    decode_typed::<ServerMessage>(buf)
}

/// Typed wrapper: encode a [`ClientMessage`] to a frame.
///
/// # Errors
///
/// Propagates any error from [`encode_frame`].
pub fn encode_client_message(msg: &ClientMessage) -> Result<Vec<u8>> {
    encode_frame(msg)
}

/// Typed wrapper: decode a [`ClientMessage`] from a frame buffer.
/// Returns `(bytes_consumed, message)`.
///
/// # Errors
///
/// Propagates any error from [`decode_frame`], plus [`CoreError::Protocol`]
/// if the parsed JSON does not deserialize into [`ClientMessage`].
pub fn decode_client_message(buf: &[u8]) -> Result<(usize, ClientMessage)> {
    decode_typed::<ClientMessage>(buf)
}

fn decode_typed<T: DeserializeOwned>(buf: &[u8]) -> Result<(usize, T)> {
    let (consumed, value) = decode_frame(buf)?;
    let typed = serde_json::from_value(value)?;
    Ok((consumed, typed))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unreachable,
    clippy::unwrap_in_result,
    clippy::indexing_slicing,
    clippy::as_conversions
)]
mod tests {
    use super::*;

    fn sample_server_message() -> ServerMessage {
        ServerMessage::AgentStart {
            agent_id: "a1".to_string(),
            session_id: "s1".to_string(),
            host: Host::Claude,
            name: "refactor".to_string(),
            workspace: "/tmp/w".to_string(),
            started_at: 1_700_000_000_000,
        }
    }

    #[test]
    fn encode_then_decode_roundtrip_server_message() {
        let msg = sample_server_message();
        let frame = encode_message(&msg).expect("encode");
        let (consumed, decoded) = decode_message(&frame).expect("decode");
        assert_eq!(consumed, frame.len());
        assert_eq!(decoded, msg);
    }

    #[test]
    fn encode_then_decode_roundtrip_client_message() {
        let msg = ClientMessage::ApprovalDecision {
            approval_id: "ap1".to_string(),
            decision: Decision::Allow,
            note: Some("looks fine".to_string()),
        };
        let frame = encode_client_message(&msg).expect("encode");
        let (consumed, decoded) = decode_client_message(&frame).expect("decode");
        assert_eq!(consumed, frame.len());
        assert_eq!(decoded, msg);
    }

    #[test]
    fn decode_eof_when_buffer_short() {
        let err = decode_frame(&[0u8, 0, 1]).expect_err("should be eof");
        assert!(err.is_protocol_eof());
    }

    #[test]
    fn decode_eof_when_payload_incomplete() {
        let msg = sample_server_message();
        let frame = encode_message(&msg).expect("encode");
        // Truncate by one byte: the length prefix declares more than the buffer holds.
        let truncated = &frame[..frame.len() - 1];
        let err = decode_frame(truncated).expect_err("should be eof");
        assert!(err.is_protocol_eof());
    }

    #[test]
    fn decode_oversized_declared_length_errors_not_eof() {
        // Declare a 2 MiB payload, supply only a few bytes.
        let mut buf = vec![0u8; 4];
        let big_len: u32 = 2 * 1024 * 1024;
        buf[..4].copy_from_slice(&big_len.to_be_bytes());
        buf.extend_from_slice(b"{}");
        let err = decode_frame(&buf).expect_err("should error");
        assert!(!err.is_protocol_eof());
        assert!(err.to_string().contains("frame too large"));
    }

    #[test]
    fn encode_rejects_oversized_payload() {
        // Build a value whose JSON exceeds MAX_FRAME_SIZE.
        let huge = "x".repeat(MAX_FRAME_SIZE);
        let value = serde_json::Value::String(huge);
        let err = encode_frame(&value).expect_err("should reject");
        assert!(err.to_string().contains("frame too large"));
    }

    #[test]
    fn host_serializes_lowercase() {
        let json = serde_json::to_string(&Host::Cursor).expect("ser");
        assert_eq!(json, "\"cursor\"");
        let json = serde_json::to_string(&Host::Claude).expect("ser");
        assert_eq!(json, "\"claude\"");
    }

    #[test]
    fn decision_serializes_snake_case() {
        let json = serde_json::to_string(&Decision::Defer).expect("ser");
        assert_eq!(json, "\"defer\"");
    }

    #[test]
    fn server_message_tag_is_snake_case() {
        let msg = ServerMessage::Heartbeat { ts: 42 };
        let json = serde_json::to_string(&msg).expect("ser");
        assert!(json.contains("\"type\":\"heartbeat\""));
    }

    #[test]
    fn length_prefix_is_big_endian() {
        let msg = ServerMessage::Heartbeat { ts: 1 };
        let frame = encode_message(&msg).expect("encode");
        let len = u32::from_be_bytes(frame[..4].try_into().expect("prefix"));
        assert_eq!(len as usize, frame.len() - 4);
    }

    // --- Enum DB/wire string + FromStr + SQL round-trips ---

    #[test]
    fn enum_db_str_roundtrips() {
        for h in [Host::Cursor, Host::Claude] {
            assert_eq!(Host::from_db_str(h.to_db_str()).expect("host"), h);
            assert_eq!(h.to_string(), h.to_db_str());
        }
        for k in [
            EventKind::ToolCall,
            EventKind::Thought,
            EventKind::Response,
            EventKind::FileEdit,
            EventKind::Shell,
            EventKind::ApprovalRequest,
            EventKind::ApprovalDecision,
        ] {
            assert_eq!(EventKind::from_db_str(k.to_db_str()).expect("kind"), k);
            assert_eq!(k.to_string(), k.to_db_str());
        }
        for st in [
            AgentStatus::Running,
            AgentStatus::AwaitingApproval,
            AgentStatus::Completed,
            AgentStatus::Aborted,
            AgentStatus::Error,
        ] {
            assert_eq!(
                AgentStatus::from_db_str(st.to_db_str()).expect("status"),
                st
            );
            assert_eq!(st.to_string(), st.to_db_str());
        }
        for d in [
            Decision::Allow,
            Decision::Deny,
            Decision::Ask,
            Decision::Defer,
        ] {
            assert_eq!(Decision::from_db_str(d.to_db_str()).expect("decision"), d);
            assert_eq!(d.to_string(), d.to_db_str());
        }
    }

    #[test]
    fn from_db_str_rejects_unknown() {
        assert!(Host::from_db_str("nonsense").is_err());
        assert!(EventKind::from_db_str("nope").is_err());
        assert!(AgentStatus::from_db_str("nope").is_err());
        assert!(Decision::from_db_str("nope").is_err());
    }

    #[test]
    fn host_from_str_roundtrip() {
        use std::str::FromStr;
        assert_eq!(Host::from_str("cursor").expect("parse"), Host::Cursor);
        assert_eq!(Host::from_str("claude").expect("parse"), Host::Claude);
        assert!(Host::from_str("bad").is_err());
        // Display matches to_db_str.
        assert_eq!(Host::Claude.to_string(), "claude");
    }

    #[test]
    fn host_to_sql_from_sql_roundtrip() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute("CREATE TABLE h (v TEXT NOT NULL)", [])
            .expect("create");
        conn.execute(
            "INSERT INTO h (v) VALUES (?1)",
            rusqlite::params![Host::Claude],
        )
        .expect("insert");
        let back: Host = conn
            .query_row("SELECT v FROM h", [], |r| r.get(0))
            .expect("query");
        assert_eq!(back, Host::Claude);
    }

    // --- Newtype conversions ---

    #[test]
    fn agent_id_newtype_conversions() {
        let id = AgentId::from("agent-42".to_string());
        assert_eq!(id.to_string(), "agent-42");
        assert_eq!(id.as_ref(), "agent-42");
        // FromStr is infallible.
        let parsed: AgentId = "agent-9".parse().expect("infallible");
        assert_eq!(parsed.as_ref(), "agent-9");
        // Into<String>.
        let s: String = id.into();
        assert_eq!(s, "agent-42");
    }

    #[test]
    fn agent_id_sql_roundtrip() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute("CREATE TABLE t (id TEXT NOT NULL)", [])
            .expect("create");
        let id = AgentId::from("agent-42".to_string());
        conn.execute("INSERT INTO t (id) VALUES (?1)", rusqlite::params![id])
            .expect("insert");
        let back: AgentId = conn
            .query_row("SELECT id FROM t", [], |r| r.get(0))
            .expect("query");
        assert_eq!(back, AgentId::from("agent-42".to_string()));
    }

    #[test]
    fn token_masked_hides_all_but_last_four() {
        let tok = Token::from("0123456789abcdef".to_string());
        assert_eq!(tok.masked(), "****cdef");
        // Short input never panics and never reveals more than available.
        assert_eq!(Token::from("ab".to_string()).masked(), "****ab");
        assert_eq!(Token::from(String::new()).masked(), "****");
        // Display is verbatim.
        assert_eq!(tok.to_string(), "0123456789abcdef");
    }

    #[test]
    fn timestamp_now_is_positive() {
        let t = TimestampMs::now();
        assert!(t.0 > 0, "unix-epoch millis should be positive");
        assert_eq!(TimestampMs::from(123i64).to_string(), "123");
    }

    #[test]
    fn rfcomm_channel_try_from_range() {
        assert!(RfcommChannel::try_from(0u8).is_err());
        assert!(RfcommChannel::try_from(31u8).is_err());
        assert_eq!(RfcommChannel::try_from(1u8).expect("min"), RfcommChannel(1));
        assert_eq!(
            RfcommChannel::try_from(30u8).expect("max"),
            RfcommChannel(30)
        );
        assert_eq!(RfcommChannel::new(15).expect("via new"), RfcommChannel(15));
        assert!(RfcommChannel::new(0).is_err());
        assert_eq!(RfcommChannel(5).to_string(), "5");
    }
}
