package io.pocketveto.data

import kotlinx.serialization.KSerializer
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * Protocol-mirror drift detector.
 *
 * `data/Protocol.kt` mirrors `crates/pocket-veto-core/src/protocol.rs` field
 * for field on the JSON wire. These golden-string assertions pin the exact
 * bytes kotlinx.serialization produces against the bytes Rust's serde
 * produces (`#[serde(tag = "type", rename_all = "snake_case")]` for the two
 * message enums, `snake_case`/`lowercase` for the supporting enums). If a
 * `@SerialName` here or a `#[serde(rename)]` on the Rust side drifts, the
 * matching assertion fails — the kind of regression that is painful to debug
 * over a Bluetooth socket and is otherwise not caught until runtime.
 *
 * The round-trip section additionally verifies `ProtocolJson`
 * (`classDiscriminator = "type"`, `encodeDefaults = false`,
 * `explicitNulls = false`) is symmetric: an optional field serialized as
 * absent must deserialize back to `null`, matching Rust's
 * `skip_serializing_if = "Option::is_none"`.
 */
class ProtocolSerializationTest {

    // ------------------------------------------------------------------
    // Enum wire tags
    // ------------------------------------------------------------------

    @Test
    fun hostWireTags() {
        assertEquals("\"cursor\"", ProtocolJson.encodeToString(Host.serializer(), Host.CURSOR))
        assertEquals("\"claude\"", ProtocolJson.encodeToString(Host.serializer(), Host.CLAUDE))
    }

    @Test
    fun eventKindWireTags() {
        val expected = mapOf(
            EventKind.TOOL_CALL to "tool_call",
            EventKind.THOUGHT to "thought",
            EventKind.RESPONSE to "response",
            EventKind.FILE_EDIT to "file_edit",
            EventKind.SHELL to "shell",
            EventKind.APPROVAL_REQUEST to "approval_request",
            EventKind.APPROVAL_DECISION to "approval_decision",
        )
        expected.forEach { (kind, tag) ->
            assertEquals("\"$tag\"", ProtocolJson.encodeToString(EventKind.serializer(), kind))
        }
    }

    @Test
    fun agentStatusWireTags() {
        val expected = mapOf(
            AgentStatus.RUNNING to "running",
            AgentStatus.AWAITING_APPROVAL to "awaiting_approval",
            AgentStatus.COMPLETED to "completed",
            AgentStatus.ABORTED to "aborted",
            AgentStatus.ERROR to "error",
        )
        expected.forEach { (status, tag) ->
            assertEquals("\"$tag\"", ProtocolJson.encodeToString(AgentStatus.serializer(), status))
        }
    }

    @Test
    fun decisionWireTags() {
        val expected = mapOf(
            Decision.ALLOW to "allow",
            Decision.DENY to "deny",
            Decision.ASK to "ask",
            Decision.DEFER to "defer",
        )
        expected.forEach { (decision, tag) ->
            assertEquals("\"$tag\"", ProtocolJson.encodeToString(Decision.serializer(), decision))
        }
    }

    // ------------------------------------------------------------------
    // ServerMessage (PC -> Phone) golden wires
    // ------------------------------------------------------------------

    @Test
    fun agentStartGolden() {
        val msg = ServerMessage.AgentStart(
            agentId = "a1",
            sessionId = "s1",
            host = Host.CLAUDE,
            name = "refactor",
            workspace = "/tmp/w",
            startedAt = 1_700_000_000_000L,
        )
        assertSer(
            """{"type":"agent_start","agent_id":"a1","session_id":"s1",""" +
                """"host":"claude","name":"refactor","workspace":"/tmp/w",""" +
                """"started_at":1700000000000}""",
            msg,
        )
    }

    @Test
    fun agentEventOmitsToolWhenNull() {
        val msg = ServerMessage.AgentEvent(
            agentId = "a1",
            kind = EventKind.TOOL_CALL,
            tool = null,
            payload = ProtocolJson.parseToJsonElement("""{"k":"v"}"""),
            ts = 123L,
        )
        assertSer(
            """{"type":"agent_event","agent_id":"a1","kind":"tool_call",""" +
                """"payload":{"k":"v"},"ts":123}""",
            msg,
        )
    }

    @Test
    fun agentEventIncludesToolWhenPresent() {
        val msg = ServerMessage.AgentEvent(
            agentId = "a1",
            kind = EventKind.TOOL_CALL,
            tool = "bash",
            payload = ProtocolJson.parseToJsonElement("""{"k":"v"}"""),
            ts = 123L,
        )
        assertSer(
            """{"type":"agent_event","agent_id":"a1","kind":"tool_call",""" +
                """"tool":"bash","payload":{"k":"v"},"ts":123}""",
            msg,
        )
    }

    @Test
    fun approvalRequestGolden() {
        val msg = ServerMessage.ApprovalRequest(
            approvalId = "ap1",
            agentId = "a1",
            tool = "bash",
            summary = "s",
            detail = "d",
            timeoutAt = 999L,
        )
        assertSer(
            """{"type":"approval_request","approval_id":"ap1","agent_id":"a1",""" +
                """"tool":"bash","summary":"s","detail":"d","timeout_at":999}""",
            msg,
        )
    }

