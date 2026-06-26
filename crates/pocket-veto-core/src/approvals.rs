//! Approval state machine and decision logic.
//!
//! When the HTTP layer receives `POST /approvals`, it creates a pending row
//! in `SQLite` and calls [`ApprovalWaiters::register`] to obtain a
//! [`oneshot::Receiver`]. The corresponding `GET /approvals/:id/wait`
//! handler awaits that receiver (wrapped in a `tokio::time::timeout`).
//!
//! When the BT bridge delivers an [`ApprovalDecision`](crate::protocol::ClientMessage::ApprovalDecision),
//! the server calls [`ApprovalWaiters::resolve`], which fires the oneshot
//! and unblocks the waiting hook. Timeouts and cleanup use
//! [`ApprovalWaiters::cancel`].

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::oneshot;

use crate::protocol::Decision;

/// In-memory registry of pending approval waiters, keyed by approval id.
///
/// Clonable is *not* required: the server keeps a single instance behind an
/// `Arc` and shares `&self` references across handlers.
pub struct ApprovalWaiters {
    senders: Mutex<HashMap<String, oneshot::Sender<Decision>>>,
}

impl ApprovalWaiters {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> ApprovalWaiters {
        ApprovalWaiters {
            senders: Mutex::new(HashMap::new()),
        }
    }

    /// Lock the inner sender map and run `f` against it, centralizing the
    /// poisoned-mutex recovery (a prior panic's payload is recovered via
    /// `into_inner` so a poisoned lock never blocks a later register/resolve/
    /// cancel). Mirrors [`crate::db::Db::with_conn`].
    fn with_lock<T>(
        &self,
        f: impl FnOnce(&mut HashMap<String, oneshot::Sender<Decision>>) -> T,
    ) -> T {
        let mut map = self
            .senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut map)
    }

    /// Register a waiter for `approval_id`, returning the receiver the
    /// caller should await.
    ///
    /// If a waiter for the same id already exists (e.g. a duplicate request),
    /// the previous sender is dropped — the older `GET .../wait` will then
    /// observe a [`RecvError`](oneshot::error::RecvError) and can exit.
    pub fn register(&self, approval_id: &str) -> oneshot::Receiver<Decision> {
        let (tx, rx) = oneshot::channel();
        self.with_lock(|map| {
            map.insert(approval_id.to_string(), tx);
        });
        rx
    }

    /// Resolve a pending approval, returning `true` if a waiter existed.
    ///
    /// Sending on a dropped receiver (the waiter timed out or disconnected)
    /// is fine — the sender is simply consumed and `true` is still returned
    /// because a waiter *did* exist at resolution time.
    pub fn resolve(&self, approval_id: &str, decision: Decision) -> bool {
        self.with_lock(|map| match map.remove(approval_id) {
            // Ignore the send error: a dropped receiver just means the
            // waiter already went away (timeout/cancel).
            Some(tx) => {
                let _send = tx.send(decision);
                true
            }
            None => false,
        })
    }

    /// Remove a waiter without resolving it (used on timeout / cleanup).
    pub fn cancel(&self, approval_id: &str) {
        self.with_lock(|map| {
            map.remove(approval_id);
        });
    }
}

impl Default for ApprovalWaiters {
    fn default() -> Self {
        Self::new()
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
    async fn register_resolve_and_await() {
        let waiters = ApprovalWaiters::new();
        let rx = waiters.register("ap-1");

        // Resolve from another "task" (here, inline).
        let existed = waiters.resolve("ap-1", Decision::Allow);
        assert!(existed);

        let decision = rx.await.expect("receiver should yield a decision");
        assert_eq!(decision, Decision::Allow);
    }

    #[tokio::test]
    async fn resolve_unknown_returns_false() {
        let waiters = ApprovalWaiters::new();
        assert!(!waiters.resolve("nope", Decision::Deny));
    }

    #[tokio::test]
    async fn cancel_drops_waiter_without_resolving() {
        let waiters = ApprovalWaiters::new();
        let rx = waiters.register("ap-2");
        waiters.cancel("ap-2");

        // After cancel, a resolve must report no waiter.
        assert!(!waiters.resolve("ap-2", Decision::Allow));

        // The receiver observes a Closed error (no value sent).
        let err = rx.await.expect_err("should be closed, not resolved");
        assert!(matches!(err, oneshot::error::RecvError { .. }));
    }

    #[tokio::test]
    async fn re_register_replaces_previous_waiter() {
        let waiters = ApprovalWaiters::new();
        let rx_old = waiters.register("ap-3");
        let _rx_new = waiters.register("ap-3");

        // Resolving now should send to the new waiter; the old receiver
        // should be closed because its sender was dropped on replace.
        let err = rx_old.await.expect_err("old receiver should be closed");
        assert!(matches!(err, oneshot::error::RecvError { .. }));
    }
}
