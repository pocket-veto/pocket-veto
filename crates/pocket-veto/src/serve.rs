//! `pocket-veto serve` — the axum HTTP server.
//!
//! Implements the server routes:
//!
//! | Route                          | Method | Purpose                                  |
//! | ------------------------------ | ------ | ---------------------------------------- |
//! | `POST /events`                 | POST   | hook subcommand fire-and-forget          |
//! | `POST /approvals`              | POST   | create a pending approval request        |
//! | `GET /approvals/{id}/wait`     | GET    | long-poll for a decision                 |
//! | `POST /approvals/{id}/decide`  | POST   | resolve a pending approval               |
//! | `GET /stream`                  | WS     | optional local browser dashboard stream  |
//! | `GET /agents`                  | GET    | list known agents                        |
//! | `GET /agents/{id}/history`     | GET    | replay events for an agent from `SQLite` |
//! | `GET /health`                  | GET    | liveness + BT status                     |
//!
//! (Path captures use axum 0.8's `{id}` syntax, not the pre-0.8 `:id`.)
//!
//! Bearer-token auth gates every route except `/health`. Approvals coordinate
//! across `POST /approvals`, `GET /wait`, and `POST /decide` through
//! [`pocket_veto_core::approvals::ApprovalWaiters`] (which owns the oneshot senders)
//! and an in-`AppState` map of pending receivers (so the receiver created in
//! `POST /approvals` can be awaited by `GET /wait`). See the module-level
//! notes on [`AppState::pending_receivers`] for the rationale.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum::Json;
use axum::extract::{
    Path, Query, State,
    ws::{Message, WebSocket, WebSocketUpgrade},
};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Router, extract::FromRequestParts};
use pocket_veto_bt::bridge::{BtBridge, BtTransport};
use pocket_veto_core::approvals::ApprovalWaiters;
use pocket_veto_core::config::Config;
use pocket_veto_core::db::Db;
use pocket_veto_core::error::CoreError;
use pocket_veto_core::events::{EventBus, EventMessage};
use pocket_veto_core::protocol::{AgentStatus, Decision, EventKind, Host, ServerMessage};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::cli::{Ctx, ServeArgs, Subcommand};

/// Capacity of the in-process event broadcast bus. Generous enough to absorb
/// a burst of hook events while a subscriber (the BT bridge or a WebSocket
/// dashboard) is briefly slow.
const EVENT_BUS_CAPACITY: usize = 256;

/// Capacity of the bridge-outbox mpsc channel. The server publishes
/// [`ServerMessage`]s here; the BT bridge (or, in headless `run`, the
/// bridge drainer) consumes them.
const BRIDGE_OUTBOX_CAPACITY: usize = 256;

/// Shared application state, cloned cheaply into every handler. All the heavy
/// fields live behind `Arc` so cloning the state is just bumping refcounts.
///
/// Approval coordination: [`ApprovalWaiters`] owns the `oneshot::Sender`s
/// (created by `register` in `POST /approvals`). The matching receivers are
/// stashed in [`AppState::pending_receivers`] and taken out by `GET /wait`.
/// This split keeps `pocket-veto-core` untouched while letting the two HTTP
/// handlers share the channel across requests.
///
/// Agent lifecycle: [`AppState::announced_agents`] tracks the set of
/// `agent_id`s that have already emitted a [`ServerMessage::AgentStart`] this
/// server lifetime. The first event for a brand-new agent triggers an
/// `AgentStart` to the bridge outbox before the `AgentEvent`; subsequent
/// events for the same agent only forward `AgentEvent`. This guarantees the
/// phone learns the agent's `name/host/workspace/started_at` exactly once per
/// session, even if the hook sends multiple `SessionStart` events.
///
/// [`pending_receivers`]: AppState::pending_receivers
/// [`announced_agents`]: AppState::announced_agents
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub bus: EventBus,
    pub waiters: Arc<ApprovalWaiters>,
    pub config: Arc<Config>,
    /// Outbox for the BT bridge task. The server pushes [`ServerMessage`]s
    /// here for the bridge drainer (headless `run`) or a real [`BtBridge`]
    /// ([`run_with_bridge_on`]) to consume.
    pub bridge_tx: mpsc::Sender<ServerMessage>,
    /// Pending approval oneshot receivers, keyed by approval id. `POST
    /// /approvals` inserts; `GET /approvals/:id/wait` removes and awaits.
    pub pending_receivers: Arc<Mutex<HashMap<String, oneshot::Receiver<Decision>>>>,
    /// Agent ids that have already emitted an `AgentStart` this server
    /// lifetime. The first `POST /events` for a new `agent_id` emits an
    /// `AgentStart` to the bridge outbox before the `AgentEvent`; later
    /// events for the same agent only forward `AgentEvent`. Scoped to a
    /// single server process (not persisted) — on a server restart the
    /// phone re-learns each agent on its next event.
    pub announced_agents: Arc<Mutex<HashSet<String>>>,
}

