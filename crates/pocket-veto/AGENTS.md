# `pocket-veto` binary crate — contributor notes

This crate is the user-facing binary: a thin clap CLI that dispatches to three
subcommands (`serve`, `hook`, `init`) whose logic lives in the library target
so integration tests can import them as `pocket_veto::...`. It depends on
both `pocket-veto-core` (protocol, db, config, normalization, output) and `pocket-veto-bt` (the
`BtTransport` trait and `BtBridge`).

## The three-subcommand structure

`src/main.rs` is a thin (~26-line) wrapper (Rule 13): it parses the CLI,
initializes tracing once, and dispatches to the matched subcommand's `run`
impl. It returns `Result<(), ExitCode>` — no `process::exit`, no hand-rolled
`match` over exit codes. clap parsing, the `Command` enum, the per-subcommand
arg structs (`ServeArgs` / `HookArgs` / `InitArgs`), the shared `Ctx`, and
the `Subcommand` dispatch trait all live in `cli.rs`. Dispatch is
`impl Subcommand for Command` using native async-fn-in-trait (AFIT, stable on
1.96 — no `async_trait` crate, Rule 11); each arm delegates to the arg
struct's own `run` impl, which lives next to the subcommand logic it
delegates to (`serve` / `hook` / `init`). A successful deny/timeout still
returns `Ok(ExitCode)` carrying the non-zero code the hook contract requires
(exit 2 = deny / fail-closed, exit 0 = allow / ask / non-blocking);
infrastructure errors propagate as `Err` and are logged via `tracing::error!`
in `main`, then mapped to `ExitCode::FAILURE` by the runtime.

- `Command::Serve` loads config and calls `serve::run(config)`.
- `Command::Hook` runs the hook flow and maps the `HookOutcome` to an
  `ExitCode` via `HookOutcome: Termination` (no `process::exit`).
- `Command::Init` carries seven flags (`--bin-path`, `--keep-token`,
  `--devcontainer`, `--skip-bt`, `--bt-channel`, `--bt-com-port`,
  `--bt-adapter-addr`). It resolves any missing Bluetooth params
  interactively (delegated to `cfg(target_os = ...)` helpers so the pure
  helpers in `init.rs` stay stdin-free and unit-testable), then writes the
  config and hook files via `run_init_flow`.

The `InitOpts` struct decouples the resolved CLI + interactive-prompt inputs
from the pure file-writing helpers in `init.rs`. This is what lets
`tests/init_subcommand.rs` drive `build_config`, `write_cursor_hooks`,
`write_claude_hooks`, `write_systemd_unit`, and `write_launchd_plist` against
`tempfile::tempdir()` roots with no stdin/stdout.

`init_tracing(default_filter: &str)` and `now_ms() -> i64` live in `lib.rs`
(the library target), not in `main.rs`. `init_tracing` is the single source
of truth for subscriber setup — guarded by a `OnceLock` so re-entry from
tests or repeated subcommand dispatch does not panic on a second `try_init`.
`now_ms` is
a thin shim over `pocket_veto_core::TimestampMs::now()` so every subcommand
shares one typed timestamp helper (Rule 15: no cross-crate duplication).

## Why three crates

The split rationale lives in `pocket-veto-bt/src/lib.rs` and is inferable from the
dependency graph: `pocket-veto -> {pocket-veto-core, pocket-veto-bt}`, `pocket-veto-bt -> pocket-veto-core`,
`pocket-veto-core` has no internal deps. The point is that `pocket-veto-core` is the
dependency-free foundation (compiles and tests on any platform), `pocket-veto-bt`
isolates the radio behind a trait so the workspace builds in environments
without `libdbus-1-dev` or a paired COM port, and `pocket-veto` is the thin
binary that wires them together. The binary only ever depends on the
`BtTransport` trait, never on a platform backend directly — which is what
makes `run_with_bridge_on<T: BtTransport>` generic and testable with
`pocket_veto_bt::mock`.

## `AppState` design

`AppState` (`src/serve.rs`) is `#[derive(Clone)]` so it can be cloned into
every axum handler cheaply. The heavy fields are behind `Arc`:

```rust
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub bus: EventBus,                 // Clone shares a broadcast::Sender internally
    pub waiters: Arc<ApprovalWaiters>,
    pub config: Arc<Config>,
    pub bridge_tx: mpsc::Sender<ServerMessage>,   // cheaply cloneable
    pub pending_receivers: Arc<Mutex<HashMap<String, oneshot::Receiver<Decision>>>>,
    pub announced_agents: Arc<Mutex<HashSet<String>>>,
}
```

A few non-obvious points:

- `EventBus` is stored directly (not behind `Arc`) because it is
  `#[derive(Clone)]` and internally shares a `tokio::sync::broadcast::Sender`.
  Cloning it is just a refcount bump.
- `bridge_tx` is a `tokio::mpsc::Sender`, also cheaply cloneable.

### `pending_receivers` — the approval round-trip

This is the second half of a deliberately split oneshot channel. The sender
half lives in `pocket_veto_core::approvals::ApprovalWaiters` (which the bridge holds
an `Arc` to). `POST /approvals` calls `state.waiters.register(&id)` to create
the channel, stores the returned `oneshot::Receiver<Decision>` in
`pending_receivers` keyed by the approval id (a UUID v4 string), and forwards
an `ApprovalRequest` frame to the bridge outbox. `GET /approvals/:id/wait`
removes the receiver from the map and awaits it with a `tokio::time::timeout`
wrapping the oneshot.

