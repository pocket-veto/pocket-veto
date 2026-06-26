//! Normalization of agent tool calls and payloads into reviewable summaries.
//!
//! Both Cursor and Claude Code send JSON on stdin to the `hook` subcommand,
//! but they use different field names and event-name casing. This module
//! converts either shape into a single [`InternalEvent`].
//!
//! # Detection rule
//!
//! The input must carry a `hook_event_name` string field. If the value's
//! first character is uppercase (`PreToolUse`, `PostToolUse`, ...) the host
//! is [`Host::Claude`]; if it is lowercase (`beforeShellExecution`,
//! `preToolUse`, ...) the host is [`Host::Cursor`]. The two hosts use
//! different casing conventions and the field is always present, so this is
//! reliable.
//!
//! # Unknown events
//!
//! The normalizer **errors on unknown event names** (the stricter option)
//! rather than falling back to `PostToolUse`. An unrecognized event is
//! almost certainly a new host event that has not been mapped yet, and
//! silently classifying it as `PostToolUse` could mask a blocking
//! `PreToolUse` variant that needs an approval. Erroring forces the
//! implementer to add the mapping.
//!
//! # Required vs optional fields
//!
//! `session_id` and `cwd` are required and error if missing. `tool_name`
//! and `tool_input` are optional and become `None` when absent.

use serde_json::Value;

use crate::error::{CoreError, NormalizeError, Result};
use crate::protocol::Host;

/// Canonical event names shared by both hosts after normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalEvent {
    PreToolUse,
    PostToolUse,
    Stop,
    SessionStart,
    SessionEnd,
    AgentThought,
    AgentResponse,
}

impl CanonicalEvent {
    /// The `PascalCase` string Claude Code expects in `hookEventName`.
    #[must_use]
    pub fn as_pascal_case(self) -> &'static str {
        match self {
            CanonicalEvent::PreToolUse => "PreToolUse",
            CanonicalEvent::PostToolUse => "PostToolUse",
            CanonicalEvent::Stop => "Stop",
            CanonicalEvent::SessionStart => "SessionStart",
            CanonicalEvent::SessionEnd => "SessionEnd",
            CanonicalEvent::AgentThought => "AgentThought",
            CanonicalEvent::AgentResponse => "AgentResponse",
        }
    }
}

/// The normalized internal representation of a hook payload.
#[derive(Debug, Clone)]
pub struct InternalEvent {
    /// Which host produced the event.
    pub host: Host,
    /// Canonical event kind (host-agnostic).
    pub event_name: CanonicalEvent,
    pub session_id: String,
    pub cwd: String,
    /// Tool name, if any. For Cursor `beforeShellExecution` this is
    /// synthesized as `"Shell"`; for `beforeMCPExecution` it is
    /// `MCP:<name>`.
    pub tool_name: Option<String>,
    /// The tool input / arguments, if any.
    pub tool_input: Option<Value>,
    /// The original input JSON, kept verbatim for the audit log.
    pub raw: Value,
}