impl AppState {
    /// Construct state from its components. Public so tests can build a state
    /// without spinning up the full `run` wiring.
    #[must_use]
    pub fn new(
        db: Arc<Db>,
        bus: EventBus,
        waiters: Arc<ApprovalWaiters>,
        config: Arc<Config>,
        bridge_tx: mpsc::Sender<ServerMessage>,
        pending_receivers: Arc<Mutex<HashMap<String, oneshot::Receiver<Decision>>>>,
        announced_agents: Arc<Mutex<HashSet<String>>>,
    ) -> Self {
        Self {
            db,
            bus,
            waiters,
            config,
            bridge_tx,
            pending_receivers,
            announced_agents,
        }
    }
}

impl Subcommand for ServeArgs {
    async fn run(&self, _ctx: &Ctx) -> anyhow::Result<ExitCode> {
        let config = Config::config_path()
            .and_then(|p| Config::load(&p))
            .context("load ~/.pocket-veto/config.toml (run `pocket-veto init` first)")?;
        run(config).await?;
        Ok(ExitCode::SUCCESS)
    }
}

/// Entry point invoked from `main` for `pocket-veto serve` (production).
///
/// This is the **headless** path: it spawns the bridge drainer for the bridge
/// outbox, which logs and drops every [`ServerMessage`]. The server still
/// works end-to-end for the HTTP API (events are persisted to `SQLite`, the
/// `/stream` WebSocket works, approvals round-trip via `POST /decide`), but
/// nothing is forwarded over Bluetooth.
///
/// The production Bluetooth transport (Linux `bluer` RFCOMM, Windows
/// `serialport` COM port) is not yet wired — wiring it requires
/// platform-specific BT APIs (`bluer` needs `libdbus-1-dev`; `serialport`
/// needs a paired SPP COM port) that are not available in the devcontainer or
/// in CI. `run` keeps the server fully functional without a radio.
///
/// The testable, Bluetooth-wired path is [`run_with_bridge`] /
/// [`run_with_bridge_on`], which integration tests use with
/// `pocket_veto_bt::mock` to exercise the hook -> server -> bridge -> "phone"
/// pipeline in CI without any radio.
///
/// # Errors
///
/// Returns `anyhow::Error` if the database cannot be opened, the bind address
/// cannot be parsed/bound, or the server returns an error from `axum::serve`.
pub async fn run(config: Config) -> anyhow::Result<()> {
    let parts = build_state(&config)?;
    // Drain the bridge outbox: the production BT transport is not wired yet,
    // so each message is logged and dropped. This keeps the channel from
    // back-pressuring the HTTP layer and lets the server run headless.
    tokio::spawn(bridge_drainer(parts.bridge_rx));

    let listener = TcpListener::bind(&config.bind_addr)
        .await
        .with_context(|| format!("bind {}", config.bind_addr))?;
    serve_loop(parts.state, listener).await
}

/// Testable / Bluetooth-wired entry point: same as [`run`] but spawns the real
/// [`BtBridge`] over `transport` instead of the bridge drainer.
///
/// Binds the `TcpListener` from `config.bind_addr` and delegates to
/// [`run_with_bridge_on`]. Use this from tests that construct a transport
/// directly (e.g. integration tests with a `pocket_veto_bt::mock` pair) when
/// a specific bound address is not needed.
///
/// # Errors
///
/// Propagates any error from `build_state`, `TcpListener::bind`, or
/// `serve_loop`.
pub async fn run_with_bridge<T: BtTransport + Sync + 'static>(
    config: Config,
    transport: T,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&config.bind_addr)
        .await
        .with_context(|| format!("bind {}", config.bind_addr))?;
    run_with_bridge_on(config, transport, listener).await
}

/// Like [`run_with_bridge`] but accepts a pre-bound `TcpListener`.
///
/// This is the form integration tests use: it binds `127.0.0.1:0` to get an
/// ephemeral port, reads the bound address (so the hook's `config.server_url`
/// can be set correctly), then spawns
/// `run_with_bridge_on(config, transport, listener)` in a `tokio` task.
///
/// The bridge task runs for the lifetime of the server. Its errors are logged
/// but do **not** tear down the HTTP server: the bridge's own reconnect loop
/// handles transport failures, and a fatal bridge exit (outbox closed) only
/// happens when the server is already shutting down.
///
/// The `T: Send + Sync + 'static` bounds are required so the
/// [`BtBridge<T>`] can be moved into a `tokio::spawn` task whose future is
/// `Send + 'static`. `Sync` is needed because the method futures hold shared
/// references to the transport across `await` points inside the bridge's
/// `tokio::select!` loop.
///
/// # Errors
///
/// Returns `anyhow::Error` if `build_state` fails.
pub async fn run_with_bridge_on<T: BtTransport + Sync + 'static>(
    config: Config,
    transport: T,
    listener: TcpListener,
) -> anyhow::Result<()> {
    let parts = build_state(&config)?;

    // Spawn the real bridge. It owns `bridge_rx` (consumed by `run`), and
    // shares the `Arc<Db>` and `Arc<ApprovalWaiters>` with the HTTP layer so
    // approval decisions delivered over the transport resolve the waiters the
    // hook is blocked on in `GET /approvals/:id/wait`.
    tokio::spawn(async move {
        let mut bridge = BtBridge::new(transport, parts.bridge_rx, parts.waiters, parts.db);
        if let Err(e) = bridge.run().await {
            error!(error = %e, "bt bridge task exited with error");
        }
    });

    serve_loop(parts.state, listener).await
}

