//! Platform-agnostic Bluetooth bridge connecting the pocket-veto server to a
//! [`BtTransport`] backend.
//!
//! The bridge is the single async task that owns the Bluetooth link lifecycle
//! on the server side. It:
//!
//! 1. Reads [`ServerMessage`]s from an outbox `mpsc::Receiver` (the HTTP layer
//!    publishes here) and writes them over the transport as length-prefixed
//!    frames.
//! 2. Reads length-prefixed [`ClientMessage`] frames off the transport and
//!    routes them: [`ApprovalDecision`](ClientMessage::ApprovalDecision) goes
//!    to [`ApprovalWaiters`] (and is mirrored to the [`Db`] so `/wait` callers
//!    that missed the oneshot can still see the final status),
//!    [`HeartbeatAck`](ClientMessage::HeartbeatAck) advances the
//!    `last_acked_ts` watermark used for replay-on-reconnect, and
//!    [`Subscribe`](ClientMessage::Subscribe) is logged as a readiness signal.
//! 3. Sends [`ServerMessage::Heartbeat`] every 15 s and forces a reconnect if
//!    45 s pass without any frame (ack or message) from the phone.
//! 4. Reconnects with exponential backoff (1 s, 2 s, 4 s, 8 s, cap 30 s) on
//!    any transport error, and replays missed per-agent events from the
//!    [`Db`] before resuming the live outbox stream.
//!
//! The bridge depends only on the [`BtTransport`] trait, so the same code
//! drives the Linux RFCOMM backend, the Windows COM-port backend, and the
//! in-memory `crate::mock` backend used by tests.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pocket_veto_core::approvals::ApprovalWaiters;
use pocket_veto_core::db::Db;
use pocket_veto_core::protocol::{self, AgentId, ClientMessage, EventKind, Host, ServerMessage};
use tokio::sync::mpsc;
use tokio::time::{self, Instant, MissedTickBehavior, interval_at};
use tracing::{debug, info, warn};

/// Heartbeat interval: the bridge sends [`ServerMessage::Heartbeat`] this
/// often while a connection is live.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// Heartbeat timeout: if no frame (ack or any client message) arrives within
/// this window, the bridge forces a reconnect. 45 s == 3 missed heartbeats.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(45);

/// Reconnect backoff schedule. On each consecutive connect failure the bridge
/// sleeps for the next entry; after the last entry it stays at the cap. The
/// schedule resets to the start on a successful connect.
const BACKOFF_SCHEDULE: &[Duration] = &[
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(30),
];

/// Backoff cap: the delay used once `idx` runs off the end of
/// [`BACKOFF_SCHEDULE`]. Mirrors the schedule's final (largest) entry so the
/// cap is a named constant rather than `BACKOFF_SCHEDULE.last().expect()`
/// (no `expect` in non-test library code).
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// The abstract Bluetooth transport the bridge drives.
///
/// `read_frame` returns one **full frame** (4-byte big-endian length prefix +
/// JSON payload) so the bridge can append the bytes to its accumulation buffer
/// and feed them straight into
/// [`pocket_veto_core::protocol::decode_client_message`], which expects the
/// buffer to start with the prefix. `write_frame` takes a JSON **payload** (no
/// prefix); the transport adds the length prefix via
/// [`crate::frame::write_length_prefixed`] (or its sync counterpart), so the
/// transport owns the wire framing end to end. The in-memory `crate::mock`
/// backend builds a full frame from the payload before sending it down its
/// channel, so the peer's decoder sees the same bytes a real radio would carry.
///
/// The async methods use native async-fn-in-trait (AFIT, stable on 1.96) with
/// an explicit `+ Send` bound on the returned futures — the bridge is driven
/// inside a `tokio::spawn` task, so the futures must be `Send`. They return
/// `anyhow::Result` (not `pocket_veto_core::error::Result`) so backend impls
/// can layer arbitrary error context without a custom error enum; this is the
/// documented exception (see `crates/pocket-veto-bt/AGENTS.md`).
pub trait BtTransport: Send {
    /// Establish the underlying connection (accept an RFCOMM client, open the
    /// COM port, etc.). Called by the bridge's reconnect loop.
    fn connect(&mut self) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Read one full frame (prefix + payload) off the transport. The bridge
    /// accumulates these into a buffer and runs the protocol decoder; a single
    /// read may or may not contain a complete frame (the accumulation buffer
    /// handles both).
    fn read_frame(&mut self) -> impl std::future::Future<Output = anyhow::Result<Vec<u8>>> + Send;

