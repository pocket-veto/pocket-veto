//! `pocket-veto hook` — the universal Cursor / Claude Code hook adapter.
//!
//! Both agent hosts spawn a short-lived process per hook invocation, feed it a
//! JSON event on stdin, and read a JSON decision (or nothing) from stdout.
//! This subcommand normalizes the event via [`pocket_veto_core::normalize`], branches on
//! whether the event is **blocking** ([`CanonicalEvent::PreToolUse`]) or
//! **fire-and-forget** (everything else), talks to the local `pocket-veto
//! serve` instance over HTTP, and prints the host-specific stdout shape.
//!
//! # Flow
//!
//! 1. Read all of stdin as JSON (blocking std read; the process is ephemeral).
//! 2. [`pocket_veto_core::normalize::normalize`] -> [`InternalEvent`] (auto-detect host).
//! 3. Branch on the canonical event:
//!
//!    - `PreToolUse` (blocking):
//!      - Load config (fail-closed if it errors).
//!      - `POST /approvals` -> `GET /approvals/:id/wait?timeout=...`.
//!      - Map the `WaitOutcome` to [`Decision`] and emit
//!        [`pocket_veto_core::output::to_stdout`]. `WaitOutcome::Timeout` ->
//!        `Deny` (fail-closed).
//!      - Exit 0 on `allow`/`ask`, exit 2 on `deny`/`timeout`/error.
//!
//!    - Other events (fire-and-forget):
//!      - Load config (exit 0 silently on error).
//!      - `POST /events` with the normalized payload.
//!      - Exit 0 silently regardless of HTTP success (telemetry must never
//!        block the agent).
//!
//! # Fail-closed decision matrix
//!
//! - stdin JSON parse error: exit 0 + `tracing::error` log (the event's
//!   blocking classification is unknown before normalize succeeds).
//! - `normalize` error: same as stdin parse error.
//! - config load error: blocking -> `fail_closed_stdout` + exit 2;
//!   non-blocking -> exit 0 silently.
//! - server unreachable: blocking -> `fail_closed_stdout` + exit 2;
//!   non-blocking -> exit 0 silently.
//! - wait timeout: blocking -> `fail_closed_stdout` + exit 2; non-blocking ->
//!   n/a.
//!
//! # Testability
//!
//! [`Subcommand::run`] (the [`crate::cli::HookArgs`] impl) is a thin wrapper
//! that reads stdin, loads config, builds a `reqwest::Client`, and delegates
//! to [`run_with_input`]. All decision logic lives in [`run_with_input`],
//! which is unit-testable without spawning a process: tests build a
//! [`pocket_veto_core::config::Config`] pointing at a real ephemeral server and pass a
//! JSON [`Value`] directly.

use std::process::{ExitCode, Termination};
use std::time::Duration;

use anyhow::Context;
use pocket_veto_core::config::Config;
use pocket_veto_core::normalize::{CanonicalEvent, InternalEvent};
use pocket_veto_core::output::{Decision, fail_closed_stdout, to_stdout};
use pocket_veto_core::protocol::Host;
use serde_json::{Value, json};
use tracing::{error, warn};

use crate::cli::{Ctx, HookArgs, Subcommand};

/// Exit code returned by the hook on a deny / fail-closed outcome.
pub const EXIT_DENY: i32 = 2;

/// Exit code returned by the hook on allow / ask / non-blocking success.
pub const EXIT_OK: i32 = 0;

/// Per-request HTTP timeout for `POST /events` and `POST /approvals`. Kept
/// short so a wedged server fails fast and (for blocking events) the hook
/// fails closed quickly rather than hanging the agent.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Small buffer added on top of `config.approval_timeout_seconds` for the
/// `GET /approvals/:id/wait` per-request timeout, so a server that long-polls
/// up to the configured timeout still returns before the reqwest client gives
/// up.
const WAIT_TIMEOUT_BUFFER: Duration = Duration::from_secs(2);

