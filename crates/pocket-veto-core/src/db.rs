//! `SQLite`-backed persistence for agents, events, and approvals.
//!
//! Schema is applied on [`Db::open`] via `CREATE TABLE IF NOT EXISTS`, so the
//! database file is self-initializing and idempotent across restarts. The
//! inner [`rusqlite::Connection`] is wrapped in a [`std::sync::Mutex`] because
//! `Connection` is `!Sync` — the mutex makes [`Db`] `Send + Sync` so it can
//! live behind an `Arc` in axum state and be shared across the HTTP handlers
//! and the BT bridge task.
//!
//! All methods take `&self` and lock the inner connection briefly via the
//! private `Db::with_conn` helper, never holding the lock across an `await`
//! (rusqlite is synchronous anyway). Poisoned-mutex recovery is centralized
//! in `Db::with_conn`.
//!
//! # Domain types in row structs
//!
//! The public row types use the newtypes/enums where it is clean to
//! do so: [`AgentId`]/[`ApprovalId`] for id columns, [`TimestampMs`] for
//! `INTEGER` millisecond columns, and [`AgentStatus`] for the
//! `agents.status` column (bound directly via `ToSql`/`FromSql`). The
//! `agents.host` and `events.kind` columns are intentionally kept as
//! `String`: `agents.host` may carry arbitrary host tags sent in the HTTP
//! `/events` body (the bridge defaults unknowns to `Claude` rather than
//! failing), and `events.kind` also stores the lifecycle markers
//! `agent_start`/`agent_end` which are not members of the wire
//! [`EventKind`](crate::protocol::EventKind) enum. Adopting `FromSql` for
//! those would turn a previously-tolerated unknown value into a hard query
//! error, so they are converted at the boundary instead (see the crate-level
//! `AGENTS.md` for the deferral rationale).

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::error::{CoreError, Result};
use crate::protocol::{AgentId, AgentStatus, ApprovalId, TimestampMs};

/// Wrapper around a single `SQLite` connection, safe to share across threads.
pub struct Db {
    conn: Mutex<Connection>,
}

/// SQL applied on open.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS agents (
  agent_id TEXT PRIMARY KEY,
  session_id TEXT,
  host TEXT NOT NULL,
  name TEXT,
  workspace TEXT,
  status TEXT NOT NULL,
  started_at INTEGER NOT NULL,
  ended_at INTEGER
);

CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  agent_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  tool TEXT,
  payload TEXT NOT NULL,
  ts INTEGER NOT NULL,
  FOREIGN KEY (agent_id) REFERENCES agents(agent_id)
);

CREATE INDEX IF NOT EXISTS idx_events_agent_ts ON events(agent_id, ts);

CREATE TABLE IF NOT EXISTS approvals (
  approval_id TEXT PRIMARY KEY,
  agent_id TEXT NOT NULL,
  tool TEXT NOT NULL,
  summary TEXT NOT NULL,
  detail TEXT,
  status TEXT NOT NULL,
  decision_note TEXT,
  created_at INTEGER NOT NULL,
  decided_at INTEGER
);
";