    /// Write a JSON payload (no length prefix) to the transport. The transport
    /// adds the 4-byte big-endian length prefix.
    fn write_frame(
        &mut self,
        payload: &[u8],
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Tear down the current connection. Called by the bridge before
    /// reconnecting and on shutdown. After this returns, `is_connected`
    /// should be `false` until the next successful `connect`.
    fn close(&mut self) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Whether a connection is currently established.
    fn is_connected(&self) -> bool;
}

/// The bridge: drives one [`BtTransport`] for the lifetime of the server.
///
/// Construct with [`BtBridge::new`] and run with [`BtBridge::run`]. `run`
/// only returns on a fatal error (e.g. the outbox sender was dropped and the
/// server is shutting down); transport failures trigger reconnect-with-backoff
/// inside `run`.
pub struct BtBridge<T: BtTransport> {
    transport: T,
    outbox: mpsc::Receiver<ServerMessage>,
    waiters: Arc<ApprovalWaiters>,
    db: Arc<Db>,
    /// Watermark of the most-recently-acked heartbeat `ts`, used as the
    /// `since_ts` argument to [`Db::events_since`] on reconnect replay.
    last_acked_ts: i64,
    /// Agent ids ever forwarded an `AgentStart` for. Used to scope
    /// replay-on-reconnect to the agents the phone actually knows about.
    known_agents: HashSet<AgentId>,
    heartbeat_interval: Duration,
    heartbeat_timeout: Duration,
}

impl<T: BtTransport> BtBridge<T> {
    /// Create a bridge with default heartbeat (15 s) and timeout (45 s)
    /// settings.
    #[must_use]
    pub fn new(
        transport: T,
        outbox: mpsc::Receiver<ServerMessage>,
        waiters: Arc<ApprovalWaiters>,
        db: Arc<Db>,
    ) -> Self {
        Self {
            transport,
            outbox,
            waiters,
            db,
            last_acked_ts: 0,
            known_agents: HashSet::new(),
            heartbeat_interval: HEARTBEAT_INTERVAL,
            heartbeat_timeout: HEARTBEAT_TIMEOUT,
        }
    }

    /// Override the heartbeat interval (mainly for tests).
    #[must_use]
    pub fn with_heartbeat_interval(mut self, d: Duration) -> Self {
        self.heartbeat_interval = d;
        self
    }

    /// Override the heartbeat timeout (mainly for tests).
    #[must_use]
    pub fn with_heartbeat_timeout(mut self, d: Duration) -> Self {
        self.heartbeat_timeout = d;
        self
    }

