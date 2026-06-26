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
//! Integration tests for the `pocket-veto hook` subcommand.
//!
//! These tests spin up a real `axum::serve` instance on `127.0.0.1:0` (an
//! ephemeral port) backed by an in-memory [`pocket_veto_core::db::Db`], then drive the
//! hook's testable core [`pocket_veto::hook::run_with_input`] against it via a
//! real `reqwest::Client`. This exercises the full hook <-> server HTTP
//! round-trip (including bearer auth, JSON bodies, and the
//! `POST /approvals` -> `GET /approvals/:id/wait` long-poll) without spawning
//! a separate process.
//!
//! Approval decisions are delivered by a background task that sleeps briefly
//! then calls `POST /approvals/:id/decide`, so the hook's `GET /wait`
//! long-poll observes a real resolution (rather than relying on a pre-buffered
//! receiver).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pocket_veto::hook::{EXIT_DENY, EXIT_OK, HookOutcome, run_with_input};
use pocket_veto_core::protocol::ServerMessage;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

mod common;
use common::{TEST_TOKEN, hook_client, spawn_test_server, test_config};

/// Resolve an approval once its id is known. Sleeps `delay` first so the hook's
/// `GET /wait` is registered, then `POST /decide`.
async fn decide_after_delay(
    addr: SocketAddr,
    token: &str,
    approval_id: String,
    decision: &str,
    delay: Duration,
) {
    tokio::time::sleep(delay).await;
    let url = format!("http://{addr}/approvals/{approval_id}/decide");
    let client = reqwest::Client::new();
    let body = json!({ "decision": decision });
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .expect("POST /decide");
    assert!(
        resp.status().is_success(),
        "POST /decide returned {}",
        resp.status()
    );
}

/// Capture the `approval_id` forwarded to the bridge outbox. The server pushes
/// an `ApprovalRequest { approval_id, ... }` frame to `bridge_tx` when the hook
/// calls `POST /approvals`; the channel is drained here to find it.
///
/// `rx` is wrapped in a `Mutex` so it can be shared between the capture task
/// and the main test task.
async fn capture_approval_id(
    rx: &Mutex<mpsc::Receiver<ServerMessage>>,
    timeout: Duration,
) -> String {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        assert!(
            std::time::Instant::now() <= deadline,
            "timed out waiting for ApprovalRequest frame on bridge outbox"
        );
        let msg = {
            let mut rx = rx.lock().expect("bridge rx poisoned");
            rx.try_recv()
        };
        match msg {
            Ok(ServerMessage::ApprovalRequest { approval_id, .. }) => return approval_id,
            Ok(_) => {
                // Some other frame (e.g. an AgentEvent). Keep waiting.
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(mpsc::error::TryRecvError::Empty) => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(mpsc::error::TryRecvError::Disconnected) => {
                panic!("bridge outbox disconnected before ApprovalRequest arrived");
            }
        }
    }
}

/// Build a Claude `PreToolUse` JSON event (the raw stdin shape).
fn claude_pre_tool_use(tool: &str, input: &Value) -> Value {
    json!({
        "hook_event_name": "PreToolUse",
        "session_id": "sess-claude-1",
        "cwd": "/tmp/claude",
        "tool_name": tool,
        "tool_input": input.clone(),
    })
}

/// Build a Cursor `beforeShellExecution` JSON event.
fn cursor_before_shell_execution(command: &str) -> Value {
    json!({
        "hook_event_name": "beforeShellExecution",
        "sessionId": "sess-cursor-1",
        "cwd": "/tmp/cursor",
        "toolInput": { "command": command },
    })
}

/// Build a Claude `PostToolUse` JSON event (non-blocking).
fn claude_post_tool_use(tool: &str) -> Value {
    json!({
        "hook_event_name": "PostToolUse",
        "session_id": "sess-claude-post",
        "cwd": "/tmp/claude",
        "tool_name": tool,
        "tool_input": { "ok": true },
    })
}