impl InternalEvent {
    /// Normalize a hook payload into an [`InternalEvent`].
    ///
    /// This is the typed constructor for [`InternalEvent`]: the free
    /// [`normalize`] fn is a thin wrapper around it. See the module docs for
    /// the detection rule, unknown-event policy, and required-field policy.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Normalize`] when:
    /// - `hook_event_name` is missing, non-string, or empty,
    /// - the host cannot be determined from the event-name casing,
    /// - the event name is not recognized for the detected host,
    /// - a required field (`session_id`, `cwd`) is missing.
    pub fn from_hook_payload(input: &Value) -> Result<Self> {
        let raw = input.clone();

        // `let-else` keeps the "extract-or-fail" ladders flat instead of
        // nesting the rest of the function inside `ok_or_else`'s success
        // branch.
        let Some(event_name_str) = input.get("hook_event_name").and_then(|v| v.as_str()) else {
            return Err(CoreError::Normalize {
                kind: NormalizeError::MissingEventName,
                field: "hook_event_name",
            });
        };

        // Detection: PascalCase -> Claude, camelCase -> Cursor.
        let Some(first) = event_name_str.chars().next() else {
            return Err(CoreError::Normalize {
                kind: NormalizeError::EmptyEventName,
                field: "hook_event_name",
            });
        };
        let host = if first.is_ascii_uppercase() {
            Host::Claude
        } else if first.is_ascii_lowercase() {
            Host::Cursor
        } else {
            return Err(CoreError::Normalize {
                kind: NormalizeError::AmbiguousHost {
                    value: event_name_str.to_string(),
                },
                field: "hook_event_name",
            });
        };

        let (canonical, tool_override) = match host {
            Host::Claude => map_claude_event(event_name_str)?,
            Host::Cursor => map_cursor_event(event_name_str, input)?,
        };

        // Field extraction differs by host.
        let Some(session_id) = get_str_field(input, "session_id", "sessionId") else {
            return Err(CoreError::Normalize {
                kind: NormalizeError::MissingField,
                field: "session_id",
            });
        };
        let Some(cwd) = get_str_field(input, "cwd", "cwd") else {
            return Err(CoreError::Normalize {
                kind: NormalizeError::MissingField,
                field: "cwd",
            });
        };

        // A host-synthesized tool override wins over the payload's tool_name.
        let tool_name = tool_override.or_else(|| get_str_field(input, "tool_name", "toolName"));
        let tool_input = get_field(input, "tool_input", "toolInput").cloned();

        Ok(Self {
            host,
            event_name: canonical,
            session_id,
            cwd,
            tool_name,
            tool_input,
            raw,
        })
    }
}

/// Normalize a hook payload into an [`InternalEvent`].
///
/// Thin compatibility wrapper around [`InternalEvent::from_hook_payload`],
/// kept so existing callers (`hook::run`, integration tests) that import
/// `pocket_veto_core::normalize::normalize` do not need to change.
///
/// # Errors
///
/// Returns [`CoreError::Normalize`] when:
/// - `hook_event_name` is missing, non-string, or empty,
/// - the host cannot be determined from the event-name casing,
/// - the event name is not recognized for the detected host,
/// - a required field (`session_id`, `cwd`) is missing.
pub fn normalize(input: &Value) -> Result<InternalEvent> {
    InternalEvent::from_hook_payload(input)
}

/// Map a Claude (`PascalCase`) event name to its canonical form. Unknown
/// names error.
fn map_claude_event(name: &str) -> Result<(CanonicalEvent, Option<String>)> {
    let canonical = match name {
        "PreToolUse" => CanonicalEvent::PreToolUse,
        "PostToolUse" => CanonicalEvent::PostToolUse,
        "Stop" => CanonicalEvent::Stop,
        "SessionStart" => CanonicalEvent::SessionStart,
        "SessionEnd" => CanonicalEvent::SessionEnd,
        "AgentThought" => CanonicalEvent::AgentThought,
        "AgentResponse" => CanonicalEvent::AgentResponse,
        other => {
            return Err(CoreError::Normalize {
                kind: NormalizeError::UnknownEvent {
                    value: other.to_string(),
                },
                field: "hook_event_name",
            });
        }
    };
    Ok((canonical, None))
}

/// Map a Cursor (camelCase) event name to its canonical form. Some events
/// synthesize a `tool_name` override. Unknown names error.
///
/// `input` is taken so that `beforeMCPExecution` can synthesize
/// `MCP:<name>` from the payload's `toolName` / `toolInput` field.
fn map_cursor_event(name: &str, input: &Value) -> Result<(CanonicalEvent, Option<String>)> {
    match name {
        "beforeShellExecution" => Ok((CanonicalEvent::PreToolUse, Some("Shell".to_string()))),
        "beforeMCPExecution" => {
            // PreToolUse with toolName = "MCP:<name>". The
            // MCP server name is carried in the payload's `toolName` (or,
            // failing that, `toolInput.name`). If neither is present, the
            // name falls back to "MCP:unknown" so the approval still carries
            // a meaningful label.
            let mcp_name = input
                .get("toolName")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    let v = input.get("toolInput").and_then(|v| v.get("name"))?;
                    v.as_str()
                })
                .unwrap_or("unknown");
            Ok((CanonicalEvent::PreToolUse, Some(format!("MCP:{mcp_name}"))))
        }
        "preToolUse" => Ok((CanonicalEvent::PreToolUse, None)),
        "postToolUse" | "afterShellExecution" | "afterMCPExecution" => {
            Ok((CanonicalEvent::PostToolUse, None))
        }
        "stop" => Ok((CanonicalEvent::Stop, None)),
        "sessionStart" => Ok((CanonicalEvent::SessionStart, None)),
        "sessionEnd" => Ok((CanonicalEvent::SessionEnd, None)),
        "afterAgentThought" => Ok((CanonicalEvent::AgentThought, None)),
        "afterAgentResponse" => Ok((CanonicalEvent::AgentResponse, None)),
        other => Err(CoreError::Normalize {
            kind: NormalizeError::UnknownEvent {
                value: other.to_string(),
            },
            field: "hook_event_name",
        }),
    }
}