/// Build the shared [`AppState`] plus the bridge-outbox receiver and the
/// `Arc` handles the bridge task needs.
///
/// Factored out of `run` and `run_with_bridge_on` so the two entry points do
/// not duplicate the Db / bus / waiters / channel setup. Returns a
/// `ServerParts` holding:
///
/// - `state` — the [`AppState`] to hand to [`build_router`].
/// - `bridge_rx` — the receiving end of the bridge outbox. The caller decides
///   whether to drain it (the bridge drainer, for headless `run`) or feed it
///   to a real [`BtBridge`] (`run_with_bridge_on`).
/// - `db` — the `Arc<Db>` shared with the bridge so it can replay events and
///   persist approval decisions.
/// - `waiters` — the `Arc<ApprovalWaiters>` shared with the bridge so it can
///   resolve pending approvals.
///
/// # Errors
///
/// Returns `anyhow::Error` if the database cannot be opened.
fn build_state(config: &Config) -> anyhow::Result<ServerParts> {
    let db_path = expand_tilde(&config.db_path);
    info!(db_path = %db_path.display(), "opening database");
    let db = Db::open(&db_path).with_context(|| format!("open db at {}", db_path.display()))?;
    let db = Arc::new(db);

    let bus = EventBus::new(EVENT_BUS_CAPACITY);
    let waiters = Arc::new(ApprovalWaiters::new());
    let (bridge_tx, bridge_rx) = mpsc::channel::<ServerMessage>(BRIDGE_OUTBOX_CAPACITY);
    let pending_receivers = Arc::new(Mutex::new(HashMap::new()));
    let announced_agents = Arc::new(Mutex::new(HashSet::new()));

    let state = AppState::new(
        Arc::clone(&db),
        bus,
        Arc::clone(&waiters),
        Arc::new(config.clone()),
        bridge_tx,
        Arc::clone(&pending_receivers),
        Arc::clone(&announced_agents),
    );

    Ok(ServerParts {
        state,
        bridge_rx,
        db,
        waiters,
    })
}

/// The pieces `build_state` hands back: the [`AppState`] for the router, plus
/// the bridge-outbox receiver and shared `Arc` handles the bridge task needs.
/// Wrapped in a struct (rather than a tuple) so the call sites are
/// self-documenting and clippy's `type_complexity` lint stays happy.
struct ServerParts {
    state: AppState,
    bridge_rx: mpsc::Receiver<ServerMessage>,
    db: Arc<Db>,
    waiters: Arc<ApprovalWaiters>,
}

/// Serve the router on an already-bound `TcpListener` with graceful shutdown
/// on Ctrl-C (unix). The bridge task — whatever the caller spawned in
/// `build_state`'s caller — runs independently for the lifetime of the
/// server.
///
/// # Errors
///
/// Returns `anyhow::Error` if `axum::serve` returns an error.
async fn serve_loop(state: AppState, listener: TcpListener) -> anyhow::Result<()> {
    let bind_addr = listener
        .local_addr()
        .map_or_else(|_| "<unknown>".to_string(), |a| a.to_string());
    info!(bind_addr = %bind_addr, "pocket-veto serve listening");
    let router = build_router(state);
    let server = axum::serve(listener, router);
    #[cfg(unix)]
    let server = server.with_graceful_shutdown(async {
        if tokio::signal::ctrl_c().await.is_err() {
            warn!("ctrl_c signal handler returned error; graceful shutdown disabled");
        }
        info!("ctrl-c received, shutting down");
    });

    server.await.context("axum::serve")?;
    Ok(())
}

/// Dummy consumer for the bridge outbox, used by the headless `run` entry
/// point. Logs each [`ServerMessage`] at `debug` level so the pipeline is
/// observable and the channel never back-pressures the HTTP layer. The
/// Bluetooth-wired path ([`run_with_bridge_on`]) replaces this with the real
/// [`BtBridge`].
async fn bridge_drainer(mut rx: mpsc::Receiver<ServerMessage>) {
    while let Some(msg) = rx.recv().await {
        debug!(?msg, "bridge outbox (no bridge wired yet)");
    }
    debug!("bridge outbox closed");
}