/// The outcome of running the hook against a single input. Returned by
/// [`run_with_input`] so the decision logic is unit-testable without touching
/// stdout or process exit codes. Implements [`Termination`] so
/// [`Subcommand::run`] can hand it (or its [`ExitCode`]) straight back to
/// `main` without any `process::exit`.
///
/// `stdout` is the already-serialized JSON string the hook would print (for
/// blocking decisions). It is `None` for non-blocking and error paths that
/// emit nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// A blocking event resolved to `allow`. Print the JSON, exit 0.
    Allow { stdout: String },
    /// A blocking event resolved to `deny` (or timeout / fail-closed).
    /// Print the JSON, exit 2.
    Deny { stdout: String },
    /// A blocking event resolved to `ask`. Print the JSON, exit 0.
    Ask { stdout: String },
    /// A non-blocking event was posted to `POST /events` successfully.
    /// Print nothing, exit 0.
    FireAndForgetOk,
    /// A non-blocking event could not be posted (HTTP error). Telemetry
    /// failures are silent: print nothing, exit 0.
    FireAndForgetError,
    /// stdin was empty or not valid JSON. Print nothing, exit 0.
    ParseError,
    /// `normalize` rejected the event (unknown event / missing fields).
    /// Print nothing, exit 0.
    NormalizeError,
    /// Config could not be loaded. For blocking events this is mapped to
    /// `Deny` by the caller; for non-blocking events the caller exits 0.
    /// `stdout` carries the fail-closed JSON for the blocking case.
    ConfigError { stdout: Option<String> },
}

impl HookOutcome {
    /// The numeric exit code this outcome maps to. Kept as `i32` (rather than
    /// [`ExitCode`]) so unit tests can assert against it with plain integer
    /// literals; [`Termination::report`] converts it to [`ExitCode`].
    ///
    /// Exit code policy:
    /// - `Allow` / `Ask` (blocking, non-deny): exit 0.
    /// - `Deny` (blocking deny / timeout / fail-closed): exit 2.
    /// - Non-blocking outcomes (`FireAndForgetOk`, `FireAndForgetError`,
    ///   `ParseError`, `NormalizeError`): exit 0 (telemetry must never block
    ///   the agent).
    /// - `ConfigError`: exit 2 if the caller kept a fail-closed stdout
    ///   (blocking case), else exit 0.
    #[allow(
        clippy::match_same_arms,
        reason = "Allow/Ask and the non-blocking outcomes both exit 0 but for \
                  distinct semantic reasons; merging them would hide the policy"
    )]
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Allow { .. } | Self::Ask { .. } => EXIT_OK,
            Self::Deny { .. } => EXIT_DENY,
            Self::FireAndForgetOk => EXIT_OK,
            Self::FireAndForgetError => EXIT_OK,
            Self::ParseError => EXIT_OK,
            Self::NormalizeError => EXIT_OK,
            Self::ConfigError { stdout } => {
                if stdout.is_some() {
                    EXIT_DENY
                } else {
                    EXIT_OK
                }
            }
        }
    }
}

/// Map a [`HookOutcome`] to the process [`ExitCode`] `main` returns. This is
/// the bridge between the hook's typed outcome and `std::process::Termination`
/// — `Result` main + `Termination`, no `process::exit`.
///
/// `Allow` / `Ask` / non-blocking outcomes -> [`ExitCode::SUCCESS`]; `Deny`
/// and blocking `ConfigError` -> `ExitCode::from(EXIT_DENY)` (exit 2).
impl Termination for HookOutcome {
    fn report(self) -> ExitCode {
        match self.exit_code() {
            EXIT_OK => ExitCode::SUCCESS,
            // EXIT_DENY is the only non-zero code (2) and fits in u8; the
            // `unwrap_or` is a defensive fallback if a future code exceeds
            // u8 range (it keeps the fail-closed exit-2 semantics).
            _ => ExitCode::from(u8::try_from(EXIT_DENY).unwrap_or(2)),
        }
    }
}