The split keeps `pocket-veto-core` untouched (it only owns the senders) while letting
the two HTTP handlers share the receiver across separate requests. The bridge
— which lives in `pocket-veto-bt` and depends on `pocket-veto-core` — fires the oneshot via
`ApprovalWaiters::resolve` without knowing about the HTTP layer's receiver
stash.

### `announced_agents` — AgentStart dedup

Keyed by `agent_id` (which under the hook's mapping equals `session_id`).
The `should_announce` helper atomically checks and inserts. On the first
`POST /events` for a new agent this server lifetime, an `AgentStart` frame is
forwarded to the bridge outbox **before** the `AgentEvent`. The set is scoped
to a single server process (not persisted) — on a server restart the phone
re-learns each agent on its next event. A poisoned mutex is recovered via
`into_inner` so a prior panic never blocks the event itself.

## `build_state` / `serve_loop` / `run_with_bridge_on` factoring

There are four entry points, layered to separate state construction from
bridge spawning from listener binding from HTTP serving:

- `run(config)` — production, headless. Spawns `bridge_drainer` (a no-op
  consumer that logs and drops each `ServerMessage`) because the real
  platform BT backends are not wired into the binary yet. The HTTP API still
  works end-to-end.
- `run_with_bridge<T: BtTransport + Sync + 'static>(config, transport)` —
  binds its own listener and spawns the real `BtBridge` over `transport`.
- `run_with_bridge_on<T: BtTransport + Sync + 'static>(config, transport, listener)` —
  accepts a pre-bound listener. This is the form integration tests use:
  bind `127.0.0.1:0`, read the ephemeral port, fix up
  `config.server_url`, then spawn `run_with_bridge_on` in a tokio task.
- `build_state(config) -> ServerParts` — shared constructor that both `run`
  and `run_with_bridge_on` call. Returns the `AppState` for the router plus
  the bridge-outbox receiver and shared `Arc<Db>` / `Arc<ApprovalWaiters>`
  handles the bridge task needs.
- `serve_loop(state, listener)` — drives axum on the bound listener with
  graceful shutdown on ctrl-c (Unix only).

`build_router` is `pub` so integration tests can build a router against an
in-memory `Db` and a known token without binding a socket. The tests use this
in three ways: `tests/serve_api.rs` drives the router via
`tower::ServiceExt::oneshot`; `tests/hook_subcommand.rs` binds
`127.0.0.1:0` and drives `hook::run_with_input` against the real socket;
`tests/m1_vertical_slice.rs` and `tests/progress_streaming.rs` call
`run_with_bridge_on` with a `pocket_veto_bt::mock::mock_pair()` transport, exercising
the full hook -> server -> BtBridge -> MockPeer pipeline with no radio.

## The bridge-outbox pattern

The HTTP layer is decoupled from the Bluetooth transport by a
`tokio::mpsc::channel::<ServerMessage>(256)`. `bridge_tx` lives in
`AppState` and is cloned into every handler; `bridge_rx` lives in
`ServerParts` and is handed to `BtBridge::new` (or `bridge_drainer` in
headless `run`).

The single chokepoint is `forward_to_bridge(tx, msg)`: it does a non-blocking
`try_send` first, then falls back to an async `send` so a briefly-full channel
does not drop the message. If even the async send fails the receiver is gone
(server shutting down), which is logged and swallowed — the HTTP layer must
never block on the bridge.

`handle_events` forwards up to three frames per request, in order:
`AgentStart` (if newly announced) -> `AgentEvent` (always) -> `AgentEnd` (if
`kind == "agent_end"`). `handle_create_approval` forwards an
`ApprovalRequest`. This is the only path frames take from the server to the
phone.

## Hook and init testability

- `impl Subcommand for HookArgs { async fn run(...) }` is the thin
  stdin/config/stdout/exit-code wrapper. `hook::run_with_input(event,
  config, client)` is the testable core: it takes an already-normalized
  `InternalEvent`, a `Config`, and a `reqwest::Client`, and returns a
  `HookOutcome` enum that carries the pre-serialized stdout JSON so the
  decision logic is unit-testable without touching stdout. `EXIT_DENY = 2`
  and `EXIT_OK = 0` are the public constants; `HookOutcome: Termination`
  maps them to `ExitCode` so `run` hands the code straight back to `main`
  with no `process::exit`. The `/approvals/:id/wait` outcome is the typed
  `WaitOutcome` enum (`Approved(Decision)` / `Timeout`) (Rule 4: no
  stringly-typed dispatch); `Timeout` maps to `Decision::Deny` (fail-closed).
- `impl Subcommand for InitArgs { async fn run(...) }` is the thin
  interactive wrapper. Every file-writing step delegates to a pure function
  that takes explicit paths and does no stdin/stdout I/O. `InitOpts`
  carries the resolved options.

When you add a new subcommand or a new field to `AppState`, follow the same
pattern: keep `main.rs` thin, put the logic in a library module, and add a
`run_with_*` entry point that lets tests inject fakes.