    /// Run the bridge until a fatal error occurs.
    ///
    /// "Fatal" here means the outbox sender was dropped (server shutting
    /// down) — transport errors are absorbed by the reconnect loop and never
    /// bubble out of `run`.
    ///
    /// # Errors
    ///
    /// Returns `Err` only if the outbox closes and the bridge can no longer
    /// make progress. Transport-level failures are logged and retried.
    // justification: the bridge run loop is an inherently-complex reconnect
    // state machine (connect -> drive -> backoff); extracting sub-steps would
    // obscure the control flow without reducing the real complexity.
    #[allow(clippy::cognitive_complexity)]
    pub async fn run(&mut self) -> anyhow::Result<()> {
        let mut backoff_idx = 0usize;
        loop {
            // Try to connect, with exponential backoff on consecutive
            // failures. The backoff resets to the start of the schedule on a
            // successful connect.
            match self.transport.connect().await {
                Ok(()) => {
                    info!("bt bridge: connected");
                    backoff_idx = 0;
                }
                Err(e) => {
                    warn!(error = %e, "bt bridge: connect failed");
                    self.sleep_backoff(&mut backoff_idx).await;
                    continue;
                }
            }

            // On a fresh connection, replay missed events for every agent the
            // bridge has ever seen. The phone's `last_acked_ts` is tracked
            // from `HeartbeatAck` messages; if the bridge has never seen one
            // it replays from ts=0 (everything). Replay failures are logged
            // but do not tear down the connection — the live stream still
            // matters.
            self.replay_missed_events().await;

            // Drive the connection until it breaks or times out.
            let outcome = self.drive_connection().await;

            // Tear down whatever transport state remains before reconnecting.
            if let Err(e) = self.transport.close().await {
                debug!(error = %e, "bt bridge: close after disconnect (ignored)");
            }

            match outcome {
                ConnectionOutcome::TransportError(e) => {
                    warn!(error = %e, "bt bridge: connection lost, reconnecting");
                }
                ConnectionOutcome::HeartbeatTimeout => {
                    warn!("bt bridge: heartbeat timeout, forcing reconnect");
                }
                ConnectionOutcome::OutboxClosed => {
                    info!("bt bridge: outbox closed, shutting down");
                    return Ok(());
                }
            }

            // Always sleep at least the first backoff slot before
            // reconnecting, even on a heartbeat timeout, to avoid a tight
            // reconnect loop if the peer is flapping.
            self.sleep_backoff(&mut backoff_idx).await;
        }
    }

    /// Drive a single live connection: forward outbox messages, read client
    /// frames, send heartbeats, watch the heartbeat deadline.
    // justification: a live connection multiplexes four concurrent activities
    // (outbox drain, frame read, heartbeat send, deadline watch) via select!;
    // the branching is inherent to that select loop.
    #[allow(clippy::cognitive_complexity)]
    async fn drive_connection(&mut self) -> ConnectionOutcome {
        let mut read_buf: Vec<u8> = Vec::new();
        let mut heartbeat_tick = interval_at(Instant::now(), self.heartbeat_interval);
        heartbeat_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // Discard the immediate tick fired by `interval_at` at `now`.
        heartbeat_tick.tick().await;

        let mut last_activity = Instant::now();

        loop {
            let timeout_deadline = last_activity + self.heartbeat_timeout;

            tokio::select! {
                // 1. Outbox -> transport. A `None` here means the server
                //    dropped the sender; the bridge should exit cleanly.
                msg = self.outbox.recv() => {
                    match msg {
                        Some(m) => {
                            if let Err(e) = self.handle_outbox_message(&m).await {
                                return ConnectionOutcome::TransportError(e);
                            }
                        }
                        None => {
                            return ConnectionOutcome::OutboxClosed;
                        }
                    }
                }
                // 2. Transport -> bridge (frames may be partial; accumulate).
                frame_result = self.transport.read_frame() => {
                    match frame_result {
                        Ok(bytes) => {
                            read_buf.extend_from_slice(&bytes);
                            last_activity = Instant::now();
                            if let Err(e) = self.drain_client_messages(&mut read_buf) {
                                return ConnectionOutcome::TransportError(e);
                            }
                        }
                        Err(e) => {
                            return ConnectionOutcome::TransportError(
                                anyhow::anyhow!("read_frame: {e}"),
                            );
                        }
                    }
                }
                // 3. Heartbeat tick -> send Heartbeat frame.
                _ = heartbeat_tick.tick() => {
                    let now = now_ms();
                    let msg = ServerMessage::Heartbeat { ts: now };
                    match serde_json::to_vec(&msg) {
                        Ok(payload) => {
                            if let Err(e) = self.transport.write_frame(&payload).await {
                                return ConnectionOutcome::TransportError(
                                    anyhow::anyhow!("heartbeat write: {e}"),
                                );
                            }
                            debug!(ts = now, "bt bridge: sent heartbeat");
                        }
                        Err(e) => {
                            warn!(error = %e, "bt bridge: encode heartbeat failed");
                        }
                    }
                }
                // 4. Heartbeat timeout -> force reconnect.
                () = time::sleep_until(timeout_deadline) => {
                    return ConnectionOutcome::HeartbeatTimeout;
                }
            }
        }
    }

