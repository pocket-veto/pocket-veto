//! In-memory mock Bluetooth transport for tests and dev runs.
//!
//! [`mock_pair`] returns a connected ([`MockTransport`], [`MockPeer`]) tuple.
//! The `MockTransport` implements [`BtTransport`]
//! and is the "server side" the bridge drives. The `MockPeer` is the "phone
//! side" with typed helpers for reading [`ServerMessage`]s and writing
//! [`ClientMessage`]s.
//!
//! Frames exchanged between the two are exactly the bytes the real transports
//! would carry: 4-byte big-endian length prefix + JSON payload. The bridge
//! hands `MockTransport::write_frame` a JSON **payload** (no prefix); the mock
//! adds the prefix via [`crate::frame::build_frame`] before sending the full
//! frame down the channel, so [`MockPeer::read_server_message`] can decode it
//! with [`pocket_veto_core::protocol::decode_message`]. In the other direction
//! [`MockPeer::write_client_message`] sends a full frame (via
//! [`pocket_veto_core::protocol::encode_client_message`]) which the bridge
//! reads back as a full frame — so the bridge's framing logic is exercised
//! identically to a real backend.
//!
//! [`MockTransport::break_connection`] flips the shared `broken` flag so the
//! next read/write fails with a "broken connection" error — used by reconnect
//! tests to force the bridge through its backoff loop.

use std::sync::Arc;

use pocket_veto_core::protocol::{
    ClientMessage, ServerMessage, decode_message, encode_client_message,
};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tracing::debug;

use crate::bridge::BtTransport;
use crate::frame::build_frame;

/// Server-side mock transport. Implements [`BtTransport`].
///
/// Holds the phone->server receiver and the server->phone sender; the matching
/// [`MockPeer`] holds the mirrors. When either side calls `break_connection`,
/// the shared `broken` flag flips and subsequent reads / writes fail.
pub struct MockTransport {
    /// Frames arriving from the phone (`MockPeer`'s `tx`).
    rx: mpsc::Receiver<Vec<u8>>,
    /// Frames headed to the phone (`MockPeer`'s `rx` reads these).
    tx: mpsc::Sender<Vec<u8>>,
    /// Shared broken-flag: `break_connection` flips it so reads / writes
    /// short-circuit with a "broken connection" error.
    control: Arc<Mutex<MockControl>>,
    connected: bool,
}

/// Phone-side peer. Pairs with a [`MockTransport`].
pub struct MockPeer {
    /// Frames arriving from the server (`MockTransport`'s `tx`).
    rx: mpsc::Receiver<Vec<u8>>,
    /// Frames headed to the server (`MockTransport`'s `rx` reads these).
    tx: mpsc::Sender<Vec<u8>>,
    control: Arc<Mutex<MockControl>>,
}

/// Shared control structure. Both ends hold the same `Arc<Mutex<MockControl>>`.
/// `break_connection` sets `broken = true`; the live `mpsc::Sender`s live in
/// `MockTransport` / `MockPeer` and are dropped when those structs are, so a
/// dropped peer eventually closes the channel. The `broken` flag is what
/// `read_frame` / `write_frame` check to short-circuit with a "broken
/// connection" error before touching the channel — dropping a cloned
/// `mpsc::Sender` does not close a channel, so the flag (not sender drops) is
/// what simulates the radio drop.
#[derive(Debug)]
struct MockControl {
    broken: bool,
}

impl MockControl {
    fn break_connection(&mut self) {
        self.broken = true;
    }
}

/// Create a connected ([`MockTransport`], [`MockPeer`]) pair.
///
/// Both sides start in the "connected" state: `MockTransport::is_connected`
/// returns `true` and a `connect()` call is a no-op. Use
/// [`MockTransport::break_connection`] to simulate a radio drop.
#[must_use]
pub fn mock_pair() -> (MockTransport, MockPeer) {
    let (s2p_tx, s2p_rx) = mpsc::channel::<Vec<u8>>(64);
    let (p2s_tx, p2s_rx) = mpsc::channel::<Vec<u8>>(64);

    let control = Arc::new(Mutex::new(MockControl { broken: false }));

    let transport = MockTransport {
        rx: p2s_rx,
        tx: s2p_tx,
        control: Arc::clone(&control),
        connected: true,
    };
    let peer = MockPeer {
        rx: s2p_rx,
        tx: p2s_tx,
        control,
    };
    (transport, peer)
}

impl MockTransport {
    /// Simulate a radio drop. The next `read_frame` / `write_frame` on this
    /// side and on the peer will fail, forcing the bridge's reconnect logic.
    pub async fn break_connection(&self) {
        let mut ctl = self.control.lock().await;
        ctl.break_connection();
    }

    /// Whether `break_connection` has been called on this pair.
    pub async fn is_broken(&self) -> bool {
        self.control.lock().await.broken
    }
}