/// Build the axum router with all routes and bearer-token auth applied to
/// every route except `/health`. Public so integration tests can build a
/// router against an in-memory [`Db`] and a known token.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/events", post(handle_events))
        .route("/approvals", post(handle_create_approval))
        .route("/approvals/{id}/wait", get(handle_wait_approval))
        .route("/approvals/{id}/decide", post(handle_decide_approval))
        .route("/stream", get(handle_stream))
        .route("/agents", get(handle_list_agents))
        .route("/agents/{id}/history", get(handle_agent_history))
        .route("/health", get(handle_health))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Bearer-token auth extractor
// ---------------------------------------------------------------------------

/// Extractor that validates an `Authorization: Bearer <token>` header against
/// `state.config.token`. Add it as the first parameter of any protected
/// handler. Missing or mismatched tokens yield `401 Unauthorized`.
#[derive(Debug, Clone, Copy)]
pub struct BearerAuth;

impl FromRequestParts<AppState> for BearerAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let headers = &parts.headers;
        let token = extract_bearer(headers).ok_or(ApiError::Unauthorized)?;
        if token.as_str() == state.config.token.as_ref() {
            Ok(BearerAuth)
        } else {
            Err(ApiError::Unauthorized)
        }
    }
}

/// Pull the bearer token out of an `Authorization: Bearer <token>` header.
fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?;
    let s = value.to_str().ok()?;
    let token = s
        .strip_prefix("Bearer ")
        .or_else(|| s.strip_prefix("bearer "))?;
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

// ---------------------------------------------------------------------------
// API error type — structured (no stringly-typed errors)
// ---------------------------------------------------------------------------

