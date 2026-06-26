package io.pocketveto.ui

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.filled.Error
import androidx.compose.material.icons.filled.PauseCircle
import androidx.compose.material.icons.filled.PlayCircle
import androidx.compose.material.icons.filled.StopCircle
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.pocketveto.R
import io.pocketveto.data.AgentState
import io.pocketveto.data.AgentStatus
import io.pocketveto.data.Decision
import io.pocketveto.data.EventKind
import io.pocketveto.ui.theme.PvAmber
import io.pocketveto.ui.theme.PvBlue
import io.pocketveto.ui.theme.PvGreen
import io.pocketveto.ui.theme.PvRed
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull

/**
 * One card per agent in the dashboard. Layout (ASCII diagram below):
 *
 * ```
 * ┌────────────────────────────────────┐
 * │ ● backend-refactor                 │
 * │   running · 4m 12s                 │
 * │   last: Edit src/api.ts (3 lines)  │
 * │   [expand transcript]              │
 * ├────────────────────────────────────┤
 * │ ⏸ awaiting approval · 00:42        │
 * │   rm -rf node_modules              │
 * │   [Allow]  [Deny]  [Reply…]        │
 * └────────────────────────────────────┘
 *
 * The [onDecision] callback is invoked with `(approvalId, decision, note)`
 * — the same path the notification actions use, so behavior is identical
 * whether the user approves from the shade or the dashboard.
 */
@Composable
fun AgentCard(
    state: AgentState,
    onDecision: (String, Decision, String?) -> Unit,
) {
    var expanded by remember { mutableStateOf(false) }
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
    ) {
        Column(modifier = Modifier.padding(14.dp)) {
            HeaderRow(state)
            Spacer(Modifier.size(4.dp))
            StatusLine(state)
            LastToolLine(state)
            if (state.transcript.isNotEmpty()) {
                TextButton(
                    onClick = { expanded = !expanded },
                    modifier = Modifier.padding(start = 0.dp, top = 2.dp),
                ) {
                    Text(
                        stringResource(
                            if (expanded) R.string.agent_hide_transcript else R.string.agent_show_transcript,
                        ),
                    )
                }
                AnimatedVisibility(visible = expanded) {
                    TranscriptList(state)
                }
            }
            state.pendingApproval?.let { req ->
                Spacer(Modifier.size(8.dp))
                ApprovalCard(req = req, onDecision = { decision, note ->
                    onDecision(req.approvalId, decision, note)
                })
            }
        }
    }
}

@Composable
private fun HeaderRow(state: AgentState) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        StatusDot(state.status)
        Spacer(Modifier.size(8.dp))
        Text(
            text = state.name.ifBlank { state.agentId.take(8) },
            style = MaterialTheme.typography.titleMedium,
            color = MaterialTheme.colorScheme.onSurface,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.weight(1f),
        )
        Text(
            text = state.host.name.lowercase(),
            style = MaterialTheme.typography.labelMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun StatusDot(status: AgentStatus) {
    val (icon, color) = when (status) {
        AgentStatus.RUNNING -> Icons.Filled.PlayCircle to PvGreen
        AgentStatus.AWAITING_APPROVAL -> Icons.Filled.PauseCircle to PvAmber
        AgentStatus.COMPLETED -> Icons.Filled.CheckCircle to PvBlue
        AgentStatus.ABORTED -> Icons.Filled.StopCircle to PvRed
        AgentStatus.ERROR -> Icons.Filled.Error to PvRed
    }
    Icon(
        imageVector = icon,
        contentDescription = status.name,
        tint = color,
        modifier = Modifier
            .size(16.dp)
            .clip(CircleShape),
    )
}

@Composable
private fun StatusLine(state: AgentState) {
    val elapsed = formatElapsed(state.startedAt)
    val text = if (state.status == AgentStatus.AWAITING_APPROVAL && state.pendingApproval != null) {
        stringResource(
            R.string.agent_awaiting_approval,
            formatRemaining(state.pendingApproval.timeoutAt)
        )
    } else {
        stringResource(R.string.agent_running, state.status.name.replace('_', ' '), elapsed)
    }
    Text(
        text = text,
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
}

@Composable
private fun LastToolLine(state: AgentState) {
    val tool = state.lastTool ?: return
    Text(
        text = stringResource(R.string.agent_last_tool, tool),
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurface,
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
    )
}

@Composable
private fun TranscriptList(state: AgentState) {
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(max = 220.dp)
            .verticalScroll(rememberScrollState())
            .padding(top = 4.dp),
    ) {
        state.transcript.forEach { entry ->
            TranscriptRow(entry)
        }
    }
}

@Composable
private fun TranscriptRow(entry: io.pocketveto.data.EventEntry) {
    Row(modifier = Modifier
        .fillMaxWidth()
        .padding(vertical = 2.dp)) {
        Text(
            text = "[${entry.kind.name.lowercase()}]",
            style = MaterialTheme.typography.labelMedium,
            color = kindColor(entry.kind),
            modifier = Modifier.padding(end = 6.dp),
        )
        Text(
            text = summarize(entry),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurface,
            fontFamily = FontFamily.Monospace,
            maxLines = 2,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

@Composable
private fun kindColor(kind: EventKind): Color = when (kind) {
    EventKind.TOOL_CALL -> PvBlue
    EventKind.THOUGHT -> MaterialTheme.colorScheme.onSurfaceVariant
    EventKind.RESPONSE -> PvGreen
    EventKind.FILE_EDIT -> PvAmber
    EventKind.SHELL -> PvAmber
    EventKind.APPROVAL_REQUEST -> PvRed
    EventKind.APPROVAL_DECISION -> PvGreen
}

private fun summarize(entry: io.pocketveto.data.EventEntry): String {
    val tool = entry.tool ?: entry.kind.name.lowercase()
    val detail = flattenPayload(entry.payload).take(120)
    return if (detail.isBlank()) tool else "$tool: $detail"
}

/** Best-effort one-line flattening of an arbitrary JsonElement. */
private fun flattenPayload(el: JsonElement): String = when (el) {
    is JsonPrimitive -> el.contentOrNull ?: ""
    is JsonObject -> el.entries.joinToString(", ") { "${it.key}=${flattenPayload(it.value)}" }
    is kotlinx.serialization.json.JsonArray -> el.joinToString(", ") { flattenPayload(it) }
}

private fun formatElapsed(startedAt: Long): String {
    if (startedAt <= 0L) return "—"
    val deltaMs = (System.currentTimeMillis() - startedAt).coerceAtLeast(0L)
    val totalSec = deltaMs / 1000
    val m = totalSec / 60
    val s = totalSec % 60
    return "${m}m ${s}s"
}

private fun formatRemaining(timeoutAt: Long): String {
    if (timeoutAt <= 0L) return "—"
    val deltaSec = (timeoutAt - System.currentTimeMillis()) / 1000
    if (deltaSec <= 0L) return "00:00"
    val m = deltaSec / 60
    val s = deltaSec % 60
    return "%02d:%02d".format(m, s)
}
