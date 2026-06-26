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
//! End-to-end integration test.
//!
//! Proves that a hook fires, the server stores the event, the BT bridge
//! encodes a frame, and the "phone" (a [`MockPeer`]) receives and decodes it
//! — end-to-end, in CI, with no radio.
//!
//! The mock backend ([`pocket_veto_bt::mock::mock_pair`]) lets CI verify the same path
//! the real Linux/Windows backends will drive. The three tests here cover:
//!
//! 1. [`m1_hook_event_flows_to_mock_phone`] — a non-blocking `PostToolUse`
//!    event flows hook -> `POST /events` -> `Db` -> `EventBus` -> bridge outbox
//!    -> `BtBridge` -> `MockPeer` as an `AgentEvent` frame. Proves the
//!    fire-and-forget pipeline.
//!
//! 2. [`m1_approval_request_flows_and_decision_returns`] — a blocking
//!    `PreToolUse` event flows hook -> `POST /approvals` -> bridge forwards an
//!    `ApprovalRequest` frame to the phone -> the phone writes an
//!    `ApprovalDecision` frame back -> the bridge resolves the waiter ->
//!    the hook's `GET /wait` returns `allow` -> the hook emits the Claude
//!    nested `hookSpecificOutput` stdout. THE FULL ROUND-TRIP.
//!
//! 3. [`m1_multiple_events_stream_to_phone`] — three `PostToolUse` events
//!    posted in sequence are received by the phone as three `AgentEvent`
//!    frames in order. Proves the streaming path handles a sequence.
//!
//! All `MockPeer` reads are wrapped in `tokio::time::timeout` so a broken
//! pipeline fails the test fast instead of hanging. The
//! [`next_non_heartbeat`] helper skips `Heartbeat` (and any other non-event
//! frames the bridge emits on connect) so the tests are robust to heartbeat
//! timing — the bridge's default 15 s heartbeat will not fire within the 2 s
//! test window, but the helper future-proofs the tests against a shorter
//! heartbeat interval.

use pocket_veto::hook::{EXIT_OK, HookOutcome, run_with_input};
use pocket_veto_core::protocol::{ClientMessage, Decision, ServerMessage};
use serde_json::{Value, json};

mod common;
use common::{hook_client, next_non_heartbeat, spawn_server_with_mock_bridge};

/// Build a Claude `PostToolUse` JSON event (non-blocking, fire-and-forget).
/// Distinct `session_id`s per call so the events stream as separate frames.
fn claude_post_tool_use(session_id: &str, tool: &str) -> Value {
    json!({
        "hook_event_name": "PostToolUse",
        "session_id": session_id,
        "cwd": "/tmp/claude-m1",
        "tool_name": tool,
        "tool_input": { "ok": true },
    })
}

/// Build a Claude `PreToolUse` JSON event (blocking, requires approval).
fn claude_pre_tool_use(session_id: &str, tool: &str, input: &Value) -> Value {
    json!({
        "hook_event_name": "PreToolUse",
        "session_id": session_id,
        "cwd": "/tmp/claude-m1",
        "tool_name": tool,
        "tool_input": input.clone(),
    })
}

// ---------------------------------------------------------------------------
// Test 1: hook -> server -> bridge -> phone (fire-and-forget event)
// ---------------------------------------------------------------------------

