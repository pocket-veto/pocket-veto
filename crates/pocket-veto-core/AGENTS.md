# `pocket-veto-core` library crate — contributor notes

`pocket-veto-core` is the dependency-free foundation of the workspace: the wire
protocol, the frame codec, the SQLite persistence, the in-process event bus,
the approval oneshot registry, the config loader, and the dual-schema
normalization that turns host-specific stdin JSON into a canonical internal
shape. It has no `axum`, no `tokio` runtime, and no platform Bluetooth deps.
This is what lets `pocket-veto-bt` and `pocket-veto` both depend on it without pulling
in HTTP or radio deps, and what lets unit tests for protocol, normalization,
and stores run on every CI runner without needing Bluetooth hardware.

A crate-root `pub use` facade in `lib.rs` re-exports the public API (`Config`,
`Db`, `CoreError`, `Host`, `Decision`, `AgentId`, `ApprovalId`, `Token`,
`TimestampMs`, `to_stdout`, …) so downstream crates import directly from
`pocket_veto_core::` rather than reaching into submodules — both
`pocket_veto_core::protocol::Host` and `pocket_veto_core::Host` work. The
facade is additive: the nested `pub mod` declarations stay, so neither
spelling is deprecated.

## The dual-schema normalization (Cursor vs Claude)

Both agent hosts send JSON on stdin, expect JSON on stdout, and treat exit
code 2 as deny. The differences are field names and event-name casing.
`normalize.rs` turns both into the common `InternalEvent`:

```rust
pub struct InternalEvent {
    pub host: Host,                  // Cursor | Claude, defined in protocol.rs
    pub event_name: CanonicalEvent,  // host-agnostic enum
    pub session_id: String,
    pub cwd: String,
    pub tool_name: Option<String>,   // synthesized for some Cursor events
    pub tool_input: Option<Value>,
    pub raw: Value,                  // verbatim, for the audit log
}
```

### Detection rule

The detection is purely the first character's case of the `hook_event_name`
field: uppercase (PascalCase, e.g. `PreToolUse`) -> Claude; lowercase
(camelCase, e.g. `beforeShellExecution`) -> Cursor. The field is always
present in both hosts, and the two hosts use disjoint casing conventions, so
this is reliable. An empty or non-alphabetic first character errors out.

### Cursor -> canonical mapping