impl MockPeer {
    /// Read the next [`ServerMessage`] the server (bridge) sent to the phone.
    ///
    /// # Errors
    ///
    /// Returns an error if the channel is closed (the bridge dropped its
    /// transport) or the frame is malformed.
    pub async fn read_server_message(&mut self) -> anyhow::Result<ServerMessage> {
        let frame = self
            .rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("mock peer: server closed the channel"))?;
        let (_consumed, msg) = decode_message(&frame)
            .map_err(|e| anyhow::anyhow!("mock peer: decode server message: {e}"))?;
        debug!(?msg, "mock peer: read server message");
        Ok(msg)
    }

    /// Write a [`ClientMessage`] (phone -> server). Encodes it as a
    /// length-prefixed frame.
    ///
    /// # Errors
    ///
    /// Returns an error if the channel is closed (the server dropped its
    /// transport) or encoding fails.
    pub async fn write_client_message(&mut self, msg: &ClientMessage) -> anyhow::Result<()> {
        let frame = encode_client_message(msg)
            .map_err(|e| anyhow::anyhow!("mock peer: encode client message: {e}"))?;
        self.tx
            .send(frame)
            .await
            .map_err(|_e| anyhow::anyhow!("mock peer: server closed the channel"))?;
        debug!(?msg, "mock peer: wrote client message");
        Ok(())
    }

    /// Simulate a radio drop from the peer side. Equivalent to
    /// [`MockTransport::break_connection`].
    pub async fn break_connection(&self) {
        let mut ctl = self.control.lock().await;
        ctl.break_connection();
    }
}

impl BtTransport for MockTransport {
    async fn connect(&mut self) -> anyhow::Result<()> {
        // The mock starts connected. If `break_connection` was called, the
        // mock does NOT simulate "radio came back" (no flag flip, no fresh
        // `mock_pair` reset here) — the bridge reconnect test only asserts the
        // bridge is still pending in the reconnect loop, not that it actually
        // re-establishes (see `bridge_reconnects_on_transport_error`).
        let ctl = self.control.lock().await;
        if ctl.broken {
            return Err(anyhow::anyhow!("mock transport: connection broken"));
        }
        self.connected = true;
        Ok(())
    }

    async fn read_frame(&mut self) -> anyhow::Result<Vec<u8>> {
        let ctl = self.control.lock().await;
        if ctl.broken {
            return Err(anyhow::anyhow!("mock transport: read on broken connection"));
        }
        drop(ctl);
        match self.rx.recv().await {
            Some(frame) => Ok(frame),
            None => Err(anyhow::anyhow!("mock transport: peer closed the channel")),
        }
    }

    async fn write_frame(&mut self, payload: &[u8]) -> anyhow::Result<()> {
        let ctl = self.control.lock().await;
        if ctl.broken {
            return Err(anyhow::anyhow!(
                "mock transport: write on broken connection"
            ));
        }
        drop(ctl);
        let frame = build_frame(payload)
            .map_err(|e| anyhow::anyhow!("mock transport: frame build: {e}"))?;
        self.tx
            .send(frame)
            .await
            .map_err(|_e| anyhow::anyhow!("mock transport: peer closed the channel"))?;
        Ok(())
    }

