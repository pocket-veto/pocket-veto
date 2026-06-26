//! Shared test harness for the `pocket-veto` integration tests.
//!
//! Extracted from the `tests/*.rs` files to dedup the copy-pasted constants,
//! `Config` literals, server-spawn helpers, `MockPeer` heartbeat filter, and
//! `reqwest` client builder (no cross-file duplication). Each
//! integration test that needs it declares `mod common;` and imports the items
//! it uses.
//!
//! Edition-2024 module layout: this is `tests/common.rs` (not
//! `tests/common/mod.rs`). cargo still discovers it as a
//! standalone integration-test target, but it declares no `#[test]`s so it
//! reports "0 passed; 0 failed" — harmless noise, accepted (excluding it via
//! `[[test]]` config would be more disruptive than the empty target).
//!
//! `#![allow(dead_code)]` silences the `dead_code` lint for the standalone
//! compilation (where none of the `pub` helpers are referenced from within
//! this file); when a real test `mod common;`-includes it, the items it uses
//! are live and the allow is a no-op for them.

#![allow(dead_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unreachable,
    clippy::unwrap_in_result,
    clippy::indexing_slicing,
    clippy::missing_docs_in_private_items,
    clippy::tests_outside_test_module,
    clippy::wildcard_enum_match_arm,
    clippy::ref_patterns,
    clippy::print_stderr,
    clippy::mem_forget
)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pocket_veto::serve::{AppState, build_router, run_with_bridge_on};
use pocket_veto_bt::mock::{MockPeer, mock_pair};
use pocket_veto_core::approvals::ApprovalWaiters;
use pocket_veto_core::config::BtBackend;
use pocket_veto_core::config::Config;
use pocket_veto_core::db::Db;
use pocket_veto_core::events::EventBus;
use pocket_veto_core::protocol::ServerMessage;
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// Token shared between the test server's bearer auth and the hook / HTTP
/// client. The value is arbitrary as long as server and client agree.
pub const TEST_TOKEN: &str = "test-token-deadbeef";

/// How long to wait for a single `MockPeer` read before declaring the
/// pipeline broken. Generous enough for a healthy localhost path, tight
/// enough to fail a broken test fast.
pub const PEER_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// A `reqwest::Client` mirroring the one `hook::run` builds (5 s timeout).
///
/// # Panics
///
/// Panics if the `reqwest::Client` cannot be built (e.g. TLS backend init
/// failure). Never observed in tests.
#[must_use]
pub fn hook_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("build hook client")
}

/// Build a `Config` with the shared test token and sane defaults
/// (`127.0.0.1:0` bind, in-memory db, 5 s approval timeout, `Bluer` backend,
/// no radio params). Tests clone and override individual fields as needed
/// (e.g. `server_url`, `db_path`, `approval_timeout_seconds`).
#[must_use]
pub fn test_config() -> Config {
    Config {
        server_url: "http://127.0.0.1:0".to_string(),
        bind_addr: "127.0.0.1:0".to_string(),
        token: TEST_TOKEN.to_string().into(),
        db_path: ":memory:".to_string(),
        approval_timeout_seconds: 5,
        bt_backend: BtBackend::Bluer,
        bt_com_port: None,
        bt_adapter_addr: None,
        bt_channel: None,
        devcontainer: false,
    }
}

/// A handle to a running test HTTP server with no Bluetooth bridge. The
/// bridge outbox is exposed via `bridge_rx` so a test can capture forwarded
/// frames. Dropping this does NOT stop the server; each test binds a fresh
/// ephemeral port so port reuse is not a concern.
pub struct TestServer {
    /// The bound ephemeral address.
    pub addr: SocketAddr,
    /// A `Config` whose `server_url` points at the bound port.
    pub config: Config,
    /// The in-memory `Db` so tests can assert against persisted state.
    pub db: Arc<Db>,
    /// Receiver for `ServerMessage`s forwarded to the (absent) bridge.
    pub bridge_rx: mpsc::Receiver<ServerMessage>,
}