impl Db {
    /// Open a database at `path`, applying the schema if needed.
    ///
    /// Pass `:memory:` for an ephemeral in-process database (used by tests).
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Db`] if the connection cannot be opened, the
    /// schema cannot be applied, or the `foreign_keys` pragma cannot be set.
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open_with_flags(path, OpenFlags::default())?;
        conn.execute_batch(SCHEMA)?;
        // Enable foreign keys so the events.agent_id FK is enforced.
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    /// Open an in-memory database (test helper, but also useful for ephemeral
    /// runs). Equivalent to `Db::open(Path::new(":memory:"))`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Db`] if the in-memory connection cannot be opened
    /// or the schema cannot be applied.
    pub fn open_in_memory() -> Result<Db> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    /// Lock the inner connection and run `f` against it, centralizing the
    /// poisoned-mutex recovery (a prior panic's payload is recovered via
    /// `into_inner` so a poisoned lock never blocks a subsequent query).
    /// Every public [`Db`] method routes through this helper so the
    /// lock-recovery boilerplate is not duplicated.
    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&conn)
    }

    /// Insert or replace an agent row. `status` is bound directly as the
    /// [`AgentStatus`] enum (its `ToSql` impl writes the canonical
    /// `snake_case` `agents.status` string).
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Db`] if the `INSERT OR REPLACE` fails.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_agent(
        &self,
        agent_id: &str,
        session_id: &str,
        host: &str,
        name: &str,
        workspace: &str,
        status: AgentStatus,
        started_at: i64,
        ended_at: Option<i64>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO agents \
                 (agent_id, session_id, host, name, workspace, status, started_at, ended_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    agent_id, session_id, host, name, workspace, status, started_at, ended_at
                ],
            )?;
            Ok(())
        })
    }

    /// Append an event row. Returns the autoincrement `id`.
    ///
    /// `kind` stays a `&str` (not the [`EventKind`](crate::protocol::EventKind)
    /// enum) because the column also stores the lifecycle markers
    /// `agent_start`/`agent_end`, which are not wire `EventKind` variants.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Protocol`] if `payload` cannot be serialized to
    /// JSON, or [`CoreError::Db`] if the `INSERT` fails.
    pub fn record_event(
        &self,
        agent_id: &str,
        kind: &str,
        tool: Option<&str>,
        payload: &serde_json::Value,
        ts: i64,
    ) -> Result<i64> {
        let payload_str = serde_json::to_string(payload)?;
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO events (agent_id, kind, tool, payload, ts) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![agent_id, kind, tool, payload_str, ts],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// Insert a pending approval row.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Db`] if the `INSERT` fails.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_approval(
        &self,
        approval_id: &str,
        agent_id: &str,
        tool: &str,
        summary: &str,
        detail: Option<&str>,
        created_at: i64,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO approvals \
                 (approval_id, agent_id, tool, summary, detail, status, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
                params![approval_id, agent_id, tool, summary, detail, created_at],
            )?;
            Ok(())
        })
    }

    /// Update an approval row with a decision. `status` should be one of
    /// `allowed` / `denied` / `timeout` (a free-form string — the
    /// `approvals.status` column is not a wire enum, so it stays `&str`).
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Db`] if the `UPDATE` fails.
    pub fn set_approval_decision(
        &self,
        approval_id: &str,
        status: &str,
        note: Option<&str>,
        decided_at: i64,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE approvals SET status = ?1, decision_note = ?2, decided_at = ?3 \
                 WHERE approval_id = ?4",
                params![status, note, decided_at, approval_id],
            )?;
            Ok(())
        })
    }

    /// Fetch a single approval row by id, if present.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Db`] if the query fails for a reason other than
    /// "no rows".
    pub fn pending_approval(&self, approval_id: &str) -> Result<Option<ApprovalRow>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT approval_id, agent_id, tool, summary, detail, status, \
                 decision_note, created_at, decided_at \
                 FROM approvals WHERE approval_id = ?1",
            )?;
            stmt.query_row(params![approval_id], ApprovalRow::from_row)
                .optional()
                .map_err(CoreError::Db)
        })
    }

    /// Replay events for a single agent since `since_ts` (exclusive),
    /// ordered oldest-first. Used by the bridge on reconnect.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Db`] if the query or any row decode fails.
    pub fn events_since(&self, agent_id: &str, since_ts: i64) -> Result<Vec<EventRow>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, agent_id, kind, tool, payload, ts FROM events \
                 WHERE agent_id = ?1 AND ts > ?2 ORDER BY ts ASC, id ASC",
            )?;
            let rows = stmt.query_map(params![agent_id, since_ts], EventRow::from_row)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// Alias of `events_since` (kept for back-compat).
    ///
    /// # Errors
    ///
    /// Propagates any error from `events_since`.
    pub fn agent_history(&self, agent_id: &str, since_ts: i64) -> Result<Vec<EventRow>> {
        self.events_since(agent_id, since_ts)
    }

    /// List all known agents, ordered by `started_at` ascending.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Db`] if the query or any row decode fails.
    pub fn list_agents(&self) -> Result<Vec<AgentRow>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT agent_id, session_id, host, name, workspace, status, \
                 started_at, ended_at FROM agents ORDER BY started_at ASC",
            )?;
            let rows = stmt.query_map([], AgentRow::from_row)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// Fetch a single agent row by id, if present. Used by the BT bridge on
    /// reconnect to reconstruct a [`crate::ServerMessage::AgentStart`] for an
    /// agent the phone already knows about — the `events` table does not carry
    /// the name/host/workspace metadata, so it is read from the `agents` table.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Db`] if the query fails for a reason other than
    /// "no rows".
    pub fn get_agent(&self, agent_id: &str) -> Result<Option<AgentRow>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT agent_id, session_id, host, name, workspace, status, \
                 started_at, ended_at FROM agents WHERE agent_id = ?1",
            )?;
            stmt.query_row(params![agent_id], AgentRow::from_row)
                .optional()
                .map_err(CoreError::Db)
        })
    }
}

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

/// A row from the `approvals` table. `status` stays a `String` because the
/// `approvals.status` column holds `pending`/`allowed`/`denied`/`timeout`,
/// which is not a wire enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRow {
    pub approval_id: ApprovalId,
    pub agent_id: AgentId,
    pub tool: String,
    pub summary: String,
    pub detail: Option<String>,
    pub status: String,
    pub decision_note: Option<String>,
    pub created_at: TimestampMs,
    pub decided_at: Option<TimestampMs>,
}

impl ApprovalRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            approval_id: row.get("approval_id")?,
            agent_id: row.get("agent_id")?,
            tool: row.get("tool")?,
            summary: row.get("summary")?,
            detail: row.get("detail")?,
            status: row.get("status")?,
            decision_note: row.get("decision_note")?,
            created_at: row.get("created_at")?,
            decided_at: row.get("decided_at")?,
        })
    }
}

/// A row from the `events` table. `payload` is parsed back into a JSON value
/// for re-emission on the wire. `kind` stays a `String` (see the module docs
/// for the lifecycle-marker rationale).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    pub id: i64,
    pub agent_id: AgentId,
    pub kind: String,
    pub tool: Option<String>,
    pub payload: serde_json::Value,
    pub ts: TimestampMs,
}