/// Errors returned by HTTP handlers. Each variant carries structured context
/// (no `String` messages). Maps to a status code and a JSON body
/// `{"error": "..."}` via [`IntoResponse`]; the message is the variant's
/// `Display` (derived by `thiserror`).
///
/// HTTP status mapping (the `serve_api` tests assert these bodies, in
/// particular `"unauthorized"` for 401):
/// - [`ApiError::Unauthorized`] -> 401 `{"error":"unauthorized"}`.
/// - [`ApiError::NotFound`] -> 404 `{"error":"not found"}`.
/// - [`ApiError::InvalidBody`] / [`ApiError::UnknownDecision`] ->
///   400 `{"error":"<detail>"}`.
/// - [`ApiError::PendingReceiversPoisoned`] / [`ApiError::Internal`] -> 500
///   `{"error":"internal error: <detail>"}` (also logged via `tracing`).
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// `Authorization` header missing or did not match `config.token`.
    #[error("unauthorized")]
    Unauthorized,
    /// The targeted approval / agent does not exist.
    #[error("not found")]
    NotFound,
    /// A JSON request body could not be deserialized into the expected shape
    /// (missing required field, wrong type, etc.). Carries the underlying
    /// serde error so the 400 body names the offending field.
    #[error("invalid body: {0}")]
    InvalidBody(#[from] serde_json::Error),
    /// `POST /approvals/:id/decide` carried a decision string that is not a
    /// known `Decision` wire tag. Carries the offending value so the 400 body
    /// echoes it back.
    #[error("unknown decision `{0}`")]
    UnknownDecision(String),
    /// The `pending_receivers` mutex was poisoned by a prior handler panic.
    /// Surfaced as 500 so the hook fails closed.
    #[error("pending receivers poisoned")]
    PendingReceiversPoisoned,
    /// A `pocket-veto-core` operation (Db, etc.) failed. Carries the
    /// structured [`CoreError`] so the error kind is preserved end-to-end;
    /// the 500 body is the error's `Display`.
    #[error("internal error: {0}")]
    Internal(#[from] CoreError),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, self.to_string()),
            Self::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            Self::InvalidBody(_) | Self::UnknownDecision(_) => {
                (StatusCode::BAD_REQUEST, self.to_string())
            }
            // Internal-class errors are logged before they become a 500 body.
            Self::PendingReceiversPoisoned | Self::Internal(_) => {
                error!(error = %self, "internal server error");
                (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
            }
        };
        let body = Json(json!({ "error": message }));
        (status, body).into_response()
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /events` — fire-and-forget agent event from the hook subcommand.
///
/// The body is parsed leniently into an [`EventPayload`] (typed
/// `#[derive(Deserialize)]` with sensible defaults; `agent_id` is the only
/// required field). The handler then persists an event row, upserts the
/// agent, publishes on the event bus, and forwards [`ServerMessage`]s to
/// the bridge outbox.
///
/// Lifecycle frames:
/// - On the **first** event for a given `agent_id` this server lifetime
///   (tracked in [`AppState::announced_agents`]), an
///   [`ServerMessage::AgentStart`] is emitted before the `AgentEvent` so the
///   phone learns the agent's `name/host/workspace/started_at`. Subsequent
///   events for the same agent only forward `AgentEvent`.
/// - When `kind == "agent_end"` (set by the hook for `Stop`/`SessionEnd`),
///   the agent row is marked `completed` with `ended_at = ts` and an
///   [`ServerMessage::AgentEnd`] is emitted after the `AgentEvent` so the
///   phone stops the elapsed-time clock and renders the card as finished.
async fn handle_events(
    _auth: BearerAuth,
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    // Parse the body via the typed `EventPayload` deserializer. A missing
    // `agent_id` (the one required field) surfaces as a 400 with the serde
    // error naming the field.
    let ev: EventPayload = serde_json::from_value(body).map_err(ApiError::from)?;
    // Unknown hosts fall back to `Claude` (the dev environment default) so an
    // unrecognized host tag never blocks the event.
    let host_enum = Host::from_db_str(&ev.host).unwrap_or(Host::Claude);
    // An `agent_end` event (Stop / SessionEnd) marks the session completed.
    let is_agent_end = ev.kind == "agent_end";

    // Decide whether to emit an AgentStart for this agent. Done before
    // the db upsert so a poisoned mutex (treated as already-announced) does
    // not block the event itself. `unwrap_or_else(into_inner)` keeps the set
    // usable even if a prior handler panicked while holding the lock.
    let announce_start = should_announce(&state.announced_agents, &ev.agent_id);

    // Upsert the agent row. On `agent_end` the terminal status and
    // `ended_at` are recorded; otherwise the agent is (re)marked running
    // with no end.
    let (db_status, db_ended_at) = if is_agent_end {
        (AgentStatus::Completed, Some(ev.ts))
    } else {
        (AgentStatus::Running, None)
    };
    state
        .db
        .upsert_agent(
            &ev.agent_id,
            &ev.session_id,
            &ev.host,
            &ev.name,
            &ev.workspace,
            db_status,
            ev.ts,
            db_ended_at,
        )
        .map_err(ApiError::from)?;

    state
        .db
        .record_event(
            &ev.agent_id,
            &ev.kind,
            ev.tool.as_deref(),
            &ev.payload,
            ev.ts,
        )
        .map_err(ApiError::from)?;

    // Unknown kinds fall back to `ToolCall` (the most common kind) so a
    // forward-compatible hook payload still produces a useful event row. The
    // `agent_start` / `agent_end` kinds also map to `ToolCall` here — their
    // lifecycle signal rides the surrounding `AgentStart` / `AgentEnd`
    // frames, not the `AgentEvent.kind`.
    let event_kind = EventKind::from_db_str(&ev.kind).unwrap_or(EventKind::ToolCall);
    state.bus.publish(EventMessage {
        agent_id: ev.agent_id.clone(),
        kind: event_kind,
        tool: ev.tool.clone(),
        payload: ev.payload.clone(),
        ts: ev.ts,
    });

    // Emit AgentStart first (only on the first event for this agent this
    // server lifetime). The phone uses this to populate the card's
    // name/host/workspace/startedAt.
    if announce_start {
        let start_msg = ServerMessage::AgentStart {
            agent_id: ev.agent_id.clone(),
            session_id: ev.session_id.clone(),
            host: host_enum,
            name: ev.name.clone(),
            workspace: ev.workspace.clone(),
            started_at: ev.ts,
        };
        forward_to_bridge(&state.bridge_tx, start_msg).await;
    }

    // Then the AgentEvent (always).
    let server_msg = ServerMessage::AgentEvent {
        agent_id: ev.agent_id.clone(),
        kind: event_kind,
        tool: ev.tool,
        payload: ev.payload,
        ts: ev.ts,
    };
    forward_to_bridge(&state.bridge_tx, server_msg).await;

    // Finally, an AgentEnd marker on session-end events. The phone receives
    // the last AgentEvent then the end frame, so the card's transcript shows
    // the final event before flipping to the completed state.
    if is_agent_end {
        let end_msg = ServerMessage::AgentEnd {
            agent_id: ev.agent_id,
            ended_at: ev.ts,
            status: AgentStatus::Completed,
        };
        forward_to_bridge(&state.bridge_tx, end_msg).await;
    }

    Ok(Json(json!({ "ok": true })))
}

/// The fields parsed from a `POST /events` body, as a typed
/// `#[derive(Deserialize)]` struct (no stringly-typed parsing).
///
/// Defaults:
/// - `agent_id` is the only required field (missing -> 400).
/// - `kind` defaults to `"tool_call"` (the most common kind).
/// - `host` defaults to `"claude"` (the dev environment default).
/// - `tool` / `name` default to `None` / `""`.
/// - `payload` defaults to `Value::Null`.
/// - `ts` defaults to `crate::now_ms()` (the shared timestamp helper).
/// - `workspace` accepts either a bare string or `{ "cwd": "..." }` and
///   defaults to `""`.
///
/// A field present with the wrong type (e.g. `kind` as a number) yields a
/// 400 from serde rather than silently defaulting; the hook always sends
/// correctly-typed fields.
#[derive(Deserialize, Debug)]
struct EventPayload {
    agent_id: String,
    #[serde(default = "default_kind")]
    kind: String,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default = "default_null")]
    payload: Value,
    #[serde(default = "crate::now_ms")]
    ts: i64,
    #[serde(default)]
    session_id: String,
    #[serde(default = "default_host")]
    host: String,
    #[serde(default)]
    name: String,
    #[serde(default, deserialize_with = "deserialize_workspace")]
    workspace: String,
}

