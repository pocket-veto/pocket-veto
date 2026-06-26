package io.pocketveto.data

import io.pocketveto.data.AgentRegistry.agents
import io.pocketveto.data.AgentRegistry.connectionState
import io.pocketveto.data.AgentRegistry.dispatch
import io.pocketveto.data.AgentRegistry.mutex
import io.pocketveto.data.ServerMessage
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import io.pocketveto.data.ServerMessage as SM

/**
 * Singleton registry holding the live state of every agent the phone has
 * heard about from the PC.
 *
 * [io.pocketveto.service.BluetoothService] calls [dispatch] for every [ServerMessage] frame it
 * reads; [dispatch] updates the underlying [MutableStateFlow] atomically so
 * the dashboard (which collects [agents]) recomposes only on real changes.
 *
 * The registry also tracks the Bluetooth [connectionState] so the dashboard
 * can render the connected/disconnected/connecting banner independently of
 * agent activity.
 */
object AgentRegistry {

    private val _agents = MutableStateFlow<Map<String, AgentState>>(emptyMap())
    val agents: StateFlow<Map<String, AgentState>> = _agents.asStateFlow()

    private val _connectionState =
        MutableStateFlow<ConnectionState>(ConnectionState.Disconnected(null))
    val connectionState: StateFlow<ConnectionState> = _connectionState.asStateFlow()

    private val mutex = Mutex()

    /** Mark the Bluetooth link as up. */
    fun onConnected() {
        _connectionState.value = ConnectionState.Connected
    }

    /** Mark the Bluetooth link as down, recording the failure cause if any. */
    fun onDisconnected(error: Throwable?) {
        _connectionState.value = ConnectionState.Disconnected(error)
    }

    /** Mark the Bluetooth link as attempting to (re)connect. */
    fun onConnecting() {
        _connectionState.value = ConnectionState.Connecting
    }

    /**
     * Apply one [ServerMessage] to the registry. Safe to call from any
     * coroutine; updates are serialized via [mutex].
     */
    suspend fun dispatch(msg: SM) {
        when (msg) {
            is SM.AgentStart -> mutate(msg.agentId) { existing ->
                existing.copy(
                    name = msg.name,
                    host = msg.host,
                    workspace = msg.workspace,
                    status = AgentStatus.RUNNING,
                    startedAt = msg.startedAt,
                )
            }

            is SM.AgentEvent -> mutate(msg.agentId) { existing ->
                existing.copy(
                    lastTool = msg.tool ?: existing.lastTool,
                    lastEventTs = msg.ts,
                    transcript = existing.transcript + EventEntry(
                        ts = msg.ts,
                        kind = msg.kind,
                        tool = msg.tool,
                        payload = msg.payload,
                    ),
                )
            }

            is SM.ApprovalRequest -> mutate(msg.agentId) { existing ->
                existing.copy(
                    status = AgentStatus.AWAITING_APPROVAL,
                    pendingApproval = msg,
                    lastEventTs = System.currentTimeMillis(),
                )
            }

            is SM.AgentEnd -> mutate(msg.agentId) { existing ->
                existing.copy(
                    status = msg.status,
                    pendingApproval = null,
                    lastEventTs = msg.endedAt,
                )
            }

            is SM.Heartbeat -> {
                // Heartbeats carry no agent-scoped state; the service replies
                // with a HeartbeatAck. Nothing to mutate in the registry.
            }
        }
    }

    /** Clear all agent state (e.g. on a full reconnect after a long gap). */
    suspend fun clear() {
        mutex.withLock { _agents.value = emptyMap() }
    }

    private suspend fun mutate(
        agentId: String,
        f: (AgentState) -> AgentState,
    ) {
        mutex.withLock {
            val current = _agents.value
            val existing = current[agentId] ?: AgentState(agentId = agentId)
            val next = f(existing)
            _agents.value = current + (agentId to next)
        }
    }
}

/** Per-agent view-model state surfaced to the dashboard. */
data class AgentState(
    val agentId: String,
    val name: String = "",
    val host: Host = Host.CURSOR,
    val workspace: String = "",
    val status: AgentStatus = AgentStatus.RUNNING,
    val startedAt: Long = 0L,
    val lastTool: String? = null,
    val lastEventTs: Long = 0L,
    val pendingApproval: ServerMessage.ApprovalRequest? = null,
    val transcript: List<EventEntry> = emptyList(),
)

/** One entry in an agent's transcript (a single `AgentEvent` frame). */
data class EventEntry(
    val ts: Long,
    val kind: EventKind,
    val tool: String?,
    val payload: kotlinx.serialization.json.JsonElement,
)

/** Coarse Bluetooth link state, surfaced as a banner in the dashboard. */
sealed class ConnectionState {
    /** Actively connected to the PC; the socket loop is running. */
    data object Connected : ConnectionState()

    /** Trying to (re)connect with backoff. */
    data object Connecting : ConnectionState()

    /** Not connected; [error] is the most recent failure cause, if any. */
    data class Disconnected(val error: Throwable?) : ConnectionState()
}
