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
//! Big-endian length-prefixed framing round-trip tests.
//!
//! Verifies `read_length_prefixed` over a `tokio::io::duplex` and a
//! `mock_pair()` round-trip of `ClientMessage` / `ServerMessage` frames.
//! Built only with the `mock` cargo feature.

use pocket_veto_bt::mock::mock_pair;
use pocket_veto_bt::{BtTransport, read_length_prefixed};
use pocket_veto_core::protocol::{
    ClientMessage, Decision, Host, ServerMessage, decode_client_message, encode_client_message,
};
use tokio::io::AsyncWriteExt;

/// A `ClientMessage` round-trips through `read_length_prefixed` over an
/// in-memory duplex pipe, and the recovered full frame is byte-equal to the
/// original and decodes back to the same message with the decoder consuming
/// the entire frame.
///
/// This directly verifies that `read_length_prefixed` returns the **full
/// frame** (prefix + payload) that `decode_client_message` can decode. The
/// big-endian prefix is exercised implicitly: if the prefix were written
/// native-endian (little-endian on this host) the declared length would be
/// huge and `read_length_prefixed` would either reject it (`MAX_FRAME_SIZE`)
/// or block forever reading the payload; either way this test would fail.
#[tokio::test]
async fn read_length_prefixed_returns_full_frame_decodable_as_client_message() {
    let msg = ClientMessage::ApprovalDecision {
        approval_id: "ap-framing-rt-1".to_string(),
        decision: Decision::Allow,
        note: Some("framing round-trip".to_string()),
    };

    // encode_client_message emits the full length-prefixed frame the real
    // phone writes and the real transports carry.
    let frame = encode_client_message(&msg).expect("encode client message");
    assert!(
        frame.len() > 4,
        "frame must include the 4-byte prefix plus payload"
    );

    // duplex: write the full frame into one end, read it back from the other.
    let (mut writer, mut reader) = tokio::io::duplex(1024);
    writer.write_all(&frame).await.expect("write full frame");
    writer.flush().await.expect("flush");

    let recovered = read_length_prefixed(&mut reader)
        .await
        .expect("read_length_prefixed");

    // The recovered frame must be the FULL frame (prefix + payload), not just
    // the payload.
    assert_eq!(
        recovered, frame,
        "read_length_prefixed must return the full frame (prefix + payload)"
    );

    // decode_client_message expects the buffer to start with the prefix, so a
    // full frame must decode cleanly and consume exactly the frame length.
    let (consumed, decoded) = decode_client_message(&recovered).expect("decode client message");
    assert_eq!(
        consumed,
        recovered.len(),
        "decoder must consume the entire recovered frame"
    );
    assert_eq!(decoded, msg, "round-trip must preserve the message");
}

/// A `ClientMessage` and a `ServerMessage` round-trip through the mock
/// transport pair, exercising the bridge's actual framing path
/// (`MockTransport::write_frame` adds the prefix via `build_frame`;
/// `MockPeer::write_client_message` uses `encode_client_message`; the peer's
/// `read_server_message` and the transport's `read_frame` decode full frames).
///
/// - phone -> server: the peer writes a `ClientMessage` as a full frame; the
///   transport's `read_frame` returns that full frame, which
///   `decode_client_message` decodes with full-frame consumption. This is the
///   path the bridge uses to receive an `ApprovalDecision`.
/// - server -> phone: the bridge writes a `ServerMessage` payload via
///   `write_frame` (the mock adds the prefix); the peer decodes a full frame
///   back to the equal message.
#[tokio::test]
async fn mock_pair_round_trips_client_and_server_messages() {
    let (mut transport, mut peer) = mock_pair();

    // --- phone -> server: ClientMessage round-trip -------------------------
    let client_msg = ClientMessage::ApprovalDecision {
        approval_id: "ap-framing-rt-mock".to_string(),
        decision: Decision::Deny,
        note: None,
    };
    peer.write_client_message(&client_msg)
        .await
        .expect("peer writes client message");

    // The bridge reads a FULL frame here and hands it to
    // decode_client_message, which expects the prefix at the start.
    let frame = transport.read_frame().await.expect("transport read frame");
    let (consumed, decoded) = decode_client_message(&frame).expect("decode client message");
    assert_eq!(
        consumed,
        frame.len(),
        "decoder must consume the full frame the transport returned"
    );
    assert_eq!(decoded, client_msg, "client message round-trip");

    // --- server -> phone: ServerMessage round-trip -------------------------
    let server_msg = ServerMessage::AgentStart {
        agent_id: "a-framing-rt".to_string(),
        session_id: "s-framing-rt".to_string(),
        host: Host::Claude,
        name: "framing-rt".to_string(),
        workspace: "/tmp/framing-rt".to_string(),
        started_at: 1_700_000_000_000,
    };
    // The bridge hands write_frame a JSON payload (no prefix); the mock adds
    // the prefix via build_frame, so the peer decodes a full frame.
    let payload = serde_json::to_vec(&server_msg).expect("serialize server message");
    transport
        .write_frame(&payload)
        .await
        .expect("transport writes server message");

    let got = peer
        .read_server_message()
        .await
        .expect("peer reads server message");
    assert_eq!(got, server_msg, "server message round-trip");
}
