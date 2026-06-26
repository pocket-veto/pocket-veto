@file:Suppress("ktlint:standard:property-naming") // wire field names are snake_case by contract

package io.pocketveto.data

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement

/**
 * Wire protocol between the PC server and the Android phone.
 *
 * Every type in this file mirrors `pocket_veto_core::protocol` in the Rust workspace
 * byte-for-byte on the JSON wire. The Rust side uses
 * `#[serde(tag = "type", rename_all = "snake_case")]` for the two message
 * enums and `#[serde(rename_all = "snake_case")]` (or `"lowercase"` for
 * [Host]) for the supporting enums; the kotlinx.serialization annotations
 * below reproduce the same discriminator and field names exactly.
 *
 * ## Frame format
 *
 * See [FrameCodec]: `[4 bytes big-endian u32 length N][N bytes UTF-8 JSON]`.
 * The big-endian length prefix must match Rust's `u32::to_be_bytes()`.
 *
 * ## Timestamps
 *
 * All timestamps are `Long` unix milliseconds, matching the Rust `i64`.
 */
@Serializable
enum class Host {
    @SerialName("cursor")
    CURSOR,
    @SerialName("claude")
    CLAUDE,
}

@Serializable
enum class EventKind {
    @SerialName("tool_call")
    TOOL_CALL,
    @SerialName("thought")
    THOUGHT,
    @SerialName("response")
    RESPONSE,
    @SerialName("file_edit")
    FILE_EDIT,
    @SerialName("shell")
    SHELL,
    @SerialName("approval_request")
    APPROVAL_REQUEST,
    @SerialName("approval_decision")
    APPROVAL_DECISION,
}

@Serializable
enum class AgentStatus {
    @SerialName("running")
    RUNNING,
    @SerialName("awaiting_approval")
    AWAITING_APPROVAL,
    @SerialName("completed")
    COMPLETED,
    @SerialName("aborted")
    ABORTED,
    @SerialName("error")
    ERROR,
}

@Serializable
enum class Decision {
    @SerialName("allow")
    ALLOW,
    @SerialName("deny")
    DENY,
    @SerialName("ask")
    ASK,
    @SerialName("defer")
    DEFER,
}

/**
 * Optional subscription filter sent by the phone in
 * [ClientMessage.Subscribe]. Both fields are nullable on the wire to match
 * Rust's `Option<Vec<...>>` with `skip_serializing_if = "Option::is_none"`.
 */
@Serializable
data class SubscriptionFilter(
    @SerialName("agent_ids") val agentIds: List<String>? = null,
    @SerialName("kinds") val kinds: List<EventKind>? = null,
)

/**
 * Messages flowing PC -> Phone. Polymorphic via the `type` discriminator
 * (configured in [ProtocolJson]); each variant carries a snake_case
 * `@SerialName` matching the Rust variant tag.
 */
@Serializable
sealed class ServerMessage {
    @Serializable
    @SerialName("agent_start")
    data class AgentStart(
        @SerialName("agent_id") val agentId: String,
        @SerialName("session_id") val sessionId: String,
        @SerialName("host") val host: Host,
        @SerialName("name") val name: String,
        @SerialName("workspace") val workspace: String,
        @SerialName("started_at") val startedAt: Long,
    ) : ServerMessage()

    @Serializable
    @SerialName("agent_event")
    data class AgentEvent(
        @SerialName("agent_id") val agentId: String,
        @SerialName("kind") val kind: EventKind,
        @SerialName("tool") val tool: String? = null,
        @SerialName("payload") val payload: JsonElement,
        @SerialName("ts") val ts: Long,
    ) : ServerMessage()

    @Serializable
    @SerialName("approval_request")
    data class ApprovalRequest(
        @SerialName("approval_id") val approvalId: String,
        @SerialName("agent_id") val agentId: String,
        @SerialName("tool") val tool: String,
        @SerialName("summary") val summary: String,
        @SerialName("detail") val detail: String,
        @SerialName("timeout_at") val timeoutAt: Long,
    ) : ServerMessage()

    @Serializable
    @SerialName("agent_end")
    data class AgentEnd(
        @SerialName("agent_id") val agentId: String,
        @SerialName("ended_at") val endedAt: Long,
        @SerialName("status") val status: AgentStatus,
    ) : ServerMessage()

    @Serializable
    @SerialName("heartbeat")
    data class Heartbeat(
        @SerialName("ts") val ts: Long,
    ) : ServerMessage()
}

/**
 * Messages flowing Phone -> PC. Same discriminator scheme as
 * [ServerMessage].
 */
@Serializable
sealed class ClientMessage {
    @Serializable
    @SerialName("subscribe")
    data class Subscribe(
        @SerialName("filter") val filter: SubscriptionFilter? = null,
    ) : ClientMessage()

    @Serializable
    @SerialName("approval_decision")
    data class ApprovalDecision(
        @SerialName("approval_id") val approvalId: String,
        @SerialName("decision") val decision: Decision,
        @SerialName("note") val note: String? = null,
    ) : ClientMessage()

    @Serializable
    @SerialName("heartbeat_ack")
    data class HeartbeatAck(
        @SerialName("ts") val ts: Long,
    ) : ClientMessage()
}

/**
 * Shared [Json] instance for the wire codec.
 *
 * - `classDiscriminator = "type"` reproduces Rust's `#[serde(tag = "type")]`.
 * - `ignoreUnknownKeys = true` lets the phone tolerate forward-compatible
 *   fields the server may add in a future protocol version (the Rust side's
 *   serde default is to ignore unknown fields for self-describing formats
 *   like JSON, so this preserves symmetry).
 * - `encodeDefaults = false` mirrors Rust's
 *   `#[serde(skip_serializing_if = "Option::is_none")]` for nullable fields:
 *   Kotlin defaults the optional fields to `null`, and we drop them on
 *   serialize so the wire bytes match what Rust produces.
 * - `explicitNulls = false` lets deserialization populate nullable fields
 *   with `null` when the key is absent, matching Rust's `Option::None`.
 */
val ProtocolJson: Json = Json {
    classDiscriminator = "type"
    ignoreUnknownKeys = true
    encodeDefaults = false
    explicitNulls = false
}