impl Subcommand for HookArgs {
    /// Entry point for `pocket-veto hook`. Reads stdin, loads config, builds a
    /// `reqwest::Client`, delegates to [`run_with_input`], prints any stdout
    /// JSON, and returns the outcome's [`ExitCode`] (via [`Termination`]).
    ///
    /// # Errors
    ///
    /// Never returns `Err`: every failure path is mapped to a [`HookOutcome`]
    /// and thus an [`ExitCode`]. The `anyhow::Result` wrapper exists only to
    /// satisfy the [`Subcommand`] trait signature.
    // justification: the hook's run loop is an inherently-sequential pipeline
    // (read stdin -> parse -> load config -> POST approval -> poll decision ->
    // print outcome); each step branches on error/user-deny, and the branching
    // maps 1:1 to the hook's lifecycle.
    #[allow(clippy::cognitive_complexity)]
    async fn run(&self, _ctx: &Ctx) -> anyhow::Result<ExitCode> {
        // 1. Read all of stdin. The hook process is short-lived; a blocking
        //    std read is simpler and avoids tokio-io-on-stdin subtleties.
        let mut raw = String::new();
        if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw) {
            error!(error = %e, "hook: failed to read stdin");
            return Ok(ExitCode::SUCCESS);
        }

        // 2. Parse stdin as JSON. Empty input -> ParseError (treated as a
        //    non-blocking no-op; see the fail-closed matrix in the module docs).
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            error!("hook: stdin is empty, nothing to do");
            return Ok(HookOutcome::ParseError.report());
        }
        let input: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                error!(error = %e, "hook: stdin is not valid JSON");
                return Ok(HookOutcome::ParseError.report());
            }
        };

        // 3. Normalize (no config needed). On failure, exit 0 silently.
        let internal = match pocket_veto_core::normalize::normalize(&input) {
            Ok(ev) => ev,
            Err(e) => {
                warn!(error = %e, "hook: normalize rejected event");
                return Ok(HookOutcome::NormalizeError.report());
            }
        };

        // 4. Load config. For blocking events a failure here is fail-closed.
        let config = match Config::config_path().and_then(|p| Config::load(&p)) {
            Ok(c) => c,
            Err(e) => {
                error!(error = %e, "hook: config load failed");
                let outcome = if internal.event_name.is_blocking() {
                    HookOutcome::ConfigError {
                        stdout: Some(fail_closed_json(&internal)),
                    }
                } else {
                    HookOutcome::ConfigError { stdout: None }
                };
                if let Some(json) = outcome_stdout(&outcome) {
                    println!("{json}");
                }
                return Ok(outcome.report());
            }
        };

        // 5. Build a reqwest client and delegate to the testable core.
        let client = match reqwest::Client::builder().timeout(HTTP_TIMEOUT).build() {
            Ok(c) => c,
            Err(e) => {
                error!(error = %e, "hook: could not build reqwest client");
                let outcome = if internal.event_name.is_blocking() {
                    HookOutcome::Deny {
                        stdout: fail_closed_json(&internal),
                    }
                } else {
                    HookOutcome::FireAndForgetError
                };
                if let Some(json) = outcome_stdout(&outcome) {
                    println!("{json}");
                }
                return Ok(outcome.report());
            }
        };

        let outcome = run_with_input(&internal, &config, &client).await;
        if let Some(json) = outcome_stdout(&outcome) {
            println!("{json}");
        }
        Ok(outcome.report())
    }
}

/// Serialize the fail-closed stdout JSON for a blocking event, falling back to
/// a hand-written deny blob if even serialization fails (so this can never
/// itself error).
fn fail_closed_json(internal: &InternalEvent) -> String {
    serde_json::to_string(&fail_closed_stdout(internal.host, internal.event_name))
        .unwrap_or_else(|_| FALLBACK_DENY_JSON.to_string())
}

/// Minimal deny JSON printed if even serialization of the fail-closed shape
/// fails. Kept hand-written so it cannot itself fail.
const FALLBACK_DENY_JSON: &str = r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"PocketVeto unreachable"}}"#;

/// Return the stdout JSON string for an outcome, if any. Non-blocking and
/// silent-error outcomes print nothing.
fn outcome_stdout(outcome: &HookOutcome) -> Option<&str> {
    match outcome {
        HookOutcome::Allow { stdout }
        | HookOutcome::Deny { stdout }
        | HookOutcome::Ask { stdout } => Some(stdout.as_str()),
        HookOutcome::ConfigError { stdout } => stdout.as_deref(),
        // Non-blocking and silent-error paths: print nothing.
        HookOutcome::FireAndForgetOk
        | HookOutcome::FireAndForgetError
        | HookOutcome::ParseError
        | HookOutcome::NormalizeError => None,
    }
}