    /// Forward one outbox [`ServerMessage`] over the transport, tracking
    /// `known_agents` on `AgentStart`.
    async fn handle_outbox_message(&mut self, msg: &ServerMessage) -> anyhow::Result<()> {
        if let ServerMessage::AgentStart { agent_id, .. } = msg {
            self.known_agents.insert(agent_id.clone().into());
        }
        let payload =
            serde_json::to_vec(msg).map_err(|e| anyhow::anyhow!("encode outbox message: {e}"))?;
        self.transport.write_frame(&payload).await?;
        debug!(
            ?msg,
            payload_bytes = payload.len(),
            "bt bridge: wrote outbox frame"
        );
        Ok(())
    }

    /// Decode every complete [`ClientMessage`] currently in `read_buf`,
    /// draining the consumed bytes from the front. Partial frames are left in
    /// the buffer for the next read.
    fn drain_client_messages(&mut self, read_buf: &mut Vec<u8>) -> anyhow::Result<()> {
        loop {
            match protocol::decode_client_message(read_buf) {
                Ok((consumed, msg)) => {
                    read_buf.drain(..consumed);
                    self.route_client_message(&msg);
                }
                Err(e) if e.is_protocol_eof() => {
                    // Need more bytes; leave the buffer intact for the next
                    // read_frame call.
                    return Ok(());
                }
                Err(e) => {
                    // A non-EOF protocol error means the stream is
                    // unrecoverably desynced. Reconnecting is the safe move.
                    return Err(anyhow::anyhow!("decode client message: {e}"));
                }
            }
        }
    }

    /// Route one decoded [`ClientMessage`]: approval decisions go to the
    /// waiters + db, heartbeat acks advance the watermark, subscribes are
    /// logged.
    // justification: routing one client message branches per variant and per
    // sub-condition (unknown waiter, db error); the branching maps 1:1 to the
    // protocol's message set and is clearest as one match.
    #[allow(clippy::cognitive_complexity)]
    fn route_client_message(&mut self, msg: &ClientMessage) {
        match msg {
            ClientMessage::ApprovalDecision {
                approval_id,
                decision,
                note,
            } => {
                info!(
                    approval_id,
                    ?decision,
                    has_note = note.is_some(),
                    "bt bridge: received approval decision",
                );
                let status = decision.to_approval_status();
                let now = now_ms();
                if let Err(e) =
                    self.db
                        .set_approval_decision(approval_id, status, note.as_deref(), now)
                {
                    warn!(error = %e, approval_id, "bt bridge: db set_approval_decision failed");
                }
                let existed = self.waiters.resolve(approval_id, *decision);
                if !existed {
                    warn!(
                        approval_id,
                        "bt bridge: approval decision for unknown waiter"
                    );
                }
            }
            ClientMessage::HeartbeatAck { ts } => {
                if *ts > self.last_acked_ts {
                    self.last_acked_ts = *ts;
                }
                debug!(
                    ts,
                    acked_ts = self.last_acked_ts,
                    "bt bridge: heartbeat ack"
                );
            }
            ClientMessage::Subscribe { filter } => {
                info!(?filter, "bt bridge: phone subscribed");
            }
        }
    }

