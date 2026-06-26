# PocketVeto architecture

A local-only approval gate and live progress dashboard for AI coding agents.
The user is away from the PC; agents request permission to run risky tools and
the user approves, denies, or replies from their Android phone over Bluetooth.
The phone also shows live progress of every running agent. No internet, no LAN
routing, no cloud.

## Diagram

```mermaid
flowchart TB
    subgraph Host["Host machine (Windows / native Linux / macOS)"]
        Cursor["Cursor agent"]
        Claude["Claude Code agent"]
        Devc["Agent in devcontainer<br/>(optional)"]
        Bin["pocket-veto binary<br/>subcommand: serve"]
        HookSub["pocket-veto binary<br/>subcommand: hook"]
        Bridge["BtBridge task<br/>cfg-gated backend"]
        Cursor -. hooks.json .-> HookSub
        Claude -. settings.json .-> HookSub
        Devc -. hook, via host.docker.internal .-> HookSub
        HookSub -->|HTTP 127.0.0.1:38475| Bin
        Bin <-->|tokio mpsc| Bridge
    end

    subgraph Phone["Android phone"]
        BtSvc["BluetoothService<br/>foreground service<br/>RFCOMM socket"]
        Notif["Action-button<br/>notifications"]
        Dash["Dashboard Activity<br/>Compose LazyColumn"]
        BtSvc --> Notif
        BtSvc --> Dash
    end

    Bridge <-->|RFCOMM SPP<br/>length-prefixed JSON| BtSvc
```

The single binary has two long-lived modes and one setup mode:

- `pocket-veto serve` — long-running server: axum HTTP API on
  `127.0.0.1:38475`, SQLite via `rusqlite` bundled, Bluetooth bridge task,
  WebSocket hub for an optional local browser dashboard.
- `pocket-veto hook` — invoked by agent hosts, reads stdin JSON, talks to the
  server over localhost HTTP, blocks for approvals, prints the correct
  response shape, exits with the correct code.
- `pocket-veto init` — interactive setup: pair BT, detect COM port / RFCOMM
  channel, write hook configs, generate bearer token, write config file,
  register the server as a launchd / systemd / Windows-autostart service.

## Why a single Rust binary

Three cross-platform constraints drive the single-binary design:

1. **Linux RFCOMM needs `bluer`.** The only solid BlueZ RFCOMM binding is
   `bluer`, which is Rust-only and async Tokio-native. Shelling out to
   `rfcomm`/`bluetoothctl` is fragile and not async-friendly.
2. **Windows serial I/O is most reliable through the `serialport` Rust crate.**
   It provides direct, native serial access with no runtime-binding
   incompatibilities.
3. **Devcontainers cannot get host Bluetooth.** Docker Desktop does not pass
   USB/Bluetooth from Windows/Mac hosts into the Linux VM, and containerizing
   Bluetooth on native Linux needs `--privileged --net=host --cap-add=NET_ADMIN`
   plus running `bluetoothd` in the container — not a setup most users tolerate.

A single Rust binary addresses all three:

- **One language, three platforms, conditional compilation.** `bluer` on
  Linux, `serialport` crate on Windows, both selected via
  `#[cfg(target_os = ...)]` against a common `BtTransport` trait. Same
  codebase, same protocol, same binary name.
- **The hook is a subcommand, not a separate package.** Cursor and
  Claude Code both accept any executable as a hook. `pocket-veto hook` reads
  stdin JSON, auto-detects the host from event-name casing, and emits the
  correct response shape. One binary, both hosts, zero shell wrappers.
- **Devcontainers become natural.** The container does not need Bluetooth —
  it just runs `pocket-veto hook`, which talks to a server on the host via
  `host.docker.internal`. The host owns the radio. See
  [docs/devcontainer.md](docs/devcontainer.md).
- **Single-binary deployment.** `rusqlite` with the `bundled` feature
  compiles SQLite from C source via the `cc` crate — no system SQLite
  dependency. Static-link against musl on Linux for a fully static binary.
  One file per platform, drop it on PATH, done. No runtime dependency.
- **Cross-compilation via CI, not locally.** GitHub Actions matrix builds
  natively on each target runner. Release artifacts land in GitHub Releases.

A Rust binary starts in roughly a millisecond, so hook latency is dominated by
the localhost round-trip and human thinking time, not process spawn.

## Repository layout

```
pocket-veto/
  Cargo.toml                    # workspace root
  Cargo.lock
  README.md
  ARCHITECTURE.md
  LICENSE                       # MIT
  rustfmt.toml
  clippy.toml
  .github/workflows/ci.yml      # build matrix + android job
  .github/workflows/release.yml # tag-triggered release artifacts
  install.sh                    # POSIX install script
  install.ps1                   # Windows install script
  crates/
    pocket-veto/                # the binary
      Cargo.toml
      AGENTS.md
      src/
        main.rs                 # clap subcommand dispatch: serve | hook | init
        serve.rs                # axum server, sqlite pool, bt bridge spawn
        hook.rs                 # hook subcommand: stdin -> server -> stdout
        init.rs                 # interactive setup
        lib.rs                  # module re-exports for integration tests
    pocket-veto-core/                    # shared library
      Cargo.toml
      AGENTS.md
      src/
        lib.rs
        protocol.rs             # message enums, serde, frame codec
        config.rs               # ~/.pocket-veto/config.toml loader
        db.rs                   # rusqlite bundled, schema, stores
        events.rs               # event store + in-process bus (tokio broadcast)
        approvals.rs            # approval store with waiters (tokio oneshot)
        normalize.rs            # stdin -> InternalEvent, Cursor vs Claude detection
        output.rs               # decision -> host-specific stdout JSON
        error.rs                # thiserror types
    pocket-veto-bt/                      # Bluetooth transport, cfg-gated
      Cargo.toml
      AGENTS.md
      src/
        lib.rs                  # BtTransport trait re-export, cfg-gated modules
        bridge.rs               # BtTransport trait, BtBridge reconnect/heartbeat/replay
        linux.rs                # bluer-based RFCOMM (cfg target_os = "linux" + linux-bt feature)
        windows.rs              # serialport crate on COM port (cfg target_os = "windows")
        macos.rs                # stub: compile_error! with helpful message
        mock.rs                 # in-memory transport for tests
  android/
    AGENTS.md
    app/
      src/main/
        kotlin/io/pocketveto/
          service/              # BluetoothService, NotifFactory, DecisionReceiver
          ui/                   # DashboardActivity, card composables, PairingActivity
          data/                 # FrameCodec, Protocol, AgentRegistry
          util/                 # Permissions, Reconnect
        AndroidManifest.xml
        res/...
      build.gradle.kts
    build.gradle.kts
    settings.gradle.kts
  docs/
    setup.md
    architecture.md
    troubleshooting.md
    devcontainer.md
```