/// The testable core of the hook. Given an already-normalized
/// [`InternalEvent`], a loaded [`Config`], and a `reqwest::Client`, execute
/// the blocking-or-fire-and-forget flow and return a [`HookOutcome`].
///
/// This function performs no I/O outside the supplied `client` (no stdin, no
/// stdout, no `process::exit`), so it can be driven from unit and integration
/// tests. [`Subcommand::run`] is the thin wrapper that wires stdin / config /
/// stdout / exit code around it.
///
/// # Panics
///
/// Never panics; all fallible HTTP and JSON steps are mapped to outcomes.
pub async fn run_with_input(
    internal: &InternalEvent,
    config: &Config,
    client: &reqwest::Client,
) -> HookOutcome {
    if internal.event_name.is_blocking() {
        run_blocking(internal, config, client).await
    } else {
        run_fire_and_forget(internal, config, client).await
    }
}

/// Blocking path: `POST /approvals` -> `GET /approvals/:id/wait`.
///
/// Any HTTP error, non-2xx status, or timeout is mapped to a fail-closed
/// [`HookOutcome::Deny`].
async fn run_blocking(
    internal: &InternalEvent,
    config: &Config,
    client: &reqwest::Client,
) -> HookOutcome {
    let agent_id = internal.session_id.as_str();
    let tool = internal
        .tool_name
        .as_deref()
        .unwrap_or("unknown")
        .to_string();
    let summary = build_summary(internal);
    let detail = build_detail(internal);

    let body = json!({
        "agent_id": agent_id,
        "tool": tool,
        "summary": summary,
        "detail": detail,
    });

    // POST /approvals
    let approval_id = match create_approval(client, config, body).await {
        Ok(id) => id,
        Err(e) => {
            warn!(error = %e, "hook: POST /approvals failed; fail-closed");
            return fail_closed_outcome(internal);
        }
    };

    // GET /approvals/:id/wait?timeout=<secs>
    let (outcome, note) = match wait_approval(client, config, &approval_id).await {
        Ok(w) => w,
        Err(e) => {
            warn!(error = %e, "hook: GET /approvals/wait failed; fail-closed");
            return fail_closed_outcome(internal);
        }
    };

    // Map the typed wait outcome to a Decision + reason.
    let (decision, reason) = map_wait_outcome(outcome, note);
    let value = to_stdout(internal.host, internal.event_name, decision, &reason);
    let stdout = serde_json::to_string(&value).unwrap_or_else(|_| FALLBACK_DENY_JSON.to_string());
    match decision {
        Decision::Allow => HookOutcome::Allow { stdout },
        Decision::Ask => HookOutcome::Ask { stdout },
        // Deny, and timeout/defer (mapped to Deny): exit 2.
        Decision::Deny | Decision::Defer => HookOutcome::Deny { stdout },
    }
}

/// Fire-and-forget path: `POST /events`. Any HTTP error is swallowed and
/// mapped to [`HookOutcome::FireAndForgetError`] (the caller still exits 0).
async fn run_fire_and_forget(
    internal: &InternalEvent,
    config: &Config,
    client: &reqwest::Client,
) -> HookOutcome {
    let kind = map_canonical_to_kind(internal.event_name);
    let body = json!({
        "agent_id": internal.session_id,
        "session_id": internal.session_id,
        "host": match internal.host {
            Host::Cursor => "cursor",
            Host::Claude => "claude",
        },
        "workspace": internal.cwd,
        "kind": kind,
        "tool": internal.tool_name,
        "payload": internal.raw,
        "ts": crate::now_ms(),
    });

    match post_events(client, config, body).await {
        Ok(()) => HookOutcome::FireAndForgetOk,
        Err(e) => {
            warn!(error = %e, "hook: POST /events failed (non-blocking, silent)");
            HookOutcome::FireAndForgetError
        }
    }
}