    @Test
    fun agentEndGolden() {
        val msg = ServerMessage.AgentEnd(
            agentId = "a1",
            endedAt = 555L,
            status = AgentStatus.COMPLETED,
        )
        assertSer(
            """{"type":"agent_end","agent_id":"a1","ended_at":555,"status":"completed"}""",
            msg,
        )
    }

    @Test
    fun heartbeatGolden() {
        assertSer("""{"type":"heartbeat","ts":42}""", ServerMessage.Heartbeat(ts = 42L))
    }

    // ------------------------------------------------------------------
    // ClientMessage (Phone -> PC) golden wires
    // ------------------------------------------------------------------

    @Test
    fun subscribeOmitsFilterWhenNull() {
        assertSer("""{"type":"subscribe"}""", ClientMessage.Subscribe(filter = null))
    }

    @Test
    fun subscribeIncludesFilterWhenPresent() {
        val msg = ClientMessage.Subscribe(
            filter = SubscriptionFilter(
                agentIds = listOf("a1"),
                kinds = listOf(EventKind.TOOL_CALL),
            ),
        )
        assertSer(
            """{"type":"subscribe","filter":{"agent_ids":["a1"],"kinds":["tool_call"]}}""",
            msg,
        )
    }

    @Test
    fun approvalDecisionOmitsNoteWhenNull() {
        val msg = ClientMessage.ApprovalDecision(
            approvalId = "ap1",
            decision = Decision.ALLOW,
            note = null,
        )
        assertSer(
            """{"type":"approval_decision","approval_id":"ap1","decision":"allow"}""",
            msg,
        )
    }

    @Test
    fun approvalDecisionIncludesNoteWhenPresent() {
        val msg = ClientMessage.ApprovalDecision(
            approvalId = "ap1",
            decision = Decision.ALLOW,
            note = "ok",
        )
        assertSer(
            """{"type":"approval_decision","approval_id":"ap1",""" +
                """"decision":"allow","note":"ok"}""",
            msg,
        )
    }

    @Test
    fun heartbeatAckGolden() {
        assertSer("""{"type":"heartbeat_ack","ts":7}""", ClientMessage.HeartbeatAck(ts = 7L))
    }

    // ------------------------------------------------------------------
    // Round-trip symmetry (encode -> decode == original)
    // ------------------------------------------------------------------

    @Test
    fun serverMessagesRoundTrip() {
        listOf(
            ServerMessage.AgentStart(
                agentId = "a1",
                sessionId = "s1",
                host = Host.CLAUDE,
                name = "refactor",
                workspace = "/tmp/w",
                startedAt = 1_700_000_000_000L,
            ),
            ServerMessage.AgentEvent(
                agentId = "a1",
                kind = EventKind.TOOL_CALL,
                tool = null,
                payload = ProtocolJson.parseToJsonElement("""{"k":"v"}"""),
                ts = 123L,
            ),
            ServerMessage.AgentEvent(
                agentId = "a1",
                kind = EventKind.FILE_EDIT,
                tool = "edit",
                payload = ProtocolJson.parseToJsonElement("""{"n":3}"""),
                ts = 124L,
            ),
            ServerMessage.ApprovalRequest(
                approvalId = "ap1",
                agentId = "a1",
                tool = "bash",
                summary = "s",
                detail = "d",
                timeoutAt = 999L,
            ),
            ServerMessage.AgentEnd(agentId = "a1", endedAt = 555L, status = AgentStatus.COMPLETED),
            ServerMessage.Heartbeat(ts = 42L),
        ).forEach { assertRoundTrip(ServerMessage.serializer(), it) }
    }

    @Test
    fun clientMessagesRoundTrip() {
        listOf(
            ClientMessage.Subscribe(filter = null),
            ClientMessage.Subscribe(
                filter = SubscriptionFilter(
                    agentIds = listOf("a1"),
                    kinds = listOf(EventKind.TOOL_CALL),
                ),
            ),
            ClientMessage.ApprovalDecision(
                approvalId = "ap1",
                decision = Decision.ALLOW,
                note = null,
            ),
            ClientMessage.ApprovalDecision(
                approvalId = "ap1",
                decision = Decision.DENY,
                note = "nope",
            ),
            ClientMessage.HeartbeatAck(ts = 7L),
        ).forEach { assertRoundTrip(ClientMessage.serializer(), it) }
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    private fun assertSer(expected: String, msg: ServerMessage) {
        assertEquals(expected, ProtocolJson.encodeToString(ServerMessage.serializer(), msg))
    }

    private fun assertSer(expected: String, msg: ClientMessage) {
        assertEquals(expected, ProtocolJson.encodeToString(ClientMessage.serializer(), msg))
    }

    private fun <T> assertRoundTrip(serializer: KSerializer<T>, msg: T) {
        val json = ProtocolJson.encodeToString(serializer, msg)
        val back = ProtocolJson.decodeFromString(serializer, json)
        assertEquals(msg, back)
    }
}
