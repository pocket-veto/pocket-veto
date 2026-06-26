//! Terminal and hook output formatting.
//!
//! Converts a [`Decision`] (plus a reason string) into the host-specific
//! JSON shape the agent host expects on the hook subcommand's stdout.
//!
//! # Shapes
//!
//! - **Cursor** (flat): `{ "permission": "allow"|"deny"|"ask", "user_message": reason, "agent_message": "" }`.
//!   The decision is lowercased. The hook subcommand only emits a decision
//!   for blocking (`PreToolUse`-family) events, so the `permission` field is
//!   always present here.
//! - **Claude Code** (nested): `{ "hookSpecificOutput": { "hookEventName": <PascalCase>, "permissionDecision": "allow"|"deny"|"ask"|"defer", "permissionDecisionReason": reason } }`.
//!   This module emits the modern `hookSpecificOutput` shape, NOT the
//!   deprecated top-level `decision`/`reason` fields (both shapes are
//!   documented in the Claude Code hooks reference).
//!
//! # Compile-checked wire shape
//!
//! The two shapes are produced by [`CursorOutput`] / [`ClaudeOutput`] (typed
//! `#[derive(Serialize)]` structs) rather than hand-written `json!({...})`
//! literals, so the field names and casing are checked at compile time. The
//! [`HookOutput`] enum is `#[serde(untagged)]`, so serializing it yields the
//! flat Cursor object or the nested Claude object directly. Field order
//! follows struct-declaration order (not `BTreeMap`-alphabetical), but the
//! JSON is semantically identical (same keys/values); the integration tests
//! in `crates/pocket-veto/tests/hook_subcommand.rs` parse by key and confirm
//! the shape.
//!
//! # Fail-closed
//!
//! [`fail_closed_stdout`] emits a `Deny` with a "`PocketVeto` unreachable"
//! reason for the hook subcommand's fail-closed path (server unreachable or
//! internal error on a blocking event). The hook then exits with code 2.

use serde::Serialize;

use crate::normalize::CanonicalEvent;
use crate::protocol::Host;

// Re-export Decision so callers can use `pocket_veto_core::output::Decision` without
// reaching into `protocol` directly.
pub use crate::protocol::Decision;

/// Reason string used by [`fail_closed_stdout`].
const FAIL_CLOSED_REASON: &str = "PocketVeto unreachable: denying for safety";

/// Flat Cursor output shape: `permission` / `user_message` / `agent_message`.
///
/// Field names are already `snake_case`, matching the wire JSON, so no
/// `rename_all` is needed. The `permission` tag is the `Decision::tag`
/// lowercase string; `agent_message` is always the empty string (the hook
/// never populates it).
#[derive(Serialize)]
pub struct CursorOutput<'a> {
    permission: &'static str,
    user_message: &'a str,
    agent_message: &'a str,
}

/// The nested `hookSpecificOutput` object Claude Code expects.
///
/// Field names are camelCase on the wire, so this carries
/// `#[serde(rename_all = "camelCase")]` and uses `snake_case` Rust field names.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookSpecificOutput<'a> {
    hook_event_name: &'static str,
    permission_decision: &'static str,
    permission_decision_reason: &'a str,
}

/// Claude Code top-level shape: a single `hookSpecificOutput` field.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeOutput<'a> {
    hook_specific_output: HookSpecificOutput<'a>,
}

/// The host-specific stdout payload for a hook decision.
///
/// `#[serde(untagged)]` makes serializing `HookOutput::Cursor(..)` produce
/// the flat [`CursorOutput`] object and `HookOutput::Claude(..)` produce the
/// nested [`ClaudeOutput`] object — exactly the two wire shapes the agent
/// hosts parse. The lifetime borrows from the caller's `reason` (and the
/// `'static` tag / event name).
#[derive(Serialize)]
#[serde(untagged)]
pub enum HookOutput<'a> {
    /// Flat Cursor shape.
    Cursor(CursorOutput<'a>),
    /// Nested Claude Code shape.
    Claude(ClaudeOutput<'a>),
}

/// Build the host-specific stdout JSON for a decision.
///
/// For Cursor this is the flat `permission`/`user_message`/`agent_message`
/// shape; for Claude Code it is the nested `hookSpecificOutput` shape. The
/// `event` argument is used only for Claude Code's `hookEventName` field.
///
/// Returns a typed [`HookOutput`] (which implements [`Serialize`]); the hook
/// binary passes it to `serde_json::to_string`. Using typed structs (rather
/// than `serde_json::Value`/`json!` literals) keeps the wire field names
/// compile-checked.
#[must_use]
pub fn to_stdout(
    host: Host,
    event: CanonicalEvent,
    decision: Decision,
    reason: &str,
) -> HookOutput<'_> {
    let tag = decision.to_db_str();
    match host {
        Host::Cursor => HookOutput::Cursor(CursorOutput {
            permission: tag,
            user_message: reason,
            agent_message: "",
        }),
        Host::Claude => HookOutput::Claude(ClaudeOutput {
            hook_specific_output: HookSpecificOutput {
                hook_event_name: event.as_pascal_case(),
                permission_decision: tag,
                permission_decision_reason: reason,
            },
        }),
    }
}

