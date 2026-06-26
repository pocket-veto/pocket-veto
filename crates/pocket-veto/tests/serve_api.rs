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
//! Integration tests for the `pocket-veto serve` HTTP API.
//!
//! These tests drive the router via `tower::ServiceExt::oneshot` against an
//! in-memory [`pocket_veto_core::db::Db`] and a known bearer token. No network socket
//! is bound. The approval round-trip (POST /approvals -> POST /decide ->
//! GET /wait) is the core of the product and is exercised here.

use std::sync::{Arc, Mutex};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use pocket_veto::serve::{AppState, build_router};
use pocket_veto_core::approvals::ApprovalWaiters;
use pocket_veto_core::db::Db;
use pocket_veto_core::events::EventBus;
use pocket_veto_core::protocol::ServerMessage;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tower::ServiceExt;

mod common;
use common::{TEST_TOKEN, test_config};

/// Build a router backed by an in-memory Db and a known token. Returns the
/// router and a handle to the bridge-outbox receiver so a test can assert on
/// forwarded messages if it wants to.
fn test_router() -> (axum::Router, mpsc::Receiver<ServerMessage>, Arc<Db>) {
    let db = Arc::new(Db::open_in_memory().expect("open in-memory db"));
    let bus = EventBus::new(64);
    let waiters = Arc::new(ApprovalWaiters::new());
    let mut config = test_config();
    config.server_url = "http://127.0.0.1:38475".to_string();
    config.bind_addr = "127.0.0.1:38475".to_string();
    config.approval_timeout_seconds = 300;
    let config = Arc::new(config);
    let (bridge_tx, bridge_rx) = mpsc::channel::<ServerMessage>(64);
    let pending_receivers = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let announced_agents = Arc::new(Mutex::new(std::collections::HashSet::new()));
    let state = AppState::new(
        Arc::clone(&db),
        bus,
        waiters,
        config,
        bridge_tx,
        pending_receivers,
        announced_agents,
    );
    let router = build_router(state);
    (router, bridge_rx, db)
}

/// Build a `Request<Body>` with the given method, path, optional bearer
/// token, and optional JSON body.
fn req(method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(t) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let body = match body {
        Some(v) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&v).expect("encode body"))
        }
        None => Body::empty(),
    };
    builder.body(body).expect("build request")
}

/// Extract the JSON body from a response, panicking on failure.
async fn body_json(res: axum::http::Response<Body>) -> Value {
    let bytes = to_bytes(res.into_body(), 1024 * 1024)
        .await
        .expect("collect body");
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).expect("decode json")
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_returns_200_without_auth() {
    let (router, _rx, _db) = test_router();
    let res = router
        .oneshot(req("GET", "/health", None, None))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["bt"], "unknown");
}

#[tokio::test]
async fn events_without_bearer_returns_401() {
    let (router, _rx, _db) = test_router();
    let res = router
        .oneshot(req("POST", "/events", None, Some(json!({}))))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(res).await;
    assert!(body["error"].as_str().unwrap().contains("unauthorized"));
}