// ---------------------------------------------------------------------------
// Test cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hook_pretooluse_allow_emits_claude_nested_allow() {
    let server = spawn_test_server().await;
    let client = hook_client();

    // Normalize the input the same way `hook::run` does, so run_with_input
    // can be handed an InternalEvent.
    let input = claude_pre_tool_use("Bash", &json!({ "command": "ls -la" }));
    let internal = pocket_veto_core::normalize::normalize(&input).expect("normalize");

    // Wrap the bridge rx in a Mutex so the capture task can share it with the
    // main task. The rx is moved out of the TestServer for this test.
    let bridge_rx = server.bridge_rx;
    let rx = Arc::new(Mutex::new(bridge_rx));

    // Spawn a task that captures the approval_id from the bridge outbox and
    // then decides "allow" after a short delay.
    let rx_clone = Arc::clone(&rx);
    let addr = server.addr;
    let token = TEST_TOKEN.to_string();
    tokio::spawn(async move {
        let approval_id = capture_approval_id(&rx_clone, Duration::from_secs(5)).await;
        decide_after_delay(
            addr,
            &token,
            approval_id,
            "allow",
            Duration::from_millis(50),
        )
        .await;
    });

    let outcome = run_with_input(&internal, &server.config, &client).await;
    let exit_code = outcome.exit_code();

    match outcome {
        HookOutcome::Allow { ref stdout } => {
            let v: Value = serde_json::from_str(stdout).expect("parse stdout json");
            assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
            assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow");
            // Must NOT use the deprecated top-level decision/reason.
            assert!(v.get("decision").is_none());
            assert!(v.get("reason").is_none());
            assert_eq!(exit_code, EXIT_OK);
        }
        other => panic!("expected Allow, got {other:?}"),
    }
}

#[tokio::test]
async fn hook_pretooluse_deny_emits_claude_nested_deny_and_exit2() {
    let server = spawn_test_server().await;
    let client = hook_client();

    let input = claude_pre_tool_use("Bash", &json!({ "command": "rm -rf /" }));
    let internal = pocket_veto_core::normalize::normalize(&input).expect("normalize");

    let rx = Arc::new(Mutex::new(server.bridge_rx));
    let rx_clone = Arc::clone(&rx);
    let addr = server.addr;
    let token = TEST_TOKEN.to_string();
    tokio::spawn(async move {
        let approval_id = capture_approval_id(&rx_clone, Duration::from_secs(5)).await;
        decide_after_delay(addr, &token, approval_id, "deny", Duration::from_millis(50)).await;
    });

    let outcome = run_with_input(&internal, &server.config, &client).await;
    let exit_code = outcome.exit_code();

    match outcome {
        HookOutcome::Deny { ref stdout } => {
            let v: Value = serde_json::from_str(stdout).expect("parse stdout json");
            assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
            assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
            assert!(v.get("decision").is_none());
            assert_eq!(exit_code, EXIT_DENY);
        }
        other => panic!("expected Deny, got {other:?}"),
    }
}

#[tokio::test]
async fn hook_posttooluse_is_fire_and_forget_and_persists_event() {
    let server = spawn_test_server().await;
    let client = hook_client();

    let input = claude_post_tool_use("Bash");
    let internal = pocket_veto_core::normalize::normalize(&input).expect("normalize");

    let outcome = run_with_input(&internal, &server.config, &client).await;
    assert_eq!(outcome, HookOutcome::FireAndForgetOk);
    assert_eq!(outcome.exit_code(), EXIT_OK);

    // The server should have recorded the event in the Db. The agent_id
    // equals the session_id per the hook's mapping.
    let agent_id = internal.session_id.clone();
    // Give the server a moment to finish persisting (the POST is awaited, so
    // this is just defensive).
    tokio::time::sleep(Duration::from_millis(20)).await;
    let history = server
        .db
        .agent_history(&agent_id, 0)
        .expect("history")
        .into_iter()
        .filter(|e| e.kind == "tool_call")
        .collect::<Vec<_>>();
    assert!(
        !history.is_empty(),
        "expected at least one tool_call event for {agent_id}"
    );
    let ev = &history[0];
    assert_eq!(ev.agent_id.as_ref(), agent_id.as_str());
    assert_eq!(ev.tool.as_deref(), Some("Bash"));
}