/// Spawn a real `axum::serve` on an ephemeral port backed by an in-memory
/// `Db`, with no Bluetooth bridge (the bridge outbox drains into
/// `bridge_rx`). Returns the bound address, a `Config` pointing at it with
/// the test token, and the `Db` handle so tests can assert against persisted
/// state.
///
/// # Panics
///
/// Panics if the in-memory `Db` cannot be opened, the ephemeral `TcpListener`
/// cannot be bound, its local address cannot be read, or the spawned
/// `axum::serve` task returns an error. None of these are expected in tests.
pub async fn spawn_test_server() -> TestServer {
    let db = Arc::new(Db::open_in_memory().expect("open in-memory db"));
    let bus = EventBus::new(64);
    let waiters = Arc::new(ApprovalWaiters::new());
    let mut config = test_config();
    // Keep the approval timeout short so a misconfigured test fails fast
    // instead of hanging for 5 minutes.
    config.approval_timeout_seconds = 3;
    let config = Arc::new(config);
    let (bridge_tx, bridge_rx) = mpsc::channel::<ServerMessage>(64);
    let pending_receivers = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let announced_agents = Arc::new(Mutex::new(std::collections::HashSet::new()));
    let state = AppState::new(
        Arc::clone(&db),
        bus,
        waiters,
        Arc::clone(&config),
        bridge_tx,
        pending_receivers,
        announced_agents,
    );
    let router = build_router(state);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("axum::serve");
    });

    // Fix up the config's server_url to point at the actually-bound port.
    let mut config = (*config).clone();
    config.server_url = format!("http://{addr}");

    TestServer {
        addr,
        config,
        db,
        bridge_rx,
    }
}

/// A handle to a running test server plus the phone-side `MockPeer`. The
/// `MockPeer` is the "phone": it reads `ServerMessage` frames the bridge
/// forwards and writes `ClientMessage` frames back (approval decisions).
/// The server task runs in the background until the process exits; tests are
/// short-lived and each binds a fresh ephemeral port.
pub struct MockServer {
    /// A `Config` whose `server_url` points at the bound port.
    pub config: Config,
    /// The phone-side peer paired with the bridge's `MockTransport`.
    pub peer: MockPeer,
}

/// Spawn a real `axum::serve` on an ephemeral port, wired to a real `BtBridge`
/// driving a `MockTransport` paired with the returned `MockPeer`. The bridge
/// shares the server's `Arc<Db>` and `Arc<ApprovalWaiters>`, so an
/// `ApprovalDecision` frame written by the `MockPeer` resolves the oneshot
/// the hook is blocked on in `GET /approvals/:id/wait` — exactly the path the
/// real Android app drives.
///
/// The database is a tempdir-backed `SQLite` file (not `:memory:`) so the test
/// exercises the real file-backed `Db::open` path. The tempdir is
/// intentionally leaked (dropping would delete the file while the server
/// holds it open); tests are short-lived and the OS reclaims the file.
///
/// # Panics
///
/// Panics if the tempdir cannot be created or the ephemeral `TcpListener`
/// cannot be bound / its local address read. Not expected in tests.
pub async fn spawn_server_with_mock_bridge() -> MockServer {
    let (transport, peer) = mock_pair();

    // Tempdir-backed sqlite file for realism. Leak the tempdir handle so the
    // file lives for the duration of the server; the OS reclaims it when the
    // test process exits.
    let dir = tempdir().expect("create tempdir for mock-bridge db");
    let db_path = dir
        .path()
        .join("pv-test.sqlite")
        .to_string_lossy()
        .into_owned();
    std::mem::forget(dir);

    let mut config = test_config();
    config.db_path = db_path;

    // Bind the listener directly so the bound port can be read and
    // config.server_url fixed up before spawning the server task.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral listener");
    let addr = listener.local_addr().expect("local addr");

    config.server_url = format!("http://{addr}");
    let server_config = config.clone();

    tokio::spawn(async move {
        if let Err(e) = run_with_bridge_on(config, transport, listener).await {
            eprintln!("test mock-bridge server exited with error: {e}");
        }
    });

    MockServer {
        config: server_config,
        peer,
    }
}

/// Read `ServerMessage`s from the `MockPeer` until a non-`Heartbeat` frame
/// arrives, returning it. The bridge sends a `Heartbeat` every
/// `HEARTBEAT_INTERVAL` and may emit other bookkeeping frames on connect;
/// the tests care about `AgentEvent` / `ApprovalRequest` frames, so the rest
/// are skipped. Bounds the wait at [`PEER_READ_TIMEOUT`] so a missing frame
/// fails the test fast instead of hanging.
///
/// # Panics
///
/// Panics if no non-heartbeat frame arrives within [`PEER_READ_TIMEOUT`] or
/// the peer read errors.
pub async fn next_non_heartbeat(peer: &mut MockPeer) -> ServerMessage {
    let deadline = tokio::time::timeout(PEER_READ_TIMEOUT, async {
        loop {
            let msg = peer
                .read_server_message()
                .await
                .expect("mock peer read server message");
            if !matches!(msg, ServerMessage::Heartbeat { .. }) {
                return msg;
            }
        }
    });
    deadline
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for non-heartbeat frame"))
}
