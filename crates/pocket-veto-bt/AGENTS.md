# `pocket-veto-bt` library crate — contributor notes

`pocket-veto-bt` is the only crate with `#[cfg(target_os = ...)]` conditional
dependencies. It isolates the Bluetooth radio behind a platform-agnostic
`BtTransport` trait so the rest of the workspace compiles cleanly in
devcontainers and CI where there is no `libdbus-1-dev` or paired COM port.
The bridge logic (`bridge.rs`) is platform-agnostic and depends only on the
trait; the platform backends are cfg-gated modules.

## The `BtTransport` trait

Defined in `src/bridge.rs` (not a separate `transport.rs`), using native
async-fn-in-trait (AFIT, stable on 1.96) with a `Send` supertrait bound and an
explicit `+ Send` bound on each returned future, so the bridge can move the
transport into a `tokio::spawn` task:

```rust
pub trait BtTransport: Send {
    fn connect(&mut self) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
    fn read_frame(&mut self) -> impl std::future::Future<Output = anyhow::Result<Vec<u8>>> + Send;
    fn write_frame(&mut self, payload: &[u8])
        -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
    fn close(&mut self) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
    fn is_connected(&self) -> bool;
}
```

The impls keep `async fn` bodies — the compiler matches `async fn` in an impl
to the `-> impl Future + Send` trait methods and verifies each future is
`Send`. `read_frame` returns one **full frame** (4-byte big-endian length
prefix + JSON payload) so the bridge can append it to its accumulation buffer
and feed it straight into `pocket_veto_core::protocol::decode_client_message`,
which expects the buffer to start with the prefix. `write_frame` takes a JSON
**payload** (no prefix); the transport adds the prefix via the shared
`crate::frame` helpers, so the transport owns the wire framing end to end.
Errors are `anyhow::Result` (not `pocket_veto_core::error::Result`) so backend
impls can layer arbitrary error context without a custom error enum.
`is_connected` is the only sync method — a cheap `&self -> bool` query.

The framing helpers live in `src/frame.rs` (`read_length_prefixed` /
`read_length_prefixed_sync`, `write_length_prefixed` /
`write_length_prefixed_sync`, `build_frame`), centralized so the wire format
`[4-byte big-endian u32 length][JSON payload]` is defined once and the
platform backends stay thin (Rule 15). The real backends (Linux, Windows)
return one complete frame per `read_frame` call; the mock delivers whole
frames through an `mpsc` channel (its `write_frame` builds the frame from the
payload via `build_frame`). The bridge's accumulation buffer still handles the
case where a future backend returns partial bytes or multiple frames in one
read (see "Frame accumulation" below).

## cfg-gated backends

`src/lib.rs` gates the modules:

```rust
pub mod bridge;   // unconditional
pub mod frame;    // unconditional
#[cfg(any(test, feature = "mock"))]
pub mod mock;

#[cfg(all(target_os = "linux", feature = "linux-bt"))]
pub mod linux;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
pub mod macos;
```