/// Fail-closed stdout JSON: a `Deny` with an explanatory reason. Used when
/// the server is unreachable or an internal error occurs on a blocking
/// event. The hook subcommand pairs this with exit code 2.
#[must_use]
pub fn fail_closed_stdout(host: Host, event: CanonicalEvent) -> HookOutput<'static> {
    to_stdout(host, event, Decision::Deny, FAIL_CLOSED_REASON)
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

    /// Helper: serialize a [`HookOutput`] to a JSON [`Value`] for field-by-
    /// field assertion (mirrors what the integration tests do with stdout).
    fn to_value(out: HookOutput<'_>) -> serde_json::Value {
        serde_json::to_value(out).expect("HookOutput always serializes")
    }

    #[test]
    fn cursor_allow_shape() {
        let v = to_value(to_stdout(
            Host::Cursor,
            CanonicalEvent::PreToolUse,
            Decision::Allow,
            "ok",
        ));
        assert_eq!(v["permission"], "allow");
        assert_eq!(v["user_message"], "ok");
        assert_eq!(v["agent_message"], "");
        // No nested hookSpecificOutput for Cursor.
        assert!(v.get("hookSpecificOutput").is_none());
    }

    #[test]
    fn cursor_deny_shape() {
        let v = to_value(to_stdout(
            Host::Cursor,
            CanonicalEvent::PreToolUse,
            Decision::Deny,
            "no",
        ));
        assert_eq!(v["permission"], "deny");
        assert_eq!(v["user_message"], "no");
        assert_eq!(v["agent_message"], "");
    }

    #[test]
    fn claude_allow_shape() {
        let v = to_value(to_stdout(
            Host::Claude,
            CanonicalEvent::PreToolUse,
            Decision::Allow,
            "ok",
        ));
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow");
        assert_eq!(v["hookSpecificOutput"]["permissionDecisionReason"], "ok");
        // Cursor flat fields must NOT appear.
        assert!(v.get("permission").is_none());
        assert!(v.get("user_message").is_none());
    }

    #[test]
    fn claude_deny_shape() {
        let v = to_value(to_stdout(
            Host::Claude,
            CanonicalEvent::PreToolUse,
            Decision::Deny,
            "no",
        ));
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(v["hookSpecificOutput"]["permissionDecisionReason"], "no");
    }

    #[test]
    fn claude_uses_pascal_case_event_name() {
        let v = to_value(to_stdout(
            Host::Claude,
            CanonicalEvent::PostToolUse,
            Decision::Allow,
            "x",
        ));
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    }

    #[test]
    fn claude_defer_is_supported_on_wire() {
        // The output path reuses the protocol Decision enum, which includes
        // Defer. The hook subcommand itself never emits Defer, but the shape
        // must still be valid if it ever did.
        let v = to_value(to_stdout(
            Host::Claude,
            CanonicalEvent::PreToolUse,
            Decision::Defer,
            "later",
        ));
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "defer");
    }

    #[test]
    fn fail_closed_emits_deny_with_reason() {
        let v = to_value(fail_closed_stdout(Host::Cursor, CanonicalEvent::PreToolUse));
        assert_eq!(v["permission"], "deny");
        assert!(
            v["user_message"]
                .as_str()
                .unwrap()
                .contains("PocketVeto unreachable")
        );

        let v2 = to_value(fail_closed_stdout(Host::Claude, CanonicalEvent::PreToolUse));
        assert_eq!(v2["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(
            v2["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("PocketVeto unreachable")
        );
    }

    #[test]
    fn decision_tags_are_lowercase() {
        assert_eq!(Decision::Allow.to_db_str(), "allow");
        assert_eq!(Decision::Deny.to_db_str(), "deny");
        assert_eq!(Decision::Ask.to_db_str(), "ask");
        assert_eq!(Decision::Defer.to_db_str(), "defer");
    }

    #[test]
    fn to_stdout_serializes_to_compact_json_string() {
        // The hook binary does `serde_json::to_string(&to_stdout(..))`; make
        // sure that round-trips to valid JSON with the expected fields.
        let out = to_stdout(
            Host::Cursor,
            CanonicalEvent::PreToolUse,
            Decision::Allow,
            "ok",
        );
        let s = serde_json::to_string(&out).expect("ser");
        assert!(s.contains("\"permission\":\"allow\""));
        assert!(s.contains("\"user_message\":\"ok\""));
        assert!(s.contains("\"agent_message\":\"\""));

        let out = to_stdout(
            Host::Claude,
            CanonicalEvent::PreToolUse,
            Decision::Deny,
            "no",
        );
        let s = serde_json::to_string(&out).expect("ser");
        assert!(s.contains("\"hookSpecificOutput\""));
        assert!(s.contains("\"hookEventName\":\"PreToolUse\""));
        assert!(s.contains("\"permissionDecision\":\"deny\""));
        assert!(s.contains("\"permissionDecisionReason\":\"no\""));
    }
}