The workspace is split into three crates so unit tests for protocol,
normalization, and stores run on every CI runner without needing Bluetooth
hardware. `pocket-veto-core` is the dependency-free foundation (protocol, SQLite,
config, normalization, output). `pocket-veto-bt` isolates the radio behind a
`BtTransport` trait with cfg-gated backends so the rest of the workspace
compiles cleanly in devcontainers and CI. `pocket-veto` is the thin binary
that wires the two together. See the per-crate `AGENTS.md` files for
technical notes and mandatory rules.

## Wire protocol

Frame layer over the RFCOMM byte stream, mirrored exactly by the Android
`FrameCodec.kt`:

```
[4 bytes: big-endian uint32 length N][N bytes: UTF-8 JSON]
```

The Rust codec lives in `crates/pocket-veto-core/src/protocol.rs`. The 1 MiB cap
(`MAX_FRAME_SIZE`) is enforced on both encode and decode. The decode path
distinguishes "not enough bytes yet" (returns an EOF-flagged error so the
caller keeps reading) from "stream desynced" (a hard error that forces a
reconnect).

Message types are `#[serde(tag = "type", rename_all = "snake_case")]` enums
validated by serde:

```rust
// PC -> Phone
pub enum ServerMessage {
    AgentStart { agent_id, session_id, host: Host, name, workspace, started_at },
    AgentEvent { agent_id, kind: EventKind, tool: Option<String>, payload, ts },
    ApprovalRequest { approval_id, agent_id, tool, summary, detail, timeout_at },
    AgentEnd { agent_id, ended_at, status: AgentStatus },
    Heartbeat { ts },
}

// Phone -> PC
pub enum ClientMessage {
    Subscribe { filter: Option<SubscriptionFilter> },
    ApprovalDecision { approval_id, decision: Decision, note: Option<String> },
    HeartbeatAck { ts },
}
```

`Decision` is `Allow | Deny | Ask | Defer` (snake_case on the wire). `Host` is
`Cursor | Claude` (lowercase on the wire). See `crates/pocket-veto-core/src/protocol.rs`
for the full field list and `crates/pocket-veto-core/AGENTS.md` for the normalization
table that turns host-specific stdin JSON into these types.

Heartbeats fire every 15 seconds; three missed acks (45 seconds with no frame
of any kind) triggers a reconnect. Every event is persisted to SQLite before
being emitted, so the phone re-syncs on reconnect: the server replays missed
events from `events.since(lastAckedTs)` for every agent the phone already
knows about. See `crates/pocket-veto-bt/src/bridge.rs::replay_missed_events`.

## Security model

- **Bearer token** between the `hook` subcommand and the server. Stored in
  `~/.pocket-veto/config.toml` (mode 0600). The subcommand reads it from
  there; the server validates on every request except `/health`. Bluetooth
  pairing is the outer trust boundary; the token is defense-in-depth against a
  malicious process on the PC.
- **Fail closed.** Cursor blocking hooks set `failClosed: true`. Claude
  blocking hooks rely on the binary exiting 2 on any internal error. A dead
  server or dead Bluetooth link means agents are denied, not silently allowed.
- **No network exposure by default.** The server binds `127.0.0.1` only. The
  Bluetooth radio is the only off-PC transport. Devcontainer mode explicitly
  opts into `0.0.0.0` binding, gated by the bearer token, and only makes sense
  on a trusted home network.
- **Audit log.** Every event and every approval decision is in SQLite. The
  dashboard's History view reads from `GET /agents/:id/history`.
- **Bluetooth pairing is the trust boundary.** No additional crypto on the
  RFCOMM link is needed for v1 — Bluetooth Classic provides link-layer
  encryption once paired. If the threat model later includes a compromised
  host, an application-layer HMAC over frames can be added without protocol
  changes (a `mac` field on each message).

## macOS roadmap

macOS Bluetooth is **post-v1**. The `pocket-veto-bt/src/macos.rs` module is an
intentional `compile_error!` stub:

```rust
#[cfg(target_os = "macos")]
compile_error!(
    "PocketVeto Bluetooth is not yet supported on macOS. \
     See docs/architecture.md#macos-roadmap."
);
```

macOS does not expose RFCOMM to non-Apple processes cleanly: `IOBluetooth` is
Objective-C and the Rust story (`objc2-io-bluetooth`) is immature. The non-BT
parts of the binary (server, hook subcommand) compile and run on macOS, so a
Mac user can still use PocketVeto in a "server on a Linux/Windows box, Mac
just runs the agent" topology — or as a devcontainer host that delegates the
radio to a Linux/Windows peer. Full macOS Bluetooth support is a future
milestone and will likely land via `objc2-io-bluetooth` once that crate
matures.