impl EventRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let payload_str: String = row.get("payload")?;
        let payload = serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null);
        Ok(Self {
            id: row.get("id")?,
            agent_id: row.get("agent_id")?,
            kind: row.get("kind")?,
            tool: row.get("tool")?,
            payload,
            ts: row.get("ts")?,
        })
    }
}

/// A row from the `agents` table. `host` stays a `String` (see the module
/// docs for the unknown-host-tag rationale); `status` is the typed
/// [`AgentStatus`] enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub agent_id: AgentId,
    pub session_id: Option<String>,
    pub host: String,
    pub name: Option<String>,
    pub workspace: Option<String>,
    pub status: AgentStatus,
    pub started_at: TimestampMs,
    pub ended_at: Option<TimestampMs>,
}

impl AgentRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            agent_id: row.get("agent_id")?,
            session_id: row.get("session_id")?,
            host: row.get("host")?,
            name: row.get("name")?,
            workspace: row.get("workspace")?,
            status: row.get("status")?,
            started_at: row.get("started_at")?,
            ended_at: row.get("ended_at")?,
        })
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

    #[test]
    fn open_insert_and_read_back_roundtrip() {
        let db = Db::open_in_memory().expect("open in-memory db");

        db.upsert_agent(
            "agent-1",
            "session-1",
            "claude",
            "refactor",
            "/tmp/w",
            AgentStatus::Running,
            1_000,
            None,
        )
        .expect("upsert agent");

        let id = db
            .record_event(
                "agent-1",
                "tool_call",
                Some("Bash"),
                &serde_json::json!({"cmd": "ls"}),
                1_100,
            )
            .expect("record event");
        assert!(id > 0);

        db.insert_approval(
            "ap-1",
            "agent-1",
            "Bash",
            "rm -rf node_modules",
            Some("detail here"),
            1_200,
        )
        .expect("insert approval");

        // Approval read-back.
        let ap = db
            .pending_approval("ap-1")
            .expect("query approval")
            .expect("row exists");
        assert_eq!(ap.approval_id.as_ref(), "ap-1");
        assert_eq!(ap.status, "pending");
        assert_eq!(ap.tool, "Bash");
        assert_eq!(ap.detail.as_deref(), Some("detail here"));
        assert!(ap.decided_at.is_none());

        // Events since ts=1000 should include the inserted event.
        let evs = db.events_since("agent-1", 1_000).expect("events since");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].agent_id.as_ref(), "agent-1");
        assert_eq!(evs[0].kind, "tool_call");
        assert_eq!(evs[0].tool.as_deref(), Some("Bash"));
        assert_eq!(evs[0].payload["cmd"], "ls");

        // events_since is exclusive on ts.
        let evs2 = db
            .events_since("agent-1", 1_100)
            .expect("events since 1100");
        assert!(evs2.is_empty());

        // agent_history mirrors events_since.
        let hist = db.agent_history("agent-1", 1_000).expect("history");
        assert_eq!(hist.len(), 1);

        // Decision updates the row.
        db.set_approval_decision("ap-1", "allowed", Some("ok"), 1_300)
            .expect("set decision");
        let ap2 = db.pending_approval("ap-1").expect("query").expect("row");
        assert_eq!(ap2.status, "allowed");
        assert_eq!(ap2.decision_note.as_deref(), Some("ok"));
        assert_eq!(ap2.decided_at, Some(TimestampMs(1_300)));

        // list_agents returns the one agent.
        let agents = db.list_agents().expect("list agents");
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_id.as_ref(), "agent-1");
        assert_eq!(agents[0].status, AgentStatus::Running);
    }

    #[test]
    fn pending_approval_returns_none_for_missing() {
        let db = Db::open_in_memory().expect("open");
        let r = db.pending_approval("nope").expect("query");
        assert!(r.is_none());
    }

    #[test]
    fn get_agent_returns_some_for_existing_and_none_for_missing() {
        let db = Db::open_in_memory().expect("open");

        // Missing agent -> None.
        assert!(db.get_agent("nope").expect("query missing").is_none());

        // Insert an agent and read it back.
        db.upsert_agent(
            "agent-get",
            "session-get",
            "claude",
            "refactor",
            "/tmp/w",
            AgentStatus::Running,
            1_234,
            None,
        )
        .expect("upsert agent");
        let row = db
            .get_agent("agent-get")
            .expect("query existing")
            .expect("row should exist");
        assert_eq!(row.agent_id.as_ref(), "agent-get");
        assert_eq!(row.session_id.as_deref(), Some("session-get"));
        assert_eq!(row.host, "claude");
        assert_eq!(row.name.as_deref(), Some("refactor"));
        assert_eq!(row.workspace.as_deref(), Some("/tmp/w"));
        assert_eq!(row.status, AgentStatus::Running);
        assert_eq!(row.started_at, TimestampMs(1_234));
        assert_eq!(row.ended_at, None);
    }
}