/// Default `kind` when the field is absent: `"tool_call"` (most common).
fn default_kind() -> String {
    "tool_call".to_string()
}

/// Default `host` when the field is absent: `"claude"` (dev environment
/// default).
fn default_host() -> String {
    "claude".to_string()
}

/// Default `payload` when the field is absent: JSON null.
fn default_null() -> Value {
    Value::Null
}

/// Deserialize the `workspace` field leniently: accept either a bare string
/// (`"workspace"`) or an object `{ "cwd": "..." }`, defaulting to `""` for
/// any other shape.
fn deserialize_workspace<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<Value>::deserialize(deserializer)?;
    Ok(match opt {
        Some(Value::String(s)) => s,
        Some(Value::Object(map)) => map
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        // null / None / numbers / bools / arrays: not a usable workspace path.
        _ => String::new(),
    })
}

/// Atomically check whether `agent_id` is newly seen and, if so, record it
/// in the `announced_agents` set. Returns `true` when the caller should
/// emit an `AgentStart` (first event for this agent this server lifetime).
/// A poisoned mutex is recovered via `into_inner` so a prior panic never
/// blocks the event itself.
fn should_announce(announced: &Mutex<HashSet<String>>, agent_id: &str) -> bool {
    let mut set = announced
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if set.contains(agent_id) {
        false
    } else {
        set.insert(agent_id.to_string());
        true
    }
}

/// `POST /approvals` — create a pending approval and block the hook via
/// `GET /wait` later.
///
/// Body: `{ agent_id, tool, summary, detail? }`. Generates a UUID v4
/// approval id, inserts the row with status `pending`, flips the agent to
/// `awaiting_approval`, registers an [`ApprovalWaiters`] waiter (stashing
/// the receiver in [`AppState::pending_receivers`]), and forwards an
/// [`ServerMessage::ApprovalRequest`] to the bridge outbox.
async fn handle_create_approval(
    _auth: BearerAuth,
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    #[derive(Deserialize)]
    struct CreateApproval {
        agent_id: String,
        tool: String,
        summary: String,
        #[serde(default)]
        detail: Option<String>,
    }
    let req: CreateApproval = serde_json::from_value(body).map_err(ApiError::from)?;

    let approval_id = uuid::Uuid::new_v4().to_string();
    let now = crate::now_ms();

    state
        .db
        .insert_approval(
            &approval_id,
            &req.agent_id,
            &req.tool,
            &req.summary,
            req.detail.as_deref(),
            now,
        )
        .map_err(ApiError::from)?;

    // Mark the agent as awaiting approval. The upsert uses the same id; if
    // the agent row does not yet exist, this creates one with a placeholder
    // session/workspace so the FK on the approvals table is satisfied.
    state
        .db
        .upsert_agent(
            &req.agent_id,
            "",
            "claude",
            "",
            "",
            AgentStatus::AwaitingApproval,
            now,
            None,
        )
        .map_err(ApiError::from)?;

    // Register the waiter (ApprovalWaiters holds the sender) and stash the
    // receiver for GET /wait to pick up.
    let receiver = state.waiters.register(&approval_id);
    if let Ok(mut map) = state.pending_receivers.lock() {
        map.insert(approval_id.clone(), receiver);
    } else {
        // Poisoned mutex; treat as internal error so the hook fails closed.
        return Err(ApiError::PendingReceiversPoisoned);
    }

    let timeout_at = now
        .saturating_add(i64::try_from(state.config.approval_timeout_seconds * 1000).unwrap_or(0));
    let server_msg = ServerMessage::ApprovalRequest {
        approval_id: approval_id.clone(),
        agent_id: req.agent_id,
        tool: req.tool,
        summary: req.summary,
        detail: req.detail.unwrap_or_default(),
        timeout_at,
    };
    forward_to_bridge(&state.bridge_tx, server_msg).await;

    Ok(Json(json!({ "approval_id": approval_id })))
}