    /// On a fresh connection, replay missed events for every known agent from
    /// `last_acked_ts` forward. Each replayed row is sent as an
    /// [`ServerMessage::AgentEvent`]. Replay errors are logged and do not
    /// break the connection — the live outbox stream is still authoritative.
    ///
    /// Before the events, for each known agent the bridge emits an
    /// [`ServerMessage::AgentStart`] reconstructed from the `agents` table.
    /// The `events` table does not carry the name/host/workspace metadata,
    /// so on a reconnect the phone would otherwise have a stale (or empty)
    /// card until the next session start. Re-emitting `AgentStart` from the
    /// persisted agent row gives the phone a consistent card state as soon
    /// as the connection comes back. If the agent row is missing (e.g. the
    /// db was wiped), the `AgentStart` is skipped and only the events are replayed —
    /// the phone keeps whatever card state it had.
    // justification: replay iterates known agents, re-emit AgentStart, then
    // stream events with per-row error handling; the nesting reflects the
    // "for each agent -> for each event" structure of the replay.
    #[allow(clippy::cognitive_complexity)]
    async fn replay_missed_events(&mut self) {
        if self.known_agents.is_empty() {
            debug!("bt bridge: no known agents, skipping replay");
            return;
        }
        let since = self.last_acked_ts;
        // Collect the known agent ids into an owned Vec to avoid holding an
        // immutable borrow of `self.known_agents` across the mutable
        // `write_server_message` calls below (the transport write borrows
        // `self` mutably via `&mut self`).
        let agent_ids: Vec<AgentId> = self.known_agents.iter().cloned().collect();
        for agent_id in &agent_ids {
            // First, re-emit an AgentStart from the persisted agent row so
            // the phone has fresh metadata (name/host/workspace/startedAt)
            // after a reconnect.
            if let Ok(Some(row)) = self.db.get_agent(agent_id.as_ref()) {
                let host = row.host.parse::<Host>().unwrap_or(Host::Claude);
                let session_id = row.session_id.clone().unwrap_or_default();
                let name = row.name.clone().unwrap_or_default();
                let workspace = row.workspace.clone().unwrap_or_default();
                let start_msg = ServerMessage::AgentStart {
                    agent_id: row.agent_id.to_string(),
                    session_id,
                    host,
                    name,
                    workspace,
                    started_at: row.started_at.0,
                };
                if let Err(e) = self.write_server_message(&start_msg).await {
                    warn!(
                        error = %e,
                        "bt bridge: replay AgentStart write failed, aborting replay"
                    );
                    return;
                }
            }

            let rows = match self.db.events_since(agent_id.as_ref(), since) {
                Ok(rows) => rows,
                Err(e) => {
                    warn!(error = %e, agent_id = %agent_id, "bt bridge: events_since failed during replay");
                    continue;
                }
            };
            if rows.is_empty() {
                continue;
            }
            debug!(
                agent_id = %agent_id,
                count = rows.len(),
                since,
                "bt bridge: replaying events"
            );
            for row in rows {
                let Ok(kind) = EventKind::from_db_str(&row.kind) else {
                    debug!(kind = %row.kind, "bt bridge: unknown event kind in replay, skipping");
                    continue;
                };
                let msg = ServerMessage::AgentEvent {
                    agent_id: row.agent_id.to_string(),
                    kind,
                    tool: row.tool.clone(),
                    payload: row.payload.clone(),
                    ts: row.ts.0,
                };
                if let Err(e) = self.write_server_message(&msg).await {
                    warn!(error = %e, "bt bridge: replay write failed, aborting replay");
                    return;
                }
            }
        }
    }

    /// Encode and write a single [`ServerMessage`] over the transport.
    /// Factored out of `replay_missed_events` so the encode + write + log
    /// path is not duplicated for the `AgentStart` and `AgentEvent` frames.
    ///
    /// # Errors
    ///
    /// Returns `anyhow::Error` if the message cannot be encoded or the
    /// transport write fails.
    async fn write_server_message(&mut self, msg: &ServerMessage) -> anyhow::Result<()> {
        let payload =
            serde_json::to_vec(msg).map_err(|e| anyhow::anyhow!("encode replay message: {e}"))?;
        self.transport.write_frame(&payload).await?;
        debug!(
            ?msg,
            payload_bytes = payload.len(),
            "bt bridge: wrote replay frame"
        );
        Ok(())
    }