| Cursor (camelCase) | Canonical | `tool_name` override |
| --- | --- | --- |
| `beforeShellExecution` | `PreToolUse` | `Some("Shell")` |
| `beforeMCPExecution` | `PreToolUse` | `Some("MCP:<name>")` (name from `toolName` -> `toolInput.name` -> `"unknown"`) |
| `preToolUse` | `PreToolUse` | `None` (uses payload's `toolName`) |
| `postToolUse` / `afterShellExecution` / `afterMCPExecution` | `PostToolUse` | `None` |
| `stop` | `Stop` | `None` |
| `sessionStart` | `SessionStart` | `None` |
| `sessionEnd` | `SessionEnd` | `None` |
| `afterAgentThought` | `AgentThought` | `None` |
| `afterAgentResponse` | `AgentResponse` | `None` |

### Claude -> canonical

Claude event names are already PascalCase (`PreToolUse`, `PostToolUse`,
`Stop`, `SessionStart`, `SessionEnd`, `AgentThought`, `AgentResponse`), so
`map_claude_event` is a passthrough with no `tool_name` override. Unknown
events on either host error out (fail-closed-by-error policy).

Field extraction is dual-keyed: `get_str_field` / `get_field` try the Claude
key first, then the Cursor key. `session_id` and `cwd` are required and error
if missing. The full Canonical set is in `CanonicalEvent` (see
`normalize.rs`); `is_blocking` (which returns true only for `PreToolUse`) is
**not** in pocket-veto-core — it is a private extension trait in the binary's
`hook.rs`, because the blocking/non-blocking distinction is a hook-subcommand
concern, not a protocol concern.

### Implementation — typed constructor + `let-else`

The real logic lives in `impl InternalEvent { fn from_hook_payload(input:
&Value) -> Result<Self> }` (Rule 1: the constructor is a method on the
type it builds, not a free function). The legacy `pub fn normalize(input:
&Value) -> Result<InternalEvent>` is kept as a thin compatibility wrapper that
just delegates to `from_hook_payload`, so existing callers (`hook::run`,
integration tests) do not need to change. Control flow uses `let-else`
(Rule 8) — `let Some(event_name_str) = ... else { return Err(...) };` —
so the "extract-or-fail" ladders stay flat instead of nesting the rest of the
function inside an `ok_or_else` success branch.

## Why exit code 2 is the contract

pocket-veto-core does **not** define or reference exit code 2 in code. The library
only defines *what* to emit on failure: `output::fail_closed_stdout(host,
event)` returns the host-specific deny JSON with reason
`"PocketVeto unreachable: denying for safety"` (the `FAIL_CLOSED_REASON`
constant). The actual `EXIT_DENY = 2` constant and the `HookOutcome::exit_code`
mapping live in the binary at `crates/pocket-veto/src/hook.rs`.

The contract: exit code 2 is deny in both Cursor and Claude Code, and is used
as a fallback if JSON emission fails or the server is unreachable
(fail-closed). Cursor blocking hooks also set `failClosed: true` in
`.cursor/hooks.json`; Claude Code relies on the binary exiting 2 on any
internal error. The fail-closed matrix (which event types block vs
fire-and-forget) is documented at the top of `crates/pocket-veto/src/hook.rs`.

## Output shapes

The two shapes are produced by typed `#[derive(Serialize)]` structs in
`output.rs` (not `serde_json::json!` macros), so the wire field names and
casing are compile-checked. `to_stdout(host, event, decision, reason)` and
`fail_closed_stdout(host, event)` both return the `HookOutput` enum, which is
`#[serde(untagged)]` — serializing it yields the flat Cursor object or the
nested Claude object directly:

```rust
pub enum HookOutput<'a> {
    Cursor(CursorOutput<'a>),   // flat: permission / user_message / agent_message
    Claude(ClaudeOutput<'a>),   // nested: hookSpecificOutput { ... }
}
```

`CursorOutput` is the flat shape; `ClaudeOutput` wraps `HookSpecificOutput`
(field names are camelCase on the wire, so it carries
`#[serde(rename_all = "camelCase")]` with `snake_case` Rust fields). The
decision tag comes from `impl Decision { fn tag(self) -> &'static str }`
(delegating to `Decision::to_db_str`). The two wire shapes the hosts parse are:

- **Cursor** — flat shape:

  ```json
  { "permission": "allow"|"deny"|"ask", "user_message": reason, "agent_message": "" }
  ```

- **Claude Code** — nested `hookSpecificOutput` shape (the modern,
  non-deprecated form):

  ```json
  { "hookSpecificOutput": {
      "hookEventName": "PreToolUse",
      "permissionDecision": "allow"|"deny"|"ask"|"defer",
      "permissionDecisionReason": reason
  } }
  ```

Do **not** use the deprecated top-level `decision` / `reason` fields for
Claude Code's `PreToolUse`. Use `hookSpecificOutput.permissionDecision` and
`permissionDecisionReason`. The deprecated form may stop working. The hook
only ever emits `Allow` / `Deny` / `Ask` for blocking events; `Defer` is
supported on the wire (`Decision::Defer`) but never emitted by the hook
output path.

## `CoreError` design

`error.rs` defines `CoreError` as a `#[derive(Debug, thiserror::Error)]` enum
that is **fully structured** — no variant carries a free-form `String` message.
Protocol failures are categorized by `ProtocolError` and normalization failures
by `NormalizeError`, both typed sub-enums. This lets callers match on a specific
cause (e.g. EOF vs. oversized frame) without parsing error text.

```rust
pub enum CoreError {
    ConfigIo {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    ConfigParse(#[from] toml::de::Error),
    ConfigSerialize(#[from] toml::ser::Error),
    Db(#[from] rusqlite::Error),
    Protocol(#[from] ProtocolError),
    Normalize {
        kind: NormalizeError,
        field: &'static str,
    },
    Io(#[from] io::Error),                  // generic io, NOT config io
}
```

Wire-protocol failures are split into a dedicated sub-enum so the BT bridge can
distinguish "need more bytes" from unrecoverable failures:

```rust
pub enum ProtocolError {
    OversizedFrame { size: usize, max: usize },
    UnexpectedEof { expected: usize },      // "need more bytes", caller keeps reading
    LengthOverflow(&'static str),
    Invalid(#[from] serde_json::Error),
}
```

Normalization failures carry the dynamic part of the failure on the sub-enum;
the `field` context lives on `CoreError::Normalize`:

```rust
pub enum NormalizeError {
    MissingEventName,
    EmptyEventName,
    AmbiguousHost { value: String },
    UnknownHost { value: String },
    UnknownEvent { value: String },
    MissingField,
    UnknownEnum { value: String },
    NoConfigDir,
    NoParentDir,
    ConfigNotFound { path: String },
    OutOfRange { value: i64, min: i64, max: i64 },
}
```

A few deliberate choices:

- `ConfigIo` carries the offending `PathBuf` AND the underlying `io::Error`
  (via `#[source]`), so it stays distinguishable from generic `Io` without
  stringifying. Call sites construct `CoreError::ConfigIo { path, source }`
  directly.
- `Protocol` wraps `ProtocolError`. The "need more bytes" case is
  `ProtocolError::UnexpectedEof { expected }`; `is_protocol_eof()` returns true
  only for that variant (the BT bridge's `drain_client_messages` relies on this
  to distinguish "need more bytes, keep reading" from "stream desynced,
  reconnect"). Oversized frames are `OversizedFrame`; length-prefix arithmetic
  overflow is `LengthOverflow`; JSON failures are `Invalid(#[from] serde_json::Error)`.
  A manual `From<serde_json::Error> for CoreError` bridges the two-step
  `serde_json::Error -> ProtocolError -> CoreError` chain so call sites can use
  `?` directly on a `serde_json` result.
- `Normalize` carries a structured `NormalizeError` kind plus the `field` name
  it was looking at — no free-form `String`.
- Call sites construct the structured variants directly;
  `is_protocol_eof()` is the only retained helper.
- Library code never `panic!`s on expected failures; it propagates via `?`.
  The type alias `Result<T> = std::result::Result<T, CoreError>` is at the
  bottom of the file.
- No `#[non_exhaustive]` is used (see root `AGENTS.md` Rule 6).

## `rusqlite` with `bundled`

The dependency line in `Cargo.toml` (version + `bundled` feature pinned at the
workspace root, referenced via `workspace = true`):

```toml
rusqlite = { workspace = true }
```

The `bundled` feature makes `libsqlite3-sys` compile SQLite from C source via
the `cc` crate, so contributors and CI do not need a system `libsqlite3` /
`sqlite3-dev` installed. It also pins a known-good SQLite version across all
platforms and sidesteps the dynamic-linking segfaults documented in
`rusqlite#914` and `rusqlite#1290`. The cost is a one-time C compile in the
build, which the `cc` crate handles transparently. This is the single most
important dependency choice in the workspace: it is what makes the binary
self-contained on Windows MSVC, Linux musl (static), and macOS, and what lets
the devcontainer build without any system SQLite.

### Other dependency notes

- `hex` is a workspace dep used by `Config::generate_token` (`hex::encode` of
  32 random bytes) instead of a hand-rolled hex encoder (Rule 5).
- `tokio` is narrowed to `features = ["sync", "macros", "rt"]` — `sync` for
  the `oneshot`/`broadcast` primitives in `approvals.rs`/`events.rs`, and
  `macros`+`rt` for `#[tokio::test]` in this crate's own tests. The `time`
  feature was dropped (no `tokio::time` usage in core; the hook timeout lives
  in the binary).
- `serde`, `serde_json`, `toml`, `thiserror`, `rand`, `dirs` are all
  `workspace = true`.

## `Mutex<Connection>` for Send+Sync

`Db` wraps a single `rusqlite::Connection` in a `std::sync::Mutex`:

```rust
pub struct Db {
    conn: Mutex<Connection>,
}
```

`Connection` is `Send` but `!Sync`, so the `Mutex` makes `Db` auto-impl
`Send + Sync`, which is what lets it live behind an `Arc` in axum state and
be shared across the HTTP handlers and the BT bridge task. Every method takes
`&self` and routes through the private `Db::with_conn` helper, which locks the
inner connection briefly and centralizes the poisoned-mutex recovery
(`unwrap_or_else(std::sync::PoisonError::into_inner)`) so a prior panic never
blocks subsequent calls and the recovery boilerplate is not copy-pasted across
every method (Rule 15). Methods never hold the lock across an `await`
(rusqlite is synchronous anyway).

The constructors are `Db::open(&Path) -> Result<Db>` and
`Db::open_in_memory() -> Result<Db>` (used by tests). Both apply the `SCHEMA`
via `execute_batch` (`CREATE TABLE IF NOT EXISTS` for `agents`, `events`,
`approvals`, plus an index `idx_events_agent_ts`) and enable the
`foreign_keys` pragma so the `events.agent_id` FK is enforced. The schema is
self-initializing and idempotent across restarts.

### Row types, enums, and newtypes

Queries that may return zero rows use `OptionalExtension::optional()` (e.g.
`stmt.query_row(...).optional().map_err(CoreError::Db)`) rather than a
hand-rolled `match` on `rusqlite::Error::QueryReturnedNoRows`. The wire enums
(`Host`, `EventKind`, `AgentStatus`, `Decision`) are bound to the DB columns
directly via their `ToSql`/`FromSql` impls (defined in `protocol.rs`), so
there is no manual `parse_host` / `parse_event_kind` helper at the call site —
`row.get("status")?` yields an `AgentStatus` directly.

The row structs adopt the newtypes where it is clean to do so:
`ApprovalRow` / `EventRow` / `AgentRow` use `AgentId` for the id column,
`ApprovalId` for the approval id, `TimestampMs` for the `INTEGER`-ms columns,
and `AgentStatus` for `agents.status`. Two columns are **intentionally kept
as `String`** (adopting a typed enum there would change behavior — see the
`db.rs` module docs): `EventRow.kind` (the column also stores the lifecycle
markers `agent_start` / `agent_end`, which are not `EventKind` variants), and
`AgentRow.host` / `AgentRow.session_id` (`host` may carry arbitrary host tags
sent in the HTTP `/events` body, which the bridge defaults to `Claude` rather
than rejecting; `session_id` is nullable and not a wire enum).

## `ApprovalWaiters` oneshot design

`approvals.rs` is the sender half of the approval round-trip. It holds a
`Mutex<HashMap<String, oneshot::Sender<Decision>>>` keyed by approval id:

```rust
pub struct ApprovalWaiters {
    senders: Mutex<HashMap<String, oneshot::Sender<Decision>>>,
}
```

- `register(&self, approval_id) -> oneshot::Receiver<Decision>` — creates the
  channel, inserts the sender, returns the receiver. Duplicate registration
  drops the previous sender; the older `GET /wait` then observes `RecvError`.
- `resolve(&self, approval_id, decision) -> bool` — removes the sender and
  fires it. Returns `true` if a waiter existed. Send errors (dropped
  receiver) are silently ignored — the waiter went away (timeout/cancel).
- `cancel(&self, approval_id)` — removes without resolving (the receiver
  observes `RecvError::Closed`).

All three route through the private `ApprovalWaiters::with_lock` helper, which
locks the sender map and centralizes the poisoned-mutex recovery
(`unwrap_or_else(std::sync::PoisonError::into_inner)`) — mirroring
`Db::with_conn` (Rule 15) so a poisoned lock never blocks a later
register/resolve/cancel and the recovery boilerplate is not repeated.

`Decision` is defined in `protocol.rs` (`Allow | Deny | Ask | Defer`,
snake_case on the wire), not here. The receiver half of the channel lives in
the binary's `AppState::pending_receivers` — see
`crates/pocket-veto/AGENTS.md` for why the split exists.

The split keeps pocket-veto-core unaware of the HTTP layer: the bridge (in `pocket-veto-bt`,
which depends on pocket-veto-core) calls `resolve` to fire the oneshot, and the
binary's `GET /wait` handler awaits the receiver. Neither side knows about
the other.

## EventBus

`events.rs` wraps a `tokio::sync::broadcast::Sender<EventMessage>`:

```rust
#[derive(Clone)]
pub struct EventBus { tx: broadcast::Sender<EventMessage> }
```

`publish(msg)` silently ignores "no subscribers" (a normal condition — the
bridge may not have connected yet). `subscribe()` returns an independent
receiver with its own queue. A lagged subscriber gets
`broadcast::error::RecvError::Lagged` and re-syncs from SQLite via
`Db::events_since` — **SQLite is the source of truth, the bus is a fast
path** for live dashboard updates and BT-bridge fan-out. The conventional
capacity is 256 (hard-coded in the `Default` impl; `new(capacity)` takes it
as a parameter).

## Config

`config.rs` resolves and loads `~/.pocket-veto/config.toml` (primary path:
`$HOME/.pocket-veto/config.toml`; fallback when `$HOME` is unset:
`dirs::config_dir()/pocket-veto/config.toml`). The config layer is
method-based on `impl Config` (Rule 1) rather than free functions:

- `Config::config_path() -> Result<PathBuf>` — resolves the path (or returns
  `CoreError::Normalize` / `NoConfigDir` if neither home nor config dir is
  locatable).
- `Config::load(path: &Path) -> Result<Self>` — errors distinctly on
  missing file (`NormalizeError::ConfigNotFound`) vs. unreadable
  (`ConfigIo`) vs. unparseable (`ConfigParse`).
- `Config::save(&self, path: &Path) -> Result<()>` — serializes to pretty
  TOML, creates the parent dir, and sets mode `0600` on Unix via
  `set_owner_only` (no-op elsewhere).
- `Config::generate_token() -> Token` — `#[must_use]`; 32 random bytes
  hex-encoded via the `hex` crate (`hex::encode`),   not a hand-rolled encoder
  (Rule 5).

`Config` derives `Default`.
Every field carries `#[serde(default = "default_<field>")]` (or
`#[serde(default)]` when the type's own `Default` already matches), so a
partial or empty TOML file fills missing fields from the same `default_<field>`
helpers `Config::default` uses — no duplicated default literals (Rule 15).
The ten fields: `server_url`, `bind_addr`, `token`, `db_path`,
`approval_timeout_seconds`, `bt_backend`, `bt_com_port`, `bt_adapter_addr`,
`bt_channel`, `devcontainer`. Defaults: `http://127.0.0.1:38475` /
`127.0.0.1:38475`, 300-second approval timeout, a fresh 32-byte hex token,
`bt_backend = Bluer`, `devcontainer = false`.

The `token` field is the `Token` newtype (`#[serde(transparent)]`), so the
on-disk TOML shape is identical to a bare-`String` token. The
`bt_backend` field is the `BtBackend` enum (`Bluer | Serialport`,
`#[serde(rename_all = "snake_case")]`), which also derives `Default` —
`BtBackend::default()` returns `Bluer`. Note this default is **not** itself
platform-aware; the actual platform-native selection happens in the binary's
`init` (which overrides to `Serialport` on Windows, or when `bt_com_port` is
set), then persists the choice into `bt_backend`.

## Domain newtypes & wire enums

`protocol.rs` is the foundation the rest of the crate builds on. The wire
enums (`Host`, `EventKind`, `AgentStatus`, `Decision`) derive
`Debug, Clone, Copy, PartialEq, Eq` plus `serde::{Serialize, Deserialize}`
with `rename_all = "snake_case"`/`"lowercase"` so the wire tags are stable
strings the Android side can switch on. Each enum gets a `to_db_str` /
`from_db_str` pair plus `Display` / `FromStr` delegating to them, and
`rusqlite` `ToSql` (TEXT) / `FromSql` (TEXT -> `from_db_str`) glue — so the
same enum is the wire type, the `Display`/`FromStr` type, and the SQL column
type, with no manual `parse_host`/`parse_event_kind` at the DB boundary.

The domain identifiers are newtypes (Rule 7), wire-transparent via
`#[serde(transparent)]`:

| Newtype | Backing | Notes |
| --- | --- | --- |
| `AgentId` | `String` | via `string_newtype!` macro |
| `ApprovalId` | `String` | via `string_newtype!` macro |
| `SessionId` | `String` | via `string_newtype!` macro |
| `ComPort` | `String` | via `string_newtype!` macro |
| `Token` | `String` | via `string_newtype!`; `Token::masked()` for safe logging |
| `RfcommChannel` | `u8` | `RfcommChannel::new`/`try_from(u8)` validates `1..=30`; plain JSON number (no `transparent`) |
| `TimestampMs` | `i64` | `TimestampMs::now()`; JSON number / SQLite `INTEGER` |

The private `string_newtype!` macro (Rule 15: within-crate dedup) stamps
out the `String`-backed newtypes with `Debug, Clone, PartialEq, Eq, Hash,
Serialize, Deserialize`, plus `Display`, `AsRef<str>`, `From<String>`,
`From<Newtype> for String`, infallible `FromStr`, and `ToSql`/`FromSql`
delegating to `String` — so each newtype is usable on the wire, in `Display`/
`FromStr`, and as a SQL column without per-type boilerplate. `RfcommChannel`
and `TimestampMs` are numeric, so they are written by hand (range validation
for the channel; `now()` for the timestamp) rather than via the macro. No
`#[non_exhaustive]` is added (Rule 6).