/// `GET /approvals/:id/wait?timeout=<secs>` — long-poll for a decision.
///
/// Takes the stashed receiver from [`AppState::pending_receivers`] and awaits
/// it under `tokio::time::timeout`. On a decision, returns
/// `{ decision, note? }`. On timeout, marks the approval `timeout` in the
/// DB and returns `{ decision: "timeout" }` (NOT an error — the hook treats
/// timeout as deny). On a missing approval / missing receiver, returns 404.
async fn handle_wait_approval(
    _auth: BearerAuth,
    State(state): State<AppState>,
    Path(approval_id): Path<String>,
    Query(query): Query<WaitQuery>,
) -> Result<Json<Value>, ApiError> {
    let timeout_secs = query
        .timeout
        .unwrap_or(state.config.approval_timeout_seconds);

    // Confirm the approval exists in the DB before doing anything else.
    let row = state
        .db
        .pending_approval(&approval_id)
        .map_err(ApiError::from)?;
    if row.is_none() {
        return Err(ApiError::NotFound);
    }

    // Take the receiver that POST /approvals stashed. Awaiting it is
    // preferred over reading the DB status, because the receiver carries the
    // canonical wire `Decision` even when /decide was called *before* /wait
    // (the value is buffered in the channel). If the receiver is already
    // gone (a prior /wait consumed it, or it was cancelled), fall back to
    // the DB row.
    let receiver = {
        let mut map = state
            .pending_receivers
            .lock()
            .map_err(|_e| ApiError::PendingReceiversPoisoned)?;
        map.remove(&approval_id)
    };

    if let Some(mut receiver) = receiver {
        let duration = std::time::Duration::from_secs(timeout_secs);
        return match tokio::time::timeout(duration, &mut receiver).await {
            Ok(Ok(decision)) => {
                // The wire decision string comes straight from the core
                // enum's `to_db_str` (no hand-rolled string mapping).
                let decision_str = decision.to_db_str();
                // The oneshot carries only the `Decision` enum (per
                // `ApprovalWaiters`'s design), not the note. The decider
                // (the BT bridge's `route_client_message`, or
                // `handle_decide_approval`) writes the full row — including
                // the note — to the Db BEFORE firing the oneshot, so re-read
                // the row here to surface the note. Best-effort: a missing
                // row yields a null note rather than an error, since the
                // decision itself is already authoritative.
                let note = state
                    .db
                    .pending_approval(&approval_id)
                    .ok()
                    .flatten()
                    .and_then(|r| r.decision_note)
                    .map_or(Value::Null, Value::from);
                Ok(Json(json!({
                    "decision": decision_str,
                    "note": note,
                })))
            }
            Ok(Err(_)) => {
                // Receiver closed without a value (waiter cancelled or
                // replaced). Treat as a timeout so the hook fails closed.
                mark_timeout(&state, &approval_id);
                Ok(Json(json!({ "decision": "timeout" })))
            }
            Err(_) => {
                mark_timeout(&state, &approval_id);
                state.waiters.cancel(&approval_id);
                Ok(Json(json!({ "decision": "timeout" })))
            }
        };
    }

    // No receiver: the approval was already decided (and the receiver
    // consumed by a prior /wait, or /decide ran without a registered waiter).
    // Surface the persisted status, mapped to a wire decision string. There
    // is no `Decision::from_approval_status` helper (adding one would touch
    // `protocol.rs` + Android `Protocol.kt` + a BT mock round-trip), so
    // iterate the variants and match on `to_approval_status()`. Unknown /
    // pending / timeout statuses surface as `"timeout"` (fail-closed).
    //
    // `row` was checked for `None` above (returning 404); the let-else keeps
    // the infallible case `expect`-free while still satisfying the borrow
    // checker.
    let Some(row) = row else {
        return Err(ApiError::NotFound);
    };
    let decision = [
        Decision::Allow,
        Decision::Deny,
        Decision::Ask,
        Decision::Defer,
    ]
    .into_iter()
    .find(|d| d.to_approval_status() == row.status.as_str())
    .map_or("timeout", Decision::to_db_str);
    Ok(Json(json!({
        "decision": decision,
        "note": row.decision_note,
    })))
}