    /// Sleep for the backoff slot at `idx`, advancing `idx` up to the cap.
    async fn sleep_backoff(&self, idx: &mut usize) {
        let slot = BACKOFF_SCHEDULE.get(*idx).copied().unwrap_or(BACKOFF_CAP);
        debug!(?slot, idx = *idx, "bt bridge: backing off before reconnect");
        *idx = (*idx).saturating_add(1).min(BACKOFF_SCHEDULE.len());
        time::sleep(slot).await;
    }
}

/// Outcome of driving one connection to completion.
enum ConnectionOutcome {
    /// The transport returned an error (read/write/encode failure). The
    /// bridge will close, backoff, and reconnect.
    TransportError(anyhow::Error),
    /// No frame from the phone within the heartbeat timeout window. The
    /// bridge forces a reconnect.
    HeartbeatTimeout,
    /// The outbox sender was dropped, meaning the server is shutting down.
    /// `run` should return.
    OutboxClosed,
}

/// Current unix time in milliseconds, matching the convention in
/// `pocket_veto_core::protocol` (all timestamps are `i64` ms).
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
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
    clippy::indexing_slicing,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use crate::mock::mock_pair;
    use pocket_veto_core::approvals::ApprovalWaiters;
    use pocket_veto_core::db::Db;
    use pocket_veto_core::protocol::AgentStatus;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// `replay_missed_events` emits an `AgentStart` reconstructed from the
    /// `agents` table row BEFORE the replayed `AgentEvent`s, so a phone that
    /// reconnects gets a consistent card state (name/host/workspace/
    /// startedAt) without waiting for the next session start.
    ///
    /// The test seeds a `Db` with an agent row + two events, constructs a
    /// `BtBridge<MockTransport>` with the agent pre-added to `known_agents`
    /// (simulating "the bridge already announced this agent before the
    /// drop"), and call `replay_missed_events` directly. The paired
    /// `MockPeer` should receive the `AgentStart` (from the row) then the
    /// two `AgentEvent`s in order.
    ///
    /// This is a unit test on the private method rather than an
    /// end-to-end connect-drop-reconnect test because the `MockTransport`
    /// has no "radio came back" path — once `break_connection` flips the
    /// broken flag, every subsequent `connect()` errors, so a full reconnect
    /// cycle would hang. Calling `replay_missed_events` directly exercises
    /// the exact code path the reconnect loop invokes after a successful
    /// `connect()`, with deterministic state.
    #[tokio::test]
    async fn replay_missed_events_emits_agent_start_then_events() {
        let (transport, mut peer) = mock_pair();
        // An outbox receiver is required by BtBridge::new but is not used
        // by replay_missed_events; create one and drop the sender.
        let (_tx, rx) = mpsc::channel::<ServerMessage>(8);
        let waiters = Arc::new(ApprovalWaiters::new());
        let db = Arc::new(Db::open_in_memory().expect("db"));

        // Seed an agent row + two events.
        db.upsert_agent(
            "replay-agent",
            "replay-sess",
            "cursor",
            "replay-test",
            "/tmp/replay",
            AgentStatus::Running,
            5_000,
            None,
        )
        .expect("seed agent");
        db.record_event(
            "replay-agent",
            "tool_call",
            Some("Bash"),
            &json!({"cmd": "ls"}),
            6_000,
        )
        .expect("seed event 1");
        db.record_event(
            "replay-agent",
            "tool_call",
            Some("Write"),
            &json!({"path": "/tmp/x"}),
            7_000,
        )
        .expect("seed event 2");

        let mut bridge = BtBridge::new(transport, rx, waiters, db);
        // Simulate "the bridge already announced this agent before the
        // drop" by pre-populating known_agents. This is what
        // handle_outbox_message does on a live AgentStart.
        bridge
            .known_agents
            .insert("replay-agent".to_string().into());

        // Drive the replay directly. This is the exact call `run()` makes
        // after a successful connect().
        bridge.replay_missed_events().await;

        // 1. AgentStart reconstructed from the agents table row.
        let msg = tokio::time::timeout(Duration::from_secs(2), peer.read_server_message())
            .await
            .expect("peer reads replayed AgentStart within timeout")
            .expect("peer read ok");
        match msg {
            ServerMessage::AgentStart {
                agent_id,
                session_id,
                host,
                name,
                workspace,
                started_at,
            } => {
                assert_eq!(agent_id, "replay-agent");
                assert_eq!(session_id, "replay-sess");
                assert_eq!(host, Host::Cursor);
                assert_eq!(name, "replay-test");
                assert_eq!(workspace, "/tmp/replay");
                assert_eq!(started_at, 5_000);
            }
            other => panic!("expected replayed AgentStart, got {other:?}"),
        }

        // 2. Replayed AgentEvent (event 1).
        let msg = tokio::time::timeout(Duration::from_secs(2), peer.read_server_message())
            .await
            .expect("peer reads replayed AgentEvent 1 within timeout")
            .expect("peer read ok");
        match msg {
            ServerMessage::AgentEvent {
                agent_id, tool, ts, ..
            } => {
                assert_eq!(agent_id, "replay-agent");
                assert_eq!(tool.as_deref(), Some("Bash"));
                assert_eq!(ts, 6_000);
            }
            other => panic!("expected replayed AgentEvent 1, got {other:?}"),
        }

        // 3. Replayed AgentEvent (event 2).
        let msg = tokio::time::timeout(Duration::from_secs(2), peer.read_server_message())
            .await
            .expect("peer reads replayed AgentEvent 2 within timeout")
            .expect("peer read ok");
        match msg {
            ServerMessage::AgentEvent {
                agent_id, tool, ts, ..
            } => {
                assert_eq!(agent_id, "replay-agent");
                assert_eq!(tool.as_deref(), Some("Write"));
                assert_eq!(ts, 7_000);
            }
            other => panic!("expected replayed AgentEvent 2, got {other:?}"),
        }
    }