/// Build the fail-closed outcome for a blocking event.
fn fail_closed_outcome(internal: &InternalEvent) -> HookOutcome {
    let stdout = fail_closed_json(internal);
    HookOutcome::Deny { stdout }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map a [`CanonicalEvent`] to the `kind` string the server's `POST /events`
/// handler expects (mirroring the `events.kind` column vocabulary).
///
/// - `PreToolUse` / `PostToolUse` -> `"tool_call"`
/// - `AgentThought` -> `"thought"`
/// - `Stop` / `SessionEnd` -> `"agent_end"` (signals the server to emit an
///   `AgentEnd` frame and mark the agent row `completed`)
/// - `SessionStart` -> `"agent_start"` (signals the server that a session is
///   beginning; combined with the server's `announced_agents` set this still
///   yields exactly one `AgentStart` per agent)
/// - `AgentResponse` -> `"response"`
///
/// These are the same strings [`pocket_veto_core::db`] stores and the BT bridge
/// forwards; unmapped variants default to `"tool_call"` (the most common
/// kind) so a future canonical event still lands as a useful row.
fn map_canonical_to_kind(event: CanonicalEvent) -> &'static str {
    match event {
        CanonicalEvent::PreToolUse | CanonicalEvent::PostToolUse => "tool_call",
        CanonicalEvent::AgentThought => "thought",
        CanonicalEvent::Stop | CanonicalEvent::SessionEnd => "agent_end",
        CanonicalEvent::SessionStart => "agent_start",
        CanonicalEvent::AgentResponse => "response",
    }
}

/// Build a short human-readable summary for an approval request. Kept compact
/// so it fits in a notification title: `<Event>: <tool> — <input snippet>`.
fn build_summary(internal: &InternalEvent) -> String {
    let tool = internal.tool_name.as_deref().unwrap_or("unknown");
    let mut summary = format!("{}: {}", internal.event_name.as_pascal_case(), tool);
    if let Some(input) = &internal.tool_input {
        let snippet = serde_json::to_string(input).unwrap_or_default();
        // Truncate at a char boundary so a multibyte UTF-8 sequence in the
        // serialized input cannot panic the byte slice.
        let snippet = if snippet.chars().count() > SUMMARY_INPUT_MAX {
            let truncated: String = snippet.chars().take(SUMMARY_INPUT_MAX).collect();
            format!("{truncated}…")
        } else {
            snippet
        };
        if !snippet.is_empty() {
            summary.push_str(" — ");
            summary.push_str(&snippet);
        }
    }
    summary
}

/// Maximum number of characters of the serialized `tool_input` to include in
/// the summary before truncating with an ellipsis.
const SUMMARY_INPUT_MAX: usize = 100;

/// Build the `detail` field for an approval request: the full `tool_input`
/// JSON string, or `None` when there is no tool input.
fn build_detail(internal: &InternalEvent) -> Option<String> {
    internal
        .tool_input
        .as_ref()
        .map(|input| serde_json::to_string(input).unwrap_or_default())
        .filter(|s| !s.is_empty())
}

/// The typed outcome of a `/approvals/:id/wait` long-poll. The timeout
/// special-case is a variant, not a magic string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    /// The server returned a concrete decision (`allow` / `deny` / `ask` /
    /// `defer`). An unknown decision string is folded into [`Decision::Deny`]
    /// by the parser so a malformed server response can never accidentally
    /// allow a risky tool.
    Approved(Decision),
    /// The long-poll expired with no decision. The caller maps this to
    /// [`Decision::Deny`] (fail-closed).
    Timeout,
}