    async fn close(&mut self) -> anyhow::Result<()> {
        self.connected = false;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unreachable,
    clippy::unwrap_in_result,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use pocket_veto_core::protocol::{Decision, Host, decode_client_message};
    use std::sync::Arc;
    use std::time::Duration;

    use crate::bridge::BtBridge;
    use pocket_veto_core::approvals::ApprovalWaiters;
    use pocket_veto_core::db::Db;

    fn sample_server_message() -> ServerMessage {
        ServerMessage::AgentStart {
            agent_id: "a1".to_string(),
            session_id: "s1".to_string(),
            host: Host::Claude,
            name: "refactor".to_string(),
            workspace: "/tmp/w".to_string(),
            started_at: 1_700_000_000_000,
        }
    }

    #[tokio::test]
    async fn mock_pair_roundtrip_server_message() {
        let (mut transport, mut peer) = mock_pair();
        let msg = sample_server_message();
        // The bridge hands `write_frame` a JSON payload (no length prefix);
        // the mock adds the prefix internally, so the peer decodes a full frame.
        let payload = serde_json::to_vec(&msg).expect("serialize");
        transport
            .write_frame(&payload)
            .await
            .expect("transport write");
        let got = peer.read_server_message().await.expect("peer read");
        assert_eq!(got, msg);
    }

    #[tokio::test]
    async fn mock_pair_roundtrip_client_message() {
        let (mut transport, mut peer) = mock_pair();
        let msg = ClientMessage::ApprovalDecision {
            approval_id: "ap1".to_string(),
            decision: Decision::Allow,
            note: Some("ok".to_string()),
        };
        peer.write_client_message(&msg).await.expect("peer write");
        let frame = transport.read_frame().await.expect("transport read");
        let (_consumed, got) = decode_client_message(&frame).expect("decode");
        assert_eq!(got, msg);
    }

    #[tokio::test]
    async fn mock_break_connection_fails_io() {
        let (mut transport, _peer) = mock_pair();
        transport.break_connection().await;
        let err = transport.read_frame().await.expect_err("read should fail");
        assert!(err.to_string().contains("broken"));
    }

    /// Spawn the bridge's `run` future in the background; returns a handle
    /// that can be aborted to stop the bridge.
    fn spawn_bridge(
        transport: MockTransport,
        outbox_rx: mpsc::Receiver<ServerMessage>,
        waiters: Arc<ApprovalWaiters>,
        db: Arc<Db>,
    ) -> tokio::task::JoinHandle<anyhow::Result<()>> {
        let mut bridge = BtBridge::new(transport, outbox_rx, waiters, db)
            .with_heartbeat_interval(Duration::from_mins(1))
            .with_heartbeat_timeout(Duration::from_mins(10));
        tokio::spawn(async move { bridge.run().await })
    }

    #[tokio::test]
    async fn bridge_forwards_outbox_to_peer() {
        let (transport, mut peer) = mock_pair();
        let (tx, rx) = mpsc::channel::<ServerMessage>(8);
        let waiters = Arc::new(ApprovalWaiters::new());
        let db = Arc::new(Db::open_in_memory().expect("db"));
        let handle = spawn_bridge(transport, rx, Arc::clone(&waiters), db);

        let msg = sample_server_message();
        tx.send(msg.clone()).await.expect("send to outbox");

        let got = tokio::time::timeout(Duration::from_secs(2), peer.read_server_message())
            .await
            .expect("peer should receive within 2s")
            .expect("peer read ok");
        assert_eq!(got, msg);

        // Drop the sender so the bridge's outbox closes and run() returns.
        drop(tx);
        handle
            .await
            .expect("bridge task joined")
            .expect("bridge run returned Ok");
    }

    #[tokio::test]
    async fn bridge_resolves_approval_decision() {
        let (transport, mut peer) = mock_pair();
        let (tx, rx) = mpsc::channel::<ServerMessage>(8);
        let waiters = Arc::new(ApprovalWaiters::new());
        let db = Arc::new(Db::open_in_memory().expect("db"));

        // Seed an approval row so the db update path has something to update.
        db.insert_approval("ap-1", "a1", "Bash", "rm -rf x", None, 1_000)
            .expect("insert approval");

        let receiver = waiters.register("ap-1");
        let handle = spawn_bridge(transport, rx, Arc::clone(&waiters), Arc::clone(&db));

        let decision = ClientMessage::ApprovalDecision {
            approval_id: "ap-1".to_string(),
            decision: Decision::Allow,
            note: Some("looks fine".to_string()),
        };
        peer.write_client_message(&decision).await.expect("write");

        let got = tokio::time::timeout(Duration::from_secs(2), receiver)
            .await
            .expect("decision within 2s")
            .expect("receiver yielded");
        assert_eq!(got, Decision::Allow);

        // The bridge should also have updated the db row.
        let row = db.pending_approval("ap-1").expect("query").expect("row");
        assert_eq!(row.status, "allowed");
        assert_eq!(row.decision_note.as_deref(), Some("looks fine"));

        drop(tx);
        handle.await.expect("join").expect("run ok");
    }

    #[tokio::test]
    async fn bridge_reconnects_on_transport_error() {
        // Use a mock that is already broken: the bridge's first connect()
        // will fail, so it goes through the backoff loop. The test asserts
        // that the `run` future is still pending (not errored, not completed)
        // after a brief sleep — i.e. the bridge is alive in the reconnect loop.
        let (transport, _peer) = mock_pair();
        transport.break_connection().await;
        let (tx, rx) = mpsc::channel::<ServerMessage>(8);
        let waiters = Arc::new(ApprovalWaiters::new());
        let db = Arc::new(Db::open_in_memory().expect("db"));
        let mut bridge = BtBridge::new(transport, rx, waiters, db)
            .with_heartbeat_interval(Duration::from_mins(1))
            .with_heartbeat_timeout(Duration::from_mins(10));
        let run = tokio::spawn(async move { bridge.run().await });

        // Give the bridge a moment to attempt connect, hit the error, and
        // enter the backoff sleep. The first backoff slot is 1s; the test sleeps
        // 200ms, which is enough for the connect attempt to have happened.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // The future should still be pending: it has not errored and the
        // outbox is not closed.
        let still_running = !run.is_finished();
        assert!(still_running, "bridge should still be in reconnect loop");

        // Cleanup: drop the sender so the bridge eventually exits its loop.
        // (It is sleeping in backoff; the next connect attempt will also
        // fail, so dropping tx is not strictly necessary, but the task is
        // aborted to avoid a hanging test.)
        drop(tx);
        run.abort();
    }
}