    /// `replay_missed_events` is a no-op when `known_agents` is empty (a
    /// fresh bridge has nothing to replay). Guards against a regression
    /// where an empty `known_agents` set still hits the db.
    #[tokio::test]
    async fn replay_missed_events_noop_when_no_known_agents() {
        let (transport, mut peer) = mock_pair();
        let (_tx, rx) = mpsc::channel::<ServerMessage>(8);
        let waiters = Arc::new(ApprovalWaiters::new());
        let db = Arc::new(Db::open_in_memory().expect("db"));

        let mut bridge = BtBridge::new(transport, rx, waiters, db);
        // known_agents is empty by default.
        assert!(bridge.known_agents.is_empty());

        bridge.replay_missed_events().await;

        // The peer should receive nothing within a short window.
        let result =
            tokio::time::timeout(Duration::from_millis(100), peer.read_server_message()).await;
        assert!(
            result.is_err(),
            "replay with no known agents should not emit any frames"
        );
    }

    /// `replay_missed_events` skips the `AgentStart` when the agent row is
    /// missing from the db (e.g. the db was wiped between the drop and the
    /// reconnect), and emits nothing else if there are also no events. The
    /// phone keeps its existing card state. A second known agent that DOES
    /// have a row but no events emits only the `AgentStart`.
    #[tokio::test]
    async fn replay_missed_events_skips_agent_start_when_row_missing() {
        let (transport, mut peer) = mock_pair();
        let (_tx, rx) = mpsc::channel::<ServerMessage>(8);
        let waiters = Arc::new(ApprovalWaiters::new());
        let db = Arc::new(Db::open_in_memory().expect("db"));

        // Seed an agent row with NO events. This agent will emit an
        // AgentStart on replay but no AgentEvents.
        db.upsert_agent(
            "orphan-agent",
            "orphan-sess",
            "claude",
            "orphan",
            "/tmp/orphan",
            AgentStatus::Running,
            1_000,
            None,
        )
        .expect("seed orphan agent");

        let mut bridge = BtBridge::new(transport, rx, waiters, db);
        // A known agent id that has NO row in the agents table and NO
        // events — replay should emit nothing for it.
        bridge
            .known_agents
            .insert("missing-agent".to_string().into());
        // A known agent id that has a row but no events — replay should
        // emit only the AgentStart.
        bridge
            .known_agents
            .insert("orphan-agent".to_string().into());

        bridge.replay_missed_events().await;

        // The orphan agent (has a row, no events) emits only an AgentStart.
        let msg = tokio::time::timeout(Duration::from_secs(2), peer.read_server_message())
            .await
            .expect("peer reads orphan AgentStart within timeout")
            .expect("peer read ok");
        match msg {
            ServerMessage::AgentStart { agent_id, .. } => {
                assert_eq!(agent_id, "orphan-agent");
            }
            other => panic!("expected orphan AgentStart, got {other:?}"),
        }
        // No further frames: the missing-agent produced nothing (no row,
        // no events) and the orphan had no events after the AgentStart.
        let result =
            tokio::time::timeout(Duration::from_millis(100), peer.read_server_message()).await;
        assert!(
            result.is_err(),
            "no further frames expected after orphan AgentStart"
        );
    }
}