/// `POST /approvals`. Returns the new `approval_id`.
///
/// # Errors
///
/// Returns `anyhow::Error` if the request fails, the status is not 2xx, or
/// the response body lacks an `approval_id` string.
async fn create_approval(
    client: &reqwest::Client,
    config: &Config,
    body: Value,
) -> anyhow::Result<String> {
    let url = format!("{}/approvals", config.server_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .bearer_auth(config.token.as_ref())
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("POST {url} returned {status}: {text}");
    }
    let value: Value = resp.json().await.context("decode approval response")?;
    let id = value
        .get("approval_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("approval response missing `approval_id`"))?
        .to_string();
    Ok(id)
}

/// `GET /approvals/:id/wait?timeout=<secs>`. Uses a per-request timeout of
/// `config.approval_timeout_seconds + WAIT_TIMEOUT_BUFFER` so the server's
/// long-poll can run to the configured limit before the client gives up.
///
/// Returns the typed [`WaitOutcome`] plus the optional `note` the server
/// surfaced alongside the decision.
///
/// # Errors
///
/// Returns `anyhow::Error` if the request fails, the status is not 2xx, or
/// the response body lacks a `decision` string.
async fn wait_approval(
    client: &reqwest::Client,
    config: &Config,
    approval_id: &str,
) -> anyhow::Result<(WaitOutcome, Option<String>)> {
    let base = config.server_url.trim_end_matches('/');
    let timeout_secs = config.approval_timeout_seconds;
    let url = format!("{base}/approvals/{approval_id}/wait?timeout={timeout_secs}");
    let per_req_timeout = WAIT_TIMEOUT_BUFFER
        .checked_add(Duration::from_secs(timeout_secs))
        .unwrap_or(Duration::from_secs(timeout_secs));
    let resp = client
        .get(&url)
        .bearer_auth(config.token.as_ref())
        .timeout(per_req_timeout)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("GET {url} returned {status}: {text}");
    }
    let value: Value = resp.json().await.context("decode wait response")?;
    let decision_str = value
        .get("decision")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("wait response missing `decision`"))?;
    let note = value.get("note").and_then(Value::as_str).map(String::from);
    // The wire `decision` is `allow`/`deny`/`ask`/`defer`/`timeout`. The
    // timeout special-case becomes a variant; anything else parses via the
    // core `Decision` FromStr, folding unknowns into `Deny` (fail-closed) so a
    // malformed response can never accidentally allow a risky tool.
    let outcome = if decision_str == "timeout" {
        WaitOutcome::Timeout
    } else {
        WaitOutcome::Approved(decision_str.parse::<Decision>().unwrap_or(Decision::Deny))
    };
    Ok((outcome, note))
}

/// `POST /events`. Fire-and-forget; any error is surfaced to the caller.
///
/// # Errors
///
/// Returns `anyhow::Error` if the request fails or the status is not 2xx.
async fn post_events(client: &reqwest::Client, config: &Config, body: Value) -> anyhow::Result<()> {
    let url = format!("{}/events", config.server_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .bearer_auth(config.token.as_ref())
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("POST {url} returned {status}: {text}");
    }
    Ok(())
}

/// Map a typed [`WaitOutcome`] (plus the optional server `note`) to a
/// [`Decision`] + reason string. The timeout case is a variant, not a
/// magic string.
///
/// Policy:
/// - [`WaitOutcome::Approved`] of `Allow` / `Ask` -> kept as-is.
/// - [`WaitOutcome::Approved`] of `Deny` / `Defer` -> `Deny` (fail-closed;
///   `Defer` is treated as deny at this layer — everything except
///   `allow`/`ask` maps to deny).
/// - [`WaitOutcome::Timeout`] -> `Deny` (fail-closed).
///
/// When the note is empty and the decision is `Deny`, a reason is synthesized
/// so the agent/user sees something actionable (`"timed out ..."` for
/// timeouts, `"PocketVeto: denied"` otherwise).
fn map_wait_outcome(outcome: WaitOutcome, note: Option<String>) -> (Decision, String) {
    let (raw, is_timeout) = match outcome {
        WaitOutcome::Approved(d) => (d, false),
        WaitOutcome::Timeout => (Decision::Deny, true),
    };
    // Fail-closed: only Allow/Ask survive; Deny/Defer both land on Deny.
    let decision = match raw {
        Decision::Allow | Decision::Ask => raw,
        Decision::Deny | Decision::Defer => Decision::Deny,
    };
    let reason = note.unwrap_or_default();
    let reason = if reason.is_empty() && matches!(decision, Decision::Deny) {
        if is_timeout {
            "PocketVeto: approval timed out (denying for safety)".to_string()
        } else {
            "PocketVeto: denied".to_string()
        }
    } else {
        reason
    };
    (decision, reason)
}