/// `POST /approvals/:id/decide` — resolve a pending approval.
///
/// Body: `{ decision: "allow"|"deny"|"ask"|"defer", note? }`. Fires the
/// oneshot via [`ApprovalWaiters::resolve`], updates the DB row, flips the
/// agent back to `running`, and returns `{ ok: true }`. 404 if the approval
/// does not exist.
async fn handle_decide_approval(
    _auth: BearerAuth,
    State(state): State<AppState>,
    Path(approval_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    #[derive(Deserialize)]
    struct Decide {
        decision: String,
        #[serde(default)]
        note: Option<String>,
    }
    let req: Decide = serde_json::from_value(body).map_err(ApiError::from)?;

    // Parse the wire decision string via the core `Decision` FromStr (no
    // hand-rolled string->enum mapping). An unknown string is a 400
    // echoing the offending value back.
    let decision = req
        .decision
        .parse::<Decision>()
        .map_err(|_e| ApiError::UnknownDecision(req.decision.clone()))?;

    let row = state
        .db
        .pending_approval(&approval_id)
        .map_err(ApiError::from)?;
    let agent_id = row.ok_or(ApiError::NotFound)?.agent_id;

    let status = decision.to_approval_status();
    let now = crate::now_ms();
    state
        .db
        .set_approval_decision(&approval_id, status, req.note.as_deref(), now)
        .map_err(ApiError::from)?;

    // Flip the agent back to running. Best-effort: a missing agent row is not
    // an error here, since POST /approvals may have created a placeholder.
    let _agent = state.db.upsert_agent(
        agent_id.as_ref(),
        "",
        "claude",
        "",
        "",
        AgentStatus::Running,
        now,
        None,
    );

    // Fire the oneshot so GET /wait unblocks. If no waiter exists (already
    // timed out or never registered), this is a no-op.
    state.waiters.resolve(&approval_id, decision);

    Ok(Json(json!({ "ok": true })))
}

/// `GET /stream` — WebSocket upgrade. Subscribes to the event bus and
/// forwards each [`EventMessage`] as a JSON text frame until the client
/// disconnects. This is the optional local browser dashboard endpoint.
async fn handle_stream(
    _auth: BearerAuth,
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> Response {
    let bus = state.bus.clone();
    ws.on_upgrade(move |socket| stream_socket(socket, bus))
}

/// Drive a `/stream` WebSocket: read bus events and write them as text.
///
/// Each [`EventMessage`] is converted to a [`ServerMessage::AgentEvent`] — the
/// same frame the BT bridge sends the phone — so a browser dashboard sees a
/// consistent shape regardless of transport.
async fn stream_socket(mut socket: WebSocket, bus: EventBus) {
    let mut rx = bus.subscribe();
    while let Ok(msg) = rx.recv().await {
        let frame = ServerMessage::AgentEvent {
            agent_id: msg.agent_id,
            kind: msg.kind,
            tool: msg.tool,
            payload: msg.payload,
            ts: msg.ts,
        };
        let json = match serde_json::to_string(&frame) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "encode event for /stream failed");
                continue;
            }
        };
        if socket.send(Message::Text(json.into())).await.is_err() {
            // Client disconnected.
            return;
        }
    }
    // Bus closed (server shutting down). Close the socket cleanly.
    let _close = socket.send(Message::Close(None)).await;
}

/// `GET /agents` — list all known agents, ordered by `started_at`.
async fn handle_list_agents(
    _auth: BearerAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    let agents = state.db.list_agents().map_err(ApiError::from)?;
    let rows: Vec<Value> = agents
        .into_iter()
        .map(|a| {
            json!({
                "agent_id": a.agent_id,
                "session_id": a.session_id,
                "host": a.host,
                "name": a.name,
                "workspace": a.workspace,
                "status": a.status,
                "started_at": a.started_at,
                "ended_at": a.ended_at,
            })
        })
        .collect();
    Ok(Json(Value::Array(rows)))
}

/// `GET /agents/:id/history?since=<ts>` — replay events for an agent.
async fn handle_agent_history(
    _auth: BearerAuth,
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    let since = query.since.unwrap_or(0);
    let events = state
        .db
        .agent_history(&agent_id, since)
        .map_err(ApiError::from)?;
    let rows: Vec<Value> = events
        .into_iter()
        .map(|e| {
            json!({
                "id": e.id,
                "agent_id": e.agent_id,
                "kind": e.kind,
                "tool": e.tool,
                "payload": e.payload,
                "ts": e.ts,
            })
        })
        .collect();
    Ok(Json(Value::Array(rows)))
}

/// `GET /health` — liveness probe. BT status is `unknown` until the bridge
/// is wired in the next todo. No auth required.
async fn handle_health(State(_state): State<AppState>) -> Json<Value> {
    Json(json!({ "status": "ok", "bt": "unknown" }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Query params for `GET /approvals/:id/wait`.
#[derive(Deserialize)]
struct WaitQuery {
    timeout: Option<u64>,
}

/// Query params for `GET /agents/:id/history`.
#[derive(Deserialize)]
struct HistoryQuery {
    since: Option<i64>,
}

/// Expand a leading `~` to the user's home directory. Paths without a leading
/// `~` are returned unchanged. If `dirs::home_dir()` is `None`, the `~` is
/// left as-is (the open will fail with a clear error).
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    if path == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home;
    }
    PathBuf::from(path)
}

/// Mark an approval as timed out in the DB (best-effort; logs on failure).
fn mark_timeout(state: &AppState, approval_id: &str) {
    if let Err(e) = state
        .db
        .set_approval_decision(approval_id, "timeout", None, crate::now_ms())
    {
        error!(error = %e, approval_id, "failed to mark approval timeout");
    }
}

/// Forward a [`ServerMessage`] to the bridge outbox. A full channel is
/// logged but not fatal — the HTTP layer must not block on the bridge.
async fn forward_to_bridge(tx: &mpsc::Sender<ServerMessage>, msg: ServerMessage) {
    if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(msg.clone()) {
        // Fall back to an async send so a briefly-full channel does not drop
        // the message; if even that fails the receiver is gone (server
        // shutting down), which is logged and swallowed.
        if let Err(e) = tx.send(msg).await {
            warn!(error = %e, "bridge outbox send failed");
        }
    }
}
