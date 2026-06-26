package io.pocketveto.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AssistChip
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import io.pocketveto.R
import io.pocketveto.data.Decision
import io.pocketveto.data.ServerMessage

/**
 * Expanded approval detail, rendered inline at the bottom of an [AgentCard]
 * when the agent has a `pendingApproval`. The three action buttons call the
 * same [onDecision] path as the notification actions — `Allow` and `Deny`
 * fire immediately; `Reply…` reveals a one-line text field whose contents
 * are sent with an `Allow` decision (the user types a justification note,
 * then commits). The note may be empty.
 */
@Composable
fun ApprovalCard(
    req: ServerMessage.ApprovalRequest,
    onDecision: (Decision, String?) -> Unit,
) {
    var replying by remember { mutableStateOf(false) }
    var note by remember { mutableStateOf("") }

    Column(modifier = Modifier
        .fillMaxWidth()
        .padding(top = 4.dp)) {
        Text(
            text = req.tool,
            style = MaterialTheme.typography.titleMedium,
            color = MaterialTheme.colorScheme.onSurface,
            fontFamily = FontFamily.Monospace,
        )
        Spacer(Modifier.size(2.dp))
        Text(
            text = req.summary,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurface,
            maxLines = 3,
        )
        if (req.detail.isNotBlank()) {
            Spacer(Modifier.size(4.dp))
            Text(
                text = req.detail,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                fontFamily = FontFamily.Monospace,
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(max = 200.dp)
                    .verticalScroll(rememberScrollState()),
            )
        }
        Spacer(Modifier.size(10.dp))
        if (replying) {
            OutlinedTextField(
                value = note,
                onValueChange = { note = it },
                label = { Text(stringResource(R.string.notif_reply_label)) },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            Spacer(Modifier.size(8.dp))
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                Button(
                    onClick = {
                        onDecision(Decision.ALLOW, note.takeIf { it.isNotBlank() })
                        replying = false
                        note = ""
                    },
                    modifier = Modifier.weight(1f),
                ) { Text(stringResource(R.string.action_allow)) }
                OutlinedButton(
                    onClick = { replying = false; note = "" },
                    modifier = Modifier.weight(1f),
                ) { Text("Cancel") }
            }
        } else {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Button(
                    onClick = { onDecision(Decision.ALLOW, null) },
                    modifier = Modifier.weight(1f),
                ) { Text(stringResource(R.string.action_allow)) }
                OutlinedButton(
                    onClick = { onDecision(Decision.DENY, null) },
                    modifier = Modifier.weight(1f),
                ) { Text(stringResource(R.string.action_deny)) }
                AssistChip(
                    onClick = { replying = true },
                    label = { Text(stringResource(R.string.action_reply)) },
                )
            }
        }
    }
}
