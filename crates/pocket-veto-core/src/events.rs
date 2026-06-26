//! In-process event bus for live dashboard updates and BT-bridge fan-out.
//!
//! The HTTP layer publishes an [`EventMessage`] here for every event it
//! persists; the BT bridge subscribes and forwards each event as a
//! [`ServerMessage::AgentEvent`](crate::protocol::ServerMessage::AgentEvent)
//! frame. Because events are also written to `SQLite` first, a dropped
//! subscriber can always re-sync via
//! [`Db::events_since`](crate::db::Db::events_since) on reconnect — the bus
//! is a best-effort fast path, not the source of truth.
//!
//! Uses [`tokio::sync::broadcast`]: many subscribers, each with its own
//! lag-tolerant queue. A subscriber that falls behind receives a
//! [`broadcast::error::RecvError::Lagged`] and can choose to catch up from
//! `SQLite`.

use tokio::sync::broadcast;

use crate::protocol::EventKind;

/// One event flowing through the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventMessage {
    pub agent_id: String,
    pub kind: EventKind,
    pub tool: Option<String>,
    pub payload: serde_json::Value,
    pub ts: i64,
}

/// The broadcast hub. Cloning the [`EventBus`] is cheap and shares the
/// underlying channel; pass clones into the HTTP layer and the bridge.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<EventMessage>,
}

impl EventBus {
    /// Create a new bus with the given per-subscriber capacity.
    ///
    /// Pick a capacity large enough to absorb a burst of events while a
    /// subscriber is briefly slow (e.g. 256).
    #[must_use]
    pub fn new(capacity: usize) -> EventBus {
        let (tx, _rx) = broadcast::channel(capacity);
        EventBus { tx }
    }

    /// Subscribe to the bus. Each call returns an independent receiver with
    /// its own queue.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<EventMessage> {
        self.tx.subscribe()
    }

    /// Publish a message to all current subscribers.
    ///
    /// A [`broadcast::error::SendError`] (no subscribers) is silently
    /// ignored — publishing when nobody is listening is a normal condition
    /// (e.g. the bridge hasn't connected yet) and not an error.
    pub fn publish(&self, msg: EventMessage) {
        let _send = self.tx.send(msg);
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(256)
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

    #[tokio::test]
    async fn publish_and_receive_one_message() {
        let bus = EventBus::new(8);
        let mut rx = bus.subscribe();

        let msg = EventMessage {
            agent_id: "a1".to_string(),
            kind: EventKind::ToolCall,
            tool: Some("Bash".to_string()),
            payload: serde_json::json!({"cmd": "ls"}),
            ts: 1_700_000_000_000,
        };

        bus.publish(msg.clone());

        let received = rx.recv().await.expect("should receive a message");
        assert_eq!(received, msg);
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_is_ok() {
        // No subscriber at all; publish must not panic or error.
        let bus = EventBus::new(8);
        bus.publish(EventMessage {
            agent_id: "a1".to_string(),
            kind: EventKind::Thought,
            tool: None,
            payload: serde_json::Value::Null,
            ts: 1,
        });
        // Reaching this point without panicking means the test passes.
    }

    #[tokio::test]
    async fn multiple_subscribers_each_receive() {
        let bus = EventBus::new(8);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.publish(EventMessage {
            agent_id: "a".to_string(),
            kind: EventKind::Response,
            tool: None,
            payload: serde_json::Value::Null,
            ts: 5,
        });

        let m1 = rx1.recv().await.expect("rx1");
        let m2 = rx2.recv().await.expect("rx2");
        assert_eq!(m1.agent_id, "a");
        assert_eq!(m2.agent_id, "a");
    }
}