#[tokio::test]
async fn hook_cursor_before_shell_execution_emits_flat_cursor_shape() {
    let server = spawn_test_server().await;
    let client = hook_client();

    let input = cursor_before_shell_execution("echo hello");
    let internal = pocket_veto_core::normalize::normalize(&input).expect("normalize");
    // Sanity: the normalizer synthesized tool_name = "Shell" for Cursor.
    assert_eq!(internal.tool_name.as_deref(), Some("Shell"));

    let rx = Arc::new(Mutex::new(server.bridge_rx));
    let rx_clone = Arc::clone(&rx);
    let addr = server.addr;
    let token = TEST_TOKEN.to_string();
    tokio::spawn(async move {
        let approval_id = capture_approval_id(&rx_clone, Duration::from_secs(5)).await;
        decide_after_delay(
            addr,
            &token,
            approval_id,
            "allow",
            Duration::from_millis(50),
        )
        .await;
    });

    let outcome = run_with_input(&internal, &server.config, &client).await;
    let exit_code = outcome.exit_code();

    match outcome {
        HookOutcome::Allow { ref stdout } => {
            let v: Value = serde_json::from_str(stdout).expect("parse stdout json");
            // Cursor flat shape: permission / user_message / agent_message.
            assert_eq!(v["permission"], "allow");
            assert!(v["user_message"].is_string());
            assert_eq!(v["agent_message"], "");
            // Must NOT use the Claude nested shape.
            assert!(v.get("hookSpecificOutput").is_none());
            assert_eq!(exit_code, EXIT_OK);
        }
        other => panic!("expected Allow, got {other:?}"),
    }
}

#[tokio::test]
async fn hook_server_unreachable_fail_closed_deny_exit2() {
    // Build a config pointing at a port nothing is listening on. A
    // likely-free port is picked by binding and immediately dropping, then
    // reusing the address.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind for free port");
    let dead_addr = listener.local_addr().expect("local addr");
    drop(listener);

    let mut config = test_config();
    config.server_url = format!("http://{dead_addr}");
    config.approval_timeout_seconds = 1;
    let client = hook_client();

    let input = claude_pre_tool_use("Bash", &json!({ "command": "ls" }));
    let internal = pocket_veto_core::normalize::normalize(&input).expect("normalize");

    let outcome = run_with_input(&internal, &config, &client).await;
    let exit_code = outcome.exit_code();

    match outcome {
        HookOutcome::Deny { ref stdout } => {
            let v: Value = serde_json::from_str(stdout).expect("parse stdout json");
            assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
            assert!(
                v["hookSpecificOutput"]["permissionDecisionReason"]
                    .as_str()
                    .unwrap()
                    .contains("PocketVeto unreachable")
            );
            assert_eq!(exit_code, EXIT_DENY);
        }
        other => panic!("expected Deny (fail-closed), got {other:?}"),
    }
}

#[tokio::test]
async fn hook_pretooluse_timeout_fail_closed_deny_exit2() {
    // Use a real server but never resolve the approval. With
    // approval_timeout_seconds = 1, the hook's GET /wait should time out and
    // the hook should fail-closed to Deny.
    let mut server = spawn_test_server().await;
    server.config.approval_timeout_seconds = 1;
    let client = hook_client();

    let input = claude_pre_tool_use("Bash", &json!({ "command": "ls" }));
    let internal = pocket_veto_core::normalize::normalize(&input).expect("normalize");

    // Drain the bridge outbox so it doesn't fill up; the approval is NOT
    // resolved.
    let mut bridge_rx = server.bridge_rx;
    let _drain = tokio::spawn(async move { while bridge_rx.recv().await.is_some() {} });

    let outcome = run_with_input(&internal, &server.config, &client).await;
    let exit_code = outcome.exit_code();

    match outcome {
        HookOutcome::Deny { ref stdout } => {
            let v: Value = serde_json::from_str(stdout).expect("parse stdout json");
            assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
            // The reason should mention the timeout (the synthesized reason
            // for a timeout-without-note case).
            let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap();
            assert!(
                reason.contains("timed out") || reason.contains("PocketVeto unreachable"),
                "reason should mention timeout or unreachability, got: {reason}"
            );
            assert_eq!(exit_code, EXIT_DENY);
        }
        other => panic!("expected Deny (timeout fail-closed), got {other:?}"),
    }
}

#[tokio::test]
async fn hook_non_blocking_event_with_dead_server_is_silent_exit0() {
    // A non-blocking event must NOT fail-closed when the server is down: it
    // should return FireAndForgetError (which the caller maps to exit 0
    // silently).
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind for free port");
    let dead_addr = listener.local_addr().expect("local addr");
    drop(listener);

    let mut config = test_config();
    config.server_url = format!("http://{dead_addr}");
    config.approval_timeout_seconds = 1;
    let client = hook_client();

    let input = claude_post_tool_use("Bash");
    let internal = pocket_veto_core::normalize::normalize(&input).expect("normalize");

    let outcome = run_with_input(&internal, &config, &client).await;
    assert_eq!(outcome, HookOutcome::FireAndForgetError);
    assert_eq!(outcome.exit_code(), EXIT_OK);
}