- `bridge` and `frame` are always compiled.
- `mock` is compiled under `cfg(test)` (the crate's own unit tests) **or** the
  `mock` cargo feature (downstream crates' tests/dev builds, enabled via a
  `[dev-dependencies]` entry with `features = ["mock"]`). A plain `cargo build
  -p pocket-veto-bt` does not compile it.
- `linux` requires **both** `target_os = "linux"` **and** the `linux-bt`
  cargo feature.
- `windows` requires only `target_os = "windows"`.
- `macos` requires only `target_os = "macos"` (and is a `compile_error!`
  stub — see below).

## The `linux-bt` feature flag — why it is default-off

In `Cargo.toml`:

```toml
[features]
default = []
linux-bt = ["dep:bluer"]
mock = []                       # compile the mock backend in a non-test context

[dependencies]
pocket-veto-core = { workspace = true }
tokio       = { workspace = true, features = ["rt", "rt-multi-thread", "sync", "time", "macros", "io-util"] }
anyhow      = { workspace = true }
serde_json  = { workspace = true }   # in deps (not dev-deps): the bridge uses serde_json::to_vec in non-test code
tracing     = { workspace = true }

[target.'cfg(target_os = "linux")'.dependencies]
bluer = { workspace = true, optional = true }

[target.'cfg(target_os = "windows")'.dependencies]
serialport = { workspace = true }
```

The `bluer` and `serialport` versions (and `bluer`'s `rfcomm` / `bluetoothd`
features) are pinned at the workspace root (`Cargo.toml [workspace.dependencies]`);
this crate references them via `workspace = true`. `linux-bt` is **default-off**
because `bluer` needs `libdbus-1-dev` system headers at build time and
`bluetoothd` at runtime. Default-off keeps the workspace buildable in bare
environments (the devcontainer, CI runners that do not install
`libdbus-1-dev`). Enable with `cargo build --features pocket-veto-bt/linux-bt`
after installing `libdbus-1-dev`; the `linux-bt-build` CI job
(`.github/workflows/ci.yml`) does exactly this on `ubuntu-latest` (installs
`libdbus-1-dev`, then `cargo build --workspace --features
pocket-veto-bt/linux-bt`) so a `bluer` compile regression is caught even
though the default build does not enable the feature. The `serialport`
dependency on Windows is **not** feature-gated because it is pure Rust and
compiles cleanly with no system headers.

The `tokio` features are narrowed from `full` to
`["rt", "rt-multi-thread", "sync", "time", "macros", "io-util"]` — exactly
what the bridge uses (the run loop, `mpsc`, `time::{interval, sleep}`, and
the async IO helpers in `frame.rs`). `serde_json` is in deps (not dev-deps)
because the bridge calls `serde_json::to_vec` in non-test code.

## Why `bluer` on Linux and `serialport` on Windows

- **Linux (`LinuxBtTransport`):** `bluer` is the official BlueZ binding,
  async Tokio-native, with an RFCOMM `Listener::bind` + `accept` API. The
  transport is server-side — the phone initiates. `new(adapter_addr,
  channel)` binds the listener up front; `connect()` drops any existing
  stream and `accept()`s an inbound phone connection. `read_frame` delegates
  to `frame::read_length_prefixed`, which reads the 4-byte **big-endian** `u32`
  length prefix, enforces `pocket_veto_core::protocol::MAX_FRAME_SIZE`,
  `read_exact`s the payload, and returns the full frame (prefix + payload). Note:
  `Listener::bind` does **not** register an SDP record — the channel number
  must be pre-agreed with the phone via `pocket-veto init` and stored in
  config. The Android side connects via the standard SPP UUID
  `00001101-0000-1000-8000-00805F9B347F`; the matching channel on the host
  is discovered by `init`.

- **Windows (`WindowsBtTransport`):** after the phone is paired via Windows
  Settings, the SPP service shows up as a virtual COM port (e.g. `COM3`). The
  transport just opens that COM port at 115200 baud and treats it as the same
  length-prefixed frame channel. `serialport::SerialPort` is synchronous, so
  blocking reads/writes are dispatched onto the blocking thread pool via
  `tokio::task::spawn_blocking` to keep the async runtime's workers free. The
  port is guarded by `Arc<Mutex<...>>` so the blocking-pool tasks do not
  race. A future revision could switch to `tokio-serial` for true async IO.
  `new(com_port)` is synchronous and does not open the port — that happens
  on the first `connect()`. The `READ_TIMEOUT` of 5 seconds is deliberately
  shorter than the bridge's 45-second heartbeat timeout so the
  heartbeat-timeout path wins on a quiet radio.

## The macOS stub policy

`src/macos.rs` in full:

```rust
//! macOS Bluetooth is not yet supported. See `docs/architecture.md#macos-roadmap`.

compile_error!(
    "PocketVeto Bluetooth is not yet supported on macOS. \
     See docs/architecture.md#macos-roadmap."
);
```

The entire module is a doc comment + a single `compile_error!`. There is no
stub `BtTransport` impl, no stub struct. The module's sole purpose is to fail
the build with an actionable message if someone tries to compile the
Bluetooth backend on macOS. The non-BT parts of the binary (server, hook
subcommand) still compile and run on macOS, so a Mac can host agents that
  gate against a Linux/Windows server, or act as a devcontainer host. macOS
  Bluetooth support is not yet implemented; it will likely land via
  `objc2-io-bluetooth` once that crate matures. Do **not** add a stub impl
  that pretends to work — the compile error is the correct behavior.

## The bridge's reconnect / backoff / heartbeat / replay logic

`BtBridge<T: BtTransport>` (`src/bridge.rs`) owns the connection lifecycle.
The exact constants:

```rust
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const HEARTBEAT_TIMEOUT: Duration  = Duration::from_secs(45); // 3 missed acks
const BACKOFF_SCHEDULE: &[Duration] = &[
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(30),  // cap
];
```

Note the backoff jumps 8s -> 30s (no 16s slot). The index saturates at the
schedule length, so after the fifth failure the bridge sleeps 30s forever.
The index resets to 0 only on a **successful** `connect()`.

The `run` loop cycles through: connect-with-backoff -> `replay_missed_events`
-> `drive_connection` -> `close` -> match outcome -> backoff. The only way
`run` returns `Ok(())` is `ConnectionOutcome::OutboxClosed` (the server
dropped the sender); transport errors and heartbeat timeouts never bubble out
of `run` — they trigger a reconnect. A backoff slot always runs before the
next reconnect attempt, even on a heartbeat timeout, to avoid a tight
reconnect loop if the peer is flapping.

`drive_connection` is a four-arm `tokio::select!`:

1. `self.outbox.recv()` — outbox -> transport. `None` means the server shut
   down; return `OutboxClosed`. On `Some(msg)`, `handle_outbox_message`
   encodes the `ServerMessage` and writes it, tracking `known_agents` on
   `AgentStart`.
2. `self.transport.read_frame()` — transport -> bridge, with frame
   accumulation (see below). A read error returns `TransportError`.
3. `heartbeat_tick.tick()` — every `heartbeat_interval` (15s default),
   encode `ServerMessage::Heartbeat { ts: now_ms() }` and write it.
4. `time::sleep_until(timeout_deadline)` — fires if no frame arrived within
   `last_activity + heartbeat_timeout` (45s default); returns
   `HeartbeatTimeout`. Any frame (ack or message) resets `last_activity`.

`replay_missed_events` queries `self.db.events_since(agent_id, since)` where
`since = self.last_acked_ts` (an `i64`-ms watermark advanced by
`ClientMessage::HeartbeatAck`). The `since_ts` boundary is **strictly
exclusive** (`ts > ?2`). Replay is scoped to `known_agents` (a
`HashSet<AgentId>` of agents the bridge has ever sent an `AgentStart` for —
the phone would have no card for others). For each known agent, the bridge
re-emits an `AgentStart` from the persisted `agents` row (so the phone has
fresh name/host/workspace metadata)
**before** the events. Replay failures are logged but do not tear down the
connection — the live outbox stream is still authoritative.

## Frame accumulation — the classic stream-framing bug spot

`drive_connection` holds a `read_buf: Vec<u8>`. On each `read_frame` it
appends the new bytes to the tail, then calls `drain_client_messages`, which
loops decoding from the front:

```rust
loop {
    match protocol::decode_client_message(read_buf) {
        Ok((consumed, msg)) => {
            read_buf.drain(..consumed);
            self.route_client_message(&msg);
        }
        Err(e) if e.is_protocol_eof() => return Ok(()),  // need more bytes; keep buffer
        Err(e) => return Err(anyhow::anyhow!("decode client message: {e}")),  // desynced; reconnect
    }
}
```

This is the textbook-correct pattern:

- Append to tail, drain from front, loop (handles one read delivering
  multiple frames).
- Partial frame -> `is_protocol_eof()` true -> `return Ok(())` leaving the
  partial bytes for the next `read_frame`. (Verified: EOF fires both when
  the buffer is shorter than the 4-byte prefix and when the declared payload
  is truncated.)
- Non-EOF protocol error (oversized declared length, bad JSON) -> the stream
  is unrecoverably desynced -> return `Err`, which becomes
  `ConnectionOutcome::TransportError` -> reconnect.

Today the real backends pre-frame via `frame::read_length_prefixed` (they
`read_exact` the prefix and payload themselves and return the full frame), so
the buffer usually drains one message per read. The mock exchanges whole
frames through `mpsc`. The accumulation buffer is defense-in-depth: it is
correct for partial reads and is required for the multi-frame-per-read case
the `read_frame` doc explicitly anticipates. There is currently no test that
feeds the bridge partial bytes directly; the EOF path is only tested at the
`pocket-veto-core::protocol` level. If you add a backend that returns partial
reads, add such a test.

## The mock backend

`mock.rs` provides `mock_pair() -> (MockTransport, MockPeer)` backed by two
`mpsc::channel::<Vec<u8>>(64)` (one per direction). Both sides start
`connected = true`; `connect()` is a no-op while the shared `broken` flag is
false. `MockPeer::read_server_message()` decodes the next frame the bridge
wrote (how tests assert what was sent); `MockPeer::write_client_message()`
encodes and sends a `ClientMessage` (how tests inject received frames).
Frames exchanged through the mock are exactly the bytes the real transports
carry (4-byte length prefix + JSON): the bridge hands `write_frame` a JSON
payload and the mock adds the prefix via `frame::build_frame` before sending
the full frame down the channel; in the other direction `MockPeer` sends a
full frame (`encode_client_message`) and the bridge reads it back as one. So
the bridge's framing logic is exercised identically to a real backend.

`break_connection()` (on either side) sets the shared `broken = true` flag
(`MockControl` holds only that bool), so `read_frame` / `write_frame` short-circuit
with a "broken connection" error and the bridge goes through its backoff
loop. There is **no
"radio came back" path** — once broken, every subsequent `connect()` errors.

The mock is what makes the full hook -> server -> BtBridge -> MockPeer
pipeline verifiable in CI with no radio. When you add a new `ServerMessage`
or `ClientMessage` variant, add a mock round-trip test that exercises it
end-to-end.

## Exceptions

### `BtTransport` trait return type (Rule 2 — `anyhow` only at binary boundaries)

**Exception:** `BtTransport` keeps `anyhow::Result`.

The `BtTransport` trait methods (`connect`, `read_frame`, `write_frame`, `close`) return `anyhow::Result<...>` rather than a typed `thiserror` enum. Rationale: the `BtBridge` consumer only ever **logs** the error and **reconnects** with backoff — it never matches on error kind, never discriminates variants, and has no recovery branch that depends on the error's shape. A typed enum would add boilerplate (a variant per backend: `bluer::Error`, `serialport::Error`, `io::Error`, `mock::Error`) without enabling any new behavior.

This is the **only** `anyhow` usage permitted in `pocket-veto-bt`. All other fallible public APIs in this crate return typed `Result`. If a future feature needs to discriminate a `BtTransport` error kind (e.g. "connection refused vs. radio gone"), a typed `BtError` enum is introduced instead.