/// A non-blocking `PostToolUse` event flows from the hook,
/// through the server (`Db` + `EventBus` + bridge outbox), through the `BtBridge`
/// (frame encode + transport write), and is received and decoded by the
/// `MockPeer` as an `AgentEvent` frame. Proves the fire-and-forget pipeline
/// end-to-end with the mock backend.
#[tokio::test]
async fn m1_hook_event_flows_to_mock_phone() {
    let server = spawn_server_with_mock_bridge().await;
    let client = hook_client();

    // A unique session_id so the AgentEvent frame is attributable. The hook
    // uses session_id as the agent_id (per the hook's mapping).
    let session_id = "m1-sess-event";
    let input = claude_post_tool_use(session_id, "Bash");
    let internal = pocket_veto_core::normalize::normalize(&input).expect("normalize");

    // Drive the hook's testable core. This POSTs to /events, which persists
    // the event, publishes on the EventBus, and forwards an AgentEvent to
    // the bridge outbox. The BtBridge picks it up and writes the frame to
    // the MockTransport; the MockPeer reads it.
    let outcome = run_with_input(&internal, &server.config, &client).await;
    assert_eq!(
        outcome,
        HookOutcome::FireAndForgetOk,
        "non-blocking event should succeed"
    );
    assert_eq!(outcome.exit_code(), EXIT_OK);

    // The phone should receive an AgentStart frame (emitted on the first
    // event for a new agent) followed by an AgentEvent frame with the right
    // agent_id (== session_id per the hook's mapping), kind, and tool.
    let mut peer = server.peer;
    let msg = next_non_heartbeat(&mut peer).await;
    match msg {
        ServerMessage::AgentStart {
            agent_id,
            host,
            workspace,
            ..
        } => {
            assert_eq!(
                agent_id, session_id,
                "agent_id should be the session_id (hook mapping)"
            );
            // The hook sends host="claude" (Host::Claude) and the cwd as
            // workspace; the server forwards both on the AgentStart.
            assert_eq!(host, pocket_veto_core::protocol::Host::Claude);
            assert_eq!(workspace, "/tmp/claude-m1");
        }
        other => panic!("expected AgentStart, got {other:?}"),
    }
    let msg = next_non_heartbeat(&mut peer).await;
    match msg {
        ServerMessage::AgentEvent {
            agent_id,
            kind,
            tool,
            ..
        } => {
            assert_eq!(
                agent_id, session_id,
                "agent_id should be the session_id (hook mapping)"
            );
            // PostToolUse -> kind "tool_call" -> EventKind::ToolCall.
            assert_eq!(kind, pocket_veto_core::protocol::EventKind::ToolCall);
            assert_eq!(tool.as_deref(), Some("Bash"));
        }
        other => panic!("expected AgentEvent, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test 2: full approval round-trip through the bridge
// ---------------------------------------------------------------------------

/// The full approval round-trip: a blocking `PreToolUse` event flows hook ->
/// `POST /approvals` -> bridge forwards an `ApprovalRequest` frame to the
/// phone -> the phone writes an `ApprovalDecision` frame back -> the bridge
/// resolves the waiter -> the hook's `GET /wait` returns `allow` -> the hook
/// emits the Claude nested `hookSpecificOutput` stdout.
///
/// This is the single test that proves the entire server-side pipeline works
/// before any Android code is written: the phone side is a `MockPeer` doing
/// exactly what the Android `BluetoothService` will do (read a frame, write a
/// decision).
#[tokio::test]
async fn m1_approval_request_flows_and_decision_returns() {
    let server = spawn_server_with_mock_bridge().await;
    let client = hook_client();

    let session_id = "m1-sess-approval";
    let input = claude_pre_tool_use(
        session_id,
        "Bash",
        &json!({ "command": "rm -rf /tmp/junk" }),
    );
    let internal = pocket_veto_core::normalize::normalize(&input).expect("normalize");

    // Spawn a "phone" task: read the ApprovalRequest frame from the peer,
    // then write an ApprovalDecision (allow) back. This mirrors the Android
    // BluetoothService: read frame -> show notification -> user taps Allow ->
    // write decision frame.
    let mut peer = server.peer;
    let phone_task = tokio::spawn(async move {
        // Wait for the ApprovalRequest frame (skip any heartbeats).
        let approval_id = loop {
            let msg = next_non_heartbeat(&mut peer).await;
            if let ServerMessage::ApprovalRequest { approval_id, .. } = msg {
                break approval_id;
            }
            // Some other frame (e.g. an AgentEvent from a prior test on the
            // same port — impossible here since each test gets a fresh pair,
            // but be defensive). Keep reading.
        };

        // Send the decision back, exactly as the Android app would.
        let decision = ClientMessage::ApprovalDecision {
            approval_id,
            decision: Decision::Allow,
            note: Some("m1: looks fine".to_string()),
        };
        peer.write_client_message(&decision)
            .await
            .expect("phone writes approval decision");
    });

    // Drive the hook's blocking path. This POSTs /approvals (which forwards
    // the ApprovalRequest to the bridge -> phone), then GETs /wait which
    // long-polls until the phone's decision resolves the waiter.
    let outcome = run_with_input(&internal, &server.config, &client).await;
    let exit_code = outcome.exit_code();

    // The phone task should have completed (wrote the decision).
    phone_task
        .await
        .expect("phone task should complete without panic");

    match outcome {
        HookOutcome::Allow { ref stdout } => {
            let v: Value = serde_json::from_str(stdout).expect("parse stdout json");
            // Claude nested shape.
            assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
            assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow");
            // The note sent from the "phone" should surface as the reason.
            assert_eq!(
                v["hookSpecificOutput"]["permissionDecisionReason"],
                "m1: looks fine"
            );
            // Must NOT use the deprecated top-level decision/reason.
            assert!(v.get("decision").is_none());
            assert!(v.get("reason").is_none());
            assert_eq!(exit_code, EXIT_OK);
        }
        other => panic!("expected Allow, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test 3: multiple events stream to the phone in order
// ---------------------------------------------------------------------------

/// Post three `PostToolUse` events via the hook in sequence and assert the
/// `MockPeer` receives three `AgentEvent` frames in order. Proves the
/// streaming path handles a sequence (the bridge forwards each outbox
/// message as a separate frame, not just the first).
#[tokio::test]
async fn m1_multiple_events_stream_to_phone() {
    let server = spawn_server_with_mock_bridge().await;
    let client = hook_client();

    // Three distinct sessions -> three distinct agent_ids -> three frames.
    let sessions = ["m1-sess-stream-1", "m1-sess-stream-2", "m1-sess-stream-3"];
    let tools = ["Bash", "Write", "Edit"];

    for (i, session) in sessions.iter().enumerate() {
        let input = claude_post_tool_use(session, tools[i]);
        let internal = pocket_veto_core::normalize::normalize(&input).expect("normalize");
        let outcome = run_with_input(&internal, &server.config, &client).await;
        assert_eq!(
            outcome,
            HookOutcome::FireAndForgetOk,
            "event {i} should succeed"
        );
    }

    // The phone should receive, for each of the three distinct agents, an
    // AgentStart (first event for that agent) followed by an AgentEvent —
    // six frames total, in posting order. next_non_heartbeat skips any
    // heartbeats interleaved.
    let mut peer = server.peer;
    for (i, expected_session) in sessions.iter().enumerate() {
        // AgentStart first.
        let msg = next_non_heartbeat(&mut peer).await;
        match msg {
            ServerMessage::AgentStart { agent_id, .. } => {
                assert_eq!(
                    agent_id, *expected_session,
                    "frame {i}: AgentStart wrong agent_id"
                );
            }
            other => panic!("frame {i}: expected AgentStart, got {other:?}"),
        }
        // Then the AgentEvent.
        let msg = next_non_heartbeat(&mut peer).await;
        match msg {
            ServerMessage::AgentEvent { agent_id, tool, .. } => {
                assert_eq!(agent_id, *expected_session, "frame {i}: wrong agent_id");
                assert_eq!(tool.as_deref(), Some(tools[i]), "frame {i}: wrong tool");
            }
            other => panic!("frame {i}: expected AgentEvent, got {other:?}"),
        }
    }
}