/// Look up a string field, trying the Claude key first then the Cursor key.
/// Returns the first non-empty hit.
fn get_str_field(input: &Value, claude_key: &str, cursor_key: &str) -> Option<String> {
    get_field(input, claude_key, cursor_key)
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

/// Look up a JSON value field, trying the Claude key first then the Cursor
/// key. Returns a reference to the first hit.
fn get_field<'a>(input: &'a Value, claude_key: &str, cursor_key: &str) -> Option<&'a Value> {
    input.get(claude_key).or_else(|| input.get(cursor_key))
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
    fn cursor_before_shell_execution_maps_to_pre_tool_use_shell() {
        let input = serde_json::json!({
            "hook_event_name": "beforeShellExecution",
            "sessionId": "s-cursor",
            "cwd": "/tmp/cursor",
            "toolInput": {"command": "rm -rf /"}
        });
        let ev = normalize(&input).expect("normalize cursor beforeShellExecution");
        assert_eq!(ev.host, Host::Cursor);
        assert_eq!(ev.event_name, CanonicalEvent::PreToolUse);
        assert_eq!(ev.tool_name.as_deref(), Some("Shell"));
        assert_eq!(ev.session_id, "s-cursor");
        assert_eq!(ev.cwd, "/tmp/cursor");
        assert!(ev.tool_input.is_some());
        assert_eq!(ev.tool_input.unwrap()["command"], "rm -rf /");
    }

    #[test]
    fn cursor_pre_tool_use_maps_to_pre_tool_use_no_override() {
        let input = serde_json::json!({
            "hook_event_name": "preToolUse",
            "sessionId": "s2",
            "cwd": "/tmp/x",
            "toolName": "Write",
            "toolInput": {"path": "/tmp/y"}
        });
        let ev = normalize(&input).expect("normalize cursor preToolUse");
        assert_eq!(ev.host, Host::Cursor);
        assert_eq!(ev.event_name, CanonicalEvent::PreToolUse);
        assert_eq!(ev.tool_name.as_deref(), Some("Write"));
    }

    #[test]
    fn claude_pre_tool_use_maps_passthrough() {
        let input = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s-claude",
            "cwd": "/tmp/claude",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"}
        });
        let ev = normalize(&input).expect("normalize claude PreToolUse");
        assert_eq!(ev.host, Host::Claude);
        assert_eq!(ev.event_name, CanonicalEvent::PreToolUse);
        assert_eq!(ev.tool_name.as_deref(), Some("Bash"));
        assert_eq!(ev.session_id, "s-claude");
        assert_eq!(ev.cwd, "/tmp/claude");
    }

    #[test]
    fn missing_hook_event_name_errors() {
        let input = serde_json::json!({"session_id": "s", "cwd": "/tmp"});
        let err = normalize(&input).expect_err("should error");
        assert!(err.to_string().contains("hook_event_name"));
    }

    #[test]
    fn missing_session_id_errors() {
        let input = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/tmp"
        });
        let err = normalize(&input).expect_err("should error on missing session_id");
        assert!(err.to_string().contains("session_id"));
    }

    #[test]
    fn missing_cwd_errors() {
        let input = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s"
        });
        let err = normalize(&input).expect_err("should error on missing cwd");
        assert!(err.to_string().contains("cwd"));
    }

    #[test]
    fn unknown_cursor_event_errors() {
        let input = serde_json::json!({
            "hook_event_name": "someNewCursorEvent",
            "sessionId": "s",
            "cwd": "/tmp"
        });
        let err = normalize(&input).expect_err("should error on unknown cursor event");
        assert!(err.to_string().contains("someNewCursorEvent"));
    }

    #[test]
    fn unknown_claude_event_errors() {
        let input = serde_json::json!({
            "hook_event_name": "SomeNewClaudeEvent",
            "session_id": "s",
            "cwd": "/tmp"
        });
        let err = normalize(&input).expect_err("should error on unknown claude event");
        assert!(err.to_string().contains("SomeNewClaudeEvent"));
    }

    #[test]
    fn cursor_session_lifecycle_events_map_correctly() {
        let start = serde_json::json!({
            "hook_event_name": "sessionStart",
            "sessionId": "s",
            "cwd": "/tmp"
        });
        assert_eq!(
            normalize(&start).expect("start").event_name,
            CanonicalEvent::SessionStart
        );

        let end = serde_json::json!({
            "hook_event_name": "sessionEnd",
            "sessionId": "s",
            "cwd": "/tmp"
        });
        assert_eq!(
            normalize(&end).expect("end").event_name,
            CanonicalEvent::SessionEnd
        );

        let stop = serde_json::json!({
            "hook_event_name": "stop",
            "sessionId": "s",
            "cwd": "/tmp"
        });
        assert_eq!(
            normalize(&stop).expect("stop").event_name,
            CanonicalEvent::Stop
        );
    }

    #[test]
    fn cursor_agent_thought_and_response_map() {
        let thought = serde_json::json!({
            "hook_event_name": "afterAgentThought",
            "sessionId": "s",
            "cwd": "/tmp"
        });
        assert_eq!(
            normalize(&thought).expect("thought").event_name,
            CanonicalEvent::AgentThought
        );

        let resp = serde_json::json!({
            "hook_event_name": "afterAgentResponse",
            "sessionId": "s",
            "cwd": "/tmp"
        });
        assert_eq!(
            normalize(&resp).expect("response").event_name,
            CanonicalEvent::AgentResponse
        );
    }

    #[test]
    fn cursor_before_mcp_execution_synthesizes_mcp_name() {
        let input = serde_json::json!({
            "hook_event_name": "beforeMCPExecution",
            "sessionId": "s",
            "cwd": "/tmp",
            "toolName": "github"
        });
        let ev = normalize(&input).expect("normalize beforeMCPExecution with toolName");
        assert_eq!(ev.host, Host::Cursor);
        assert_eq!(ev.event_name, CanonicalEvent::PreToolUse);
        assert_eq!(ev.tool_name.as_deref(), Some("MCP:github"));
    }

    #[test]
    fn cursor_before_mcp_execution_falls_back_to_unknown() {
        let input = serde_json::json!({
            "hook_event_name": "beforeMCPExecution",
            "sessionId": "s",
            "cwd": "/tmp"
        });
        let ev = normalize(&input).expect("normalize beforeMCPExecution without name");
        assert_eq!(ev.tool_name.as_deref(), Some("MCP:unknown"));
    }

    #[test]
    fn raw_is_preserved_verbatim() {
        let input = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s",
            "cwd": "/tmp",
            "extra": {"nested": [1, 2, 3]}
        });
        let ev = normalize(&input).expect("normalize");
        assert_eq!(ev.raw["extra"]["nested"][2], 3);
    }

    #[test]
    fn canonical_event_pascal_case_strings() {
        assert_eq!(CanonicalEvent::PreToolUse.as_pascal_case(), "PreToolUse");
        assert_eq!(CanonicalEvent::PostToolUse.as_pascal_case(), "PostToolUse");
        assert_eq!(CanonicalEvent::Stop.as_pascal_case(), "Stop");
        assert_eq!(
            CanonicalEvent::SessionStart.as_pascal_case(),
            "SessionStart"
        );
        assert_eq!(CanonicalEvent::SessionEnd.as_pascal_case(), "SessionEnd");
        assert_eq!(
            CanonicalEvent::AgentThought.as_pascal_case(),
            "AgentThought"
        );
        assert_eq!(
            CanonicalEvent::AgentResponse.as_pascal_case(),
            "AgentResponse"
        );
    }
}