#[tokio::test]
async fn events_with_wrong_bearer_returns_401() {
    let (router, _rx, _db) = test_router();
    let res = router
        .oneshot(req(
            "POST",
            "/events",
            Some("wrong-token"),
            Some(json!({"agent_id": "a1"})),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn events_with_bearer_persists_and_lists_agent() {
    let (router, _rx, db) = test_router();
    let res = router
        .clone()
        .oneshot(req(
            "POST",
            "/events",
            Some(TEST_TOKEN),
            Some(json!({
                "agent_id": "agent-events-1",
                "session_id": "sess-1",
                "host": "claude",
                "name": "refactor",
                "workspace": "/tmp/w",
                "kind": "tool_call",
                "tool": "Bash",
                "payload": {"cmd": "ls"},
                "ts": 1_700_000_000_000_i64,
            })),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["ok"], true);

    // The agent row should now exist in the DB.
    let agents = db.list_agents().expect("list agents");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].agent_id.as_ref(), "agent-events-1");
    assert_eq!(agents[0].status, pocket_veto_core::AgentStatus::Running);

    // And /agents should surface it (rebuild router since oneshot consumed it).
    let res = router
        .oneshot(req("GET", "/agents", Some(TEST_TOKEN), None))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    let arr = body.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["agent_id"], "agent-events-1");
}

#[tokio::test]
async fn events_missing_agent_id_returns_400() {
    let (router, _rx, _db) = test_router();
    let res = router
        .oneshot(req(
            "POST",
            "/events",
            Some(TEST_TOKEN),
            Some(json!({"kind": "tool_call"})),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn approval_wait_times_out_and_marks_db() {
    let (router, _rx, db) = test_router();

    // Create the approval.
    let res = router
        .clone()
        .oneshot(req(
            "POST",
            "/approvals",
            Some(TEST_TOKEN),
            Some(json!({
                "agent_id": "agent-timeout",
                "tool": "Bash",
                "summary": "rm -rf node_modules",
                "detail": "destructive",
            })),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    let approval_id = body["approval_id"]
        .as_str()
        .expect("approval_id")
        .to_string();

    // Wait with a 1s timeout; should return decision=timeout.
    let res = router
        .oneshot(req(
            "GET",
            &format!("/approvals/{approval_id}/wait?timeout=1"),
            Some(TEST_TOKEN),
            None,
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["decision"], "timeout");

    // The DB row should be marked timeout.
    let row = db
        .pending_approval(&approval_id)
        .expect("query")
        .expect("row exists");
    assert_eq!(row.status, "timeout");
}

#[tokio::test]
async fn approval_round_trip_allow() {
    // The critical test: POST /approvals -> POST /decide(allow) -> GET /wait
    // returns decision=allow. The receiver is stashed by POST /approvals,
    // the sender is fired by POST /decide via ApprovalWaiters::resolve, and
    // GET /wait consumes the now-ready receiver.
    let (router, _rx, db) = test_router();

    // 1. Create the approval.
    let res = router
        .clone()
        .oneshot(req(
            "POST",
            "/approvals",
            Some(TEST_TOKEN),
            Some(json!({
                "agent_id": "agent-allow",
                "tool": "Write",
                "summary": "edit src/lib.rs",
            })),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let approval_id = body_json(res).await["approval_id"]
        .as_str()
        .expect("approval_id")
        .to_string();

    // 2. Decide allow before /wait. The oneshot fires and buffers the value
    //    in the stashed receiver.
    let res = router
        .clone()
        .oneshot(req(
            "POST",
            &format!("/approvals/{approval_id}/decide"),
            Some(TEST_TOKEN),
            Some(json!({ "decision": "allow", "note": "looks good" })),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_json(res).await["ok"], true);

    // 3. /wait should now resolve immediately with allow.
    let res = router
        .clone()
        .oneshot(req(
            "GET",
            &format!("/approvals/{approval_id}/wait?timeout=5"),
            Some(TEST_TOKEN),
            None,
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["decision"], "allow");

    // 4. The DB row should be marked allowed.
    let row = db
        .pending_approval(&approval_id)
        .expect("query")
        .expect("row exists");
    assert_eq!(row.status, "allowed");
    assert_eq!(row.decision_note.as_deref(), Some("looks good"));
}

#[tokio::test]
async fn approval_round_trip_deny() {
    let (router, _rx, _db) = test_router();

    let res = router
        .clone()
        .oneshot(req(
            "POST",
            "/approvals",
            Some(TEST_TOKEN),
            Some(json!({
                "agent_id": "agent-deny",
                "tool": "Bash",
                "summary": "rm -rf /",
            })),
        ))
        .await
        .expect("oneshot");
    let approval_id = body_json(res).await["approval_id"]
        .as_str()
        .expect("approval_id")
        .to_string();

    // Decide deny, then wait.
    let res = router
        .clone()
        .oneshot(req(
            "POST",
            &format!("/approvals/{approval_id}/decide"),
            Some(TEST_TOKEN),
            Some(json!({ "decision": "deny" })),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);

    let res = router
        .oneshot(req(
            "GET",
            &format!("/approvals/{approval_id}/wait?timeout=5"),
            Some(TEST_TOKEN),
            None,
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_json(res).await["decision"], "deny");
}

#[tokio::test]
async fn decide_unknown_approval_returns_404() {
    let (router, _rx, _db) = test_router();
    let res = router
        .oneshot(req(
            "POST",
            "/approvals/no-such-id/decide",
            Some(TEST_TOKEN),
            Some(json!({ "decision": "allow" })),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn wait_unknown_approval_returns_404() {
    let (router, _rx, _db) = test_router();
    let res = router
        .oneshot(req(
            "GET",
            "/approvals/no-such-id/wait?timeout=1",
            Some(TEST_TOKEN),
            None,
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn decide_invalid_decision_returns_400() {
    let (router, _rx, _db) = test_router();
    // First create an approval to get past the 404 check.
    let res = router
        .clone()
        .oneshot(req(
            "POST",
            "/approvals",
            Some(TEST_TOKEN),
            Some(json!({
                "agent_id": "agent-bad-decision",
                "tool": "Bash",
                "summary": "x",
            })),
        ))
        .await
        .expect("oneshot");
    let approval_id = body_json(res).await["approval_id"]
        .as_str()
        .expect("approval_id")
        .to_string();

    let res = router
        .oneshot(req(
            "POST",
            &format!("/approvals/{approval_id}/decide"),
            Some(TEST_TOKEN),
            Some(json!({ "decision": "yolo" })),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn agent_history_returns_events() {
    let (router, _rx, _db) = test_router();
    // Post two events for an agent.
    for ts in [1_000_i64, 1_100, 1_200] {
        let res = router
            .clone()
            .oneshot(req(
                "POST",
                "/events",
                Some(TEST_TOKEN),
                Some(json!({
                    "agent_id": "agent-hist",
                    "session_id": "s",
                    "host": "cursor",
                    "name": "n",
                    "workspace": "/w",
                    "kind": "tool_call",
                    "tool": "Bash",
                    "payload": {"ts": ts},
                    "ts": ts,
                })),
            ))
            .await
            .expect("oneshot");
        assert_eq!(res.status(), StatusCode::OK);
    }

    // History with since=1050 should return the two later events.
    let res = router
        .oneshot(req(
            "GET",
            "/agents/agent-hist/history?since=1050",
            Some(TEST_TOKEN),
            None,
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    let arr = body.as_array().expect("array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["agent_id"], "agent-hist");
    assert_eq!(arr[0]["kind"], "tool_call");
}

#[tokio::test]
async fn approvals_without_bearer_returns_401() {
    let (router, _rx, _db) = test_router();
    let res = router
        .oneshot(req(
            "POST",
            "/approvals",
            None,
            Some(json!({
                "agent_id": "a",
                "tool": "Bash",
                "summary": "x",
            })),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn agents_without_bearer_returns_401() {
    let (router, _rx, _db) = test_router();
    let res = router
        .oneshot(req("GET", "/agents", None, None))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn approval_request_forwarded_to_bridge_outbox() {
    let (router, mut rx, _db) = test_router();
    let res = router
        .clone()
        .oneshot(req(
            "POST",
            "/approvals",
            Some(TEST_TOKEN),
            Some(json!({
                "agent_id": "agent-bridge",
                "tool": "Bash",
                "summary": "rm -rf node_modules",
                "detail": "destructive",
            })),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);

    // The bridge outbox should have received an ApprovalRequest frame.
    let msg = rx.recv().await.expect("bridge outbox message");
    match msg {
        ServerMessage::ApprovalRequest {
            agent_id,
            tool,
            summary,
            detail,
            ..
        } => {
            assert_eq!(agent_id, "agent-bridge");
            assert_eq!(tool, "Bash");
            assert_eq!(summary, "rm -rf node_modules");
            assert_eq!(detail, "destructive");
        }
        other => panic!("expected ApprovalRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn event_forwarded_to_bridge_outbox() {
    let (router, mut rx, _db) = test_router();
    let res = router
        .clone()
        .oneshot(req(
            "POST",
            "/events",
            Some(TEST_TOKEN),
            Some(json!({
                "agent_id": "agent-ev-bridge",
                "kind": "shell",
                "tool": "Bash",
                "payload": {"cmd": "ls"},
                "ts": 42_i64,
            })),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);

    // The first event for a new agent emits an AgentStart (with the agent
    // metadata) before the AgentEvent. Consume both in order.
    let start = rx.recv().await.expect("bridge outbox AgentStart");
    match start {
        ServerMessage::AgentStart { agent_id, host, .. } => {
            assert_eq!(agent_id, "agent-ev-bridge");
            // The event body did not set `host`, so the server defaults to
            // "claude" -> Host::Claude.
            assert_eq!(host, pocket_veto_core::protocol::Host::Claude);
        }
        other => panic!("expected AgentStart, got {other:?}"),
    }
    let msg = rx.recv().await.expect("bridge outbox AgentEvent");
    match msg {
        ServerMessage::AgentEvent {
            agent_id,
            kind,
            tool,
            ts,
            ..
        } => {
            assert_eq!(agent_id, "agent-ev-bridge");
            assert_eq!(kind, pocket_veto_core::protocol::EventKind::Shell);
            assert_eq!(tool.as_deref(), Some("Bash"));
            assert_eq!(ts, 42);
        }
        other => panic!("expected AgentEvent, got {other:?}"),
    }
}

// Keep a reference to the oneshot receiver type so the import is used even
// if future tests don't construct one directly.
#[allow(dead_code)]
type _Receiver = oneshot::Receiver<pocket_veto_core::protocol::Decision>;