/// Extension on [`CanonicalEvent`] for the blocking-vs-fire-and-forget split.
trait CanonicalEventExt {
    /// Returns `true` for the `PreToolUse` family (the only blocking event).
    fn is_blocking(&self) -> bool;
}

impl CanonicalEventExt for CanonicalEvent {
    fn is_blocking(&self) -> bool {
        matches!(self, CanonicalEvent::PreToolUse)
    }
}

// ---------------------------------------------------------------------------
// Unit tests for the pure helpers (no network).
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
    use pocket_veto_core::normalize::InternalEvent;
    use pocket_veto_core::protocol::Host;
    use serde_json::json;

    fn claude_pre_tool_use(tool: &str, input: Value) -> InternalEvent {
        InternalEvent {
            host: Host::Claude,
            event_name: CanonicalEvent::PreToolUse,
            session_id: "sess-1".to_string(),
            cwd: "/tmp".to_string(),
            tool_name: Some(tool.to_string()),
            tool_input: Some(input),
            raw: json!({}),
        }
    }

    #[test]
    fn map_canonical_to_kind_covers_all_variants() {
        assert_eq!(
            map_canonical_to_kind(CanonicalEvent::PreToolUse),
            "tool_call"
        );
        assert_eq!(
            map_canonical_to_kind(CanonicalEvent::PostToolUse),
            "tool_call"
        );
        assert_eq!(
            map_canonical_to_kind(CanonicalEvent::AgentThought),
            "thought"
        );
        assert_eq!(map_canonical_to_kind(CanonicalEvent::Stop), "agent_end");
        assert_eq!(
            map_canonical_to_kind(CanonicalEvent::SessionStart),
            "agent_start"
        );
        assert_eq!(
            map_canonical_to_kind(CanonicalEvent::SessionEnd),
            "agent_end"
        );
        assert_eq!(
            map_canonical_to_kind(CanonicalEvent::AgentResponse),
            "response"
        );
    }

    #[test]
    fn build_summary_includes_tool_and_truncated_input() {
        let big = "x".repeat(SUMMARY_INPUT_MAX + 50);
        let ev = claude_pre_tool_use("Bash", json!({ "command": big }));
        let s = build_summary(&ev);
        assert!(s.starts_with("PreToolUse: Bash — "));
        assert!(s.contains('…'));
        // The snippet is the first SUMMARY_INPUT_MAX bytes of the serialized
        // tool_input followed by a single ellipsis char (which is 3 bytes in
        // UTF-8). Assert on char count to be byte-width-agnostic.
        let snippet = s.split(" — ").nth(1).unwrap();
        assert_eq!(snippet.chars().count(), SUMMARY_INPUT_MAX + 1);
        // And the original big string must not appear in full.
        assert!(!s.contains(&big));
    }

    #[test]
    fn build_summary_omits_input_when_absent() {
        let mut ev = claude_pre_tool_use("Bash", json!({}));
        ev.tool_input = None;
        let s = build_summary(&ev);
        assert_eq!(s, "PreToolUse: unknown".replace("unknown", "Bash"));
    }

    #[test]
    fn build_detail_serializes_tool_input() {
        let ev = claude_pre_tool_use("Bash", json!({ "command": "ls" }));
        assert_eq!(build_detail(&ev).as_deref(), Some(r#"{"command":"ls"}"#));
    }

    #[test]
    fn build_detail_is_none_when_no_input() {
        let mut ev = claude_pre_tool_use("Bash", json!({}));
        ev.tool_input = None;
        assert!(build_detail(&ev).is_none());
    }

    #[test]
    fn map_wait_outcome_allow_ask_deny_timeout() {
        let (d, r) = map_wait_outcome(WaitOutcome::Approved(Decision::Allow), Some("ok".into()));
        assert_eq!(d, Decision::Allow);
        assert_eq!(r, "ok");

        let (d, r) = map_wait_outcome(WaitOutcome::Approved(Decision::Ask), None);
        assert_eq!(d, Decision::Ask);
        assert!(r.is_empty());

        let (d, r) = map_wait_outcome(WaitOutcome::Approved(Decision::Deny), Some("no".into()));
        assert_eq!(d, Decision::Deny);
        assert_eq!(r, "no");

        // Deny with no note synthesizes a reason.
        let (d, r) = map_wait_outcome(WaitOutcome::Approved(Decision::Deny), None);
        assert_eq!(d, Decision::Deny);
        assert!(r.contains("denied"));

        // Defer is fail-closed to Deny.
        let (d, _) = map_wait_outcome(WaitOutcome::Approved(Decision::Defer), None);
        assert_eq!(d, Decision::Deny);

        // Timeout -> Deny + "timed out" reason.
        let (d, r) = map_wait_outcome(WaitOutcome::Timeout, None);
        assert_eq!(d, Decision::Deny);
        assert!(r.contains("timed out"));

        // Timeout with a note keeps the note.
        let (d, r) = map_wait_outcome(WaitOutcome::Timeout, Some("user note".into()));
        assert_eq!(d, Decision::Deny);
        assert_eq!(r, "user note");
    }

    #[test]
    fn outcome_exit_codes_match_plan() {
        assert_eq!(
            HookOutcome::Allow { stdout: "x".into() }.exit_code(),
            EXIT_OK
        );
        assert_eq!(HookOutcome::Ask { stdout: "x".into() }.exit_code(), EXIT_OK);
        assert_eq!(
            HookOutcome::Deny { stdout: "x".into() }.exit_code(),
            EXIT_DENY
        );
        assert_eq!(HookOutcome::FireAndForgetOk.exit_code(), EXIT_OK);
        assert_eq!(HookOutcome::FireAndForgetError.exit_code(), EXIT_OK);
        assert_eq!(HookOutcome::ParseError.exit_code(), EXIT_OK);
        assert_eq!(HookOutcome::NormalizeError.exit_code(), EXIT_OK);
        assert_eq!(
            HookOutcome::ConfigError {
                stdout: Some("x".into())
            }
            .exit_code(),
            EXIT_DENY
        );
        assert_eq!(
            HookOutcome::ConfigError { stdout: None }.exit_code(),
            EXIT_OK
        );
    }

    #[test]
    fn termination_report_compiles_and_maps_deny() {
        // `ExitCode` does not expose its raw value and does not implement
        // `PartialEq`, so equality cannot be asserted directly. Instead
        // `report()` is confirmed to run (the Termination impl compiles +
        // does not panic) for each outcome class; the numeric mapping is
        // already covered by `outcome_exit_codes_match_plan`, which
        // `report()` delegates to.
        let _ = HookOutcome::Allow { stdout: "x".into() }.report();
        let _ = HookOutcome::Deny { stdout: "x".into() }.report();
        let _ = HookOutcome::FireAndForgetOk.report();
        let _ = HookOutcome::ConfigError { stdout: None }.report();
        let _ = HookOutcome::ConfigError {
            stdout: Some("x".into()),
        }
        .report();
    }

    #[test]
    fn outcome_stdout_only_for_blocking_decisions() {
        assert_eq!(
            outcome_stdout(&HookOutcome::Allow { stdout: "a".into() }),
            Some("a")
        );
        assert_eq!(
            outcome_stdout(&HookOutcome::Deny { stdout: "d".into() }),
            Some("d")
        );
        assert_eq!(
            outcome_stdout(&HookOutcome::Ask { stdout: "k".into() }),
            Some("k")
        );
        assert_eq!(
            outcome_stdout(&HookOutcome::ConfigError {
                stdout: Some("c".into())
            }),
            Some("c")
        );
        assert_eq!(
            outcome_stdout(&HookOutcome::ConfigError { stdout: None }),
            None
        );
        assert_eq!(outcome_stdout(&HookOutcome::FireAndForgetOk), None);
        assert_eq!(outcome_stdout(&HookOutcome::FireAndForgetError), None);
        assert_eq!(outcome_stdout(&HookOutcome::ParseError), None);
        assert_eq!(outcome_stdout(&HookOutcome::NormalizeError), None);
    }
}
