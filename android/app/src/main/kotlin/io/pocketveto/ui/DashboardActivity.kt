package io.pocketveto.ui

import android.content.Context
import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.PowerSettingsNew
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import io.pocketveto.R
import io.pocketveto.data.AgentRegistry
import io.pocketveto.data.AgentState
import io.pocketveto.data.ConnectionState
import io.pocketveto.data.Decision
import io.pocketveto.service.BluetoothService
import io.pocketveto.ui.theme.PocketVetoTheme
import io.pocketveto.util.hasAllRuntimePermissions

/**
 * Main dashboard activity. Renders a Compose [Scaffold] with a top bar
 * showing the Bluetooth [ConnectionState] and a [LazyColumn] of agent cards.
 *
 * The activity owns no agent state itself: it collects [AgentRegistry.agents]
 * (a `StateFlow`) and recomposes on every frame the service dispatches.
 * Inline Allow/Deny/Reply taps call [BluetoothService.writeDecision] via the
 * running service instance — the service is started in foreground mode on
 * first launch (see [maybeStartService]) and remains running for the
 * lifetime of the process.
 */
class DashboardActivity : ComponentActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        maybeStartService()
        setContent {
            PocketVetoTheme {
                DashboardScreen(
                    onDecision = { approvalId, decision, note ->
                        forwardDecision(approvalId, decision, note)
                    },
                    onStopService = { stopServiceFromUi() },
                )
            }
        }
    }

    private fun maybeStartService() {
        if (!hasAllRuntimePermissions(this)) return
        val intent = Intent(this, BluetoothService::class.java).apply {
            action = BluetoothService.ACTION_START
        }
        startService(intent)
    }

    /**
     * Forwards an inline decision from the dashboard. We use the same path
     * as the notification receiver: an Intent with ACTION_DECISION extras
     * delivered to [BluetoothService.onStartCommand]. This guarantees the
     * dashboard buttons and the notification buttons produce identical wire
     * bytes.
     */
    private fun forwardDecision(approvalId: String, decision: Decision, note: String?) {
        val intent = Intent(this, BluetoothService::class.java).apply {
            action = io.pocketveto.service.DecisionReceiver.ACTION_DECISION
            putExtra(io.pocketveto.service.DecisionReceiver.EXTRA_APPROVAL_ID, approvalId)
            putExtra(io.pocketveto.service.DecisionReceiver.EXTRA_DECISION, decision.name)
            if (note != null) putExtra(io.pocketveto.service.DecisionReceiver.EXTRA_NOTE, note)
        }
        startService(intent)
    }

    /**
     * User-initiated shutdown from the dashboard Stop button. Sends
     * [BluetoothService.ACTION_STOP] to the service (which tears down the
     * socket, drops the foreground notification, and stops itself) and then
     * finishes the dashboard so the user is returned to the home screen,
     * mirroring the "the app is now off" mental model. Reopening the app
     * restarts the service via [maybeStartService].
     */
    private fun stopServiceFromUi() {
        val intent = Intent(this, BluetoothService::class.java).apply {
            action = BluetoothService.ACTION_STOP
        }
        startService(intent)
        finish()
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun DashboardScreen(
    onDecision: (String, Decision, String?) -> Unit,
    onStopService: () -> Unit,
) {
    val agents by AgentRegistry.agents.collectAsState()
    val connection by AgentRegistry.connectionState.collectAsState()
    val ctx = LocalContext.current

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(stringResource(R.string.dashboard_title)) },
                actions = {
                    IconButton(onClick = { restartService(ctx) }) {
                        Icon(Icons.Default.Refresh, contentDescription = "Reconnect")
                    }
                    IconButton(onClick = onStopService) {
                        Icon(
                            Icons.Default.PowerSettingsNew,
                            contentDescription = stringResource(R.string.action_stop_service),
                        )
                    }
                },
                colors = TopAppBarDefaults.topAppBarColors(
                    containerColor = MaterialTheme.colorScheme.surface,
                ),
            )
        },
    ) { padding ->
        DashboardBody(
            agents = agents.values.toList(),
            connection = connection,
            contentPadding = padding,
            onDecision = onDecision,
        )
    }
}

@Composable
private fun DashboardBody(
    agents: List<AgentState>,
    connection: ConnectionState,
    contentPadding: PaddingValues,
    onDecision: (String, Decision, String?) -> Unit,
) {
    Column(modifier = Modifier
        .fillMaxSize()
        .padding(contentPadding)) {
        ConnectionBanner(connection)
        if (agents.isEmpty()) {
            Box(
                modifier = Modifier.fillMaxSize(),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    stringResource(R.string.dashboard_empty),
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        } else {
            LazyColumn(
                modifier = Modifier.fillMaxSize(),
                contentPadding = PaddingValues(horizontal = 12.dp, vertical = 8.dp),
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                items(agents, key = { it.agentId }) { agent ->
                    AgentCard(state = agent, onDecision = onDecision)
                }
            }
        }
    }
}

@Composable
private fun ConnectionBanner(connection: ConnectionState) {
    val (text, color) = when (connection) {
        is ConnectionState.Connected ->
            stringResource(R.string.dashboard_status_connected) to MaterialTheme.colorScheme.secondary

        is ConnectionState.Connecting ->
            stringResource(R.string.dashboard_status_connecting) to MaterialTheme.colorScheme.tertiary

        is ConnectionState.Disconnected ->
            stringResource(R.string.dashboard_status_disconnected) to MaterialTheme.colorScheme.error
    }
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 16.dp, vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Text(text, style = MaterialTheme.typography.labelMedium, color = color)
    }
}

private fun restartService(ctx: Context) {
    val intent = Intent(ctx, BluetoothService::class.java).apply {
        action = BluetoothService.ACTION_START
    }
    ctx.startService(intent)
}
