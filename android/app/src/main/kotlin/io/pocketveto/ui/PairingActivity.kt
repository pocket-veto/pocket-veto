package io.pocketveto.ui

import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.content.Context
import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.core.content.edit
import io.pocketveto.R
import io.pocketveto.service.BluetoothService
import io.pocketveto.ui.theme.PocketVetoTheme
import io.pocketveto.util.rememberPermissionRequest

/**
 * First-launch pairing activity. Lists the system's bonded Bluetooth devices,
 * lets the user tap one (their PC), persists its MAC + name to
 * [android.content.SharedPreferences], then launches [DashboardActivity] and finishes.
 *
 * If the user has no bonded devices we surface a button that opens the
 * system Bluetooth settings so they can pair their PC first. PocketVeto
 * never initiates pairing itself — the trust boundary is the OS-level
 * Bluetooth bond, exactly as on the Rust side.
 */
class PairingActivity : ComponentActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Skip pairing if the user already chose a PC. The dashboard still
        // requires permissions, so we don't gate on those here.
        val prefs = getSharedPreferences(BluetoothService.PREFS_NAME, MODE_PRIVATE)
        if (prefs.contains(BluetoothService.PREF_TARGET_MAC)) {
            startActivity(Intent(this, DashboardActivity::class.java))
            finish()
            return
        }
        setContent {
            PocketVetoTheme {
                PairingScreen(
                    onPicked = { mac, name -> persistAndLaunch(mac, name) },
                    onOpenBtSettings = { openBluetoothSettings() },
                )
            }
        }
    }

    private fun persistAndLaunch(mac: String, name: String?) {
        getSharedPreferences(BluetoothService.PREFS_NAME, MODE_PRIVATE)
            .edit {
                putString(BluetoothService.PREF_TARGET_MAC, mac)
                    .putString(BluetoothService.PREF_TARGET_NAME, name)
            }
        startActivity(Intent(this, DashboardActivity::class.java))
        finish()
    }

    private fun openBluetoothSettings() {
        startActivity(Intent(android.provider.Settings.ACTION_BLUETOOTH_SETTINGS))
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun PairingScreen(
    onPicked: (String, String?) -> Unit,
    onOpenBtSettings: () -> Unit,
) {
    // Surface the permission prompt first; without it bondedDevices may be
    // empty or the call may throw SecurityException on Android 12+.
    val perms = rememberPermissionRequest()
    val ctx = LocalContext.current
    val devices = remember(perms.allGranted) {
        if (perms.allGranted) bondedDevices(ctx) else emptyList()
    }

    Scaffold(
        topBar = {
            TopAppBar(title = { Text(stringResource(R.string.pairing_title)) })
        },
    ) { padding ->
        PairingBody(
            devices = devices,
            permissionsGranted = perms.allGranted,
            contentPadding = padding,
            onPicked = onPicked,
            onOpenBtSettings = onOpenBtSettings,
        )
    }
}

@Composable
private fun PairingBody(
    devices: List<BondedDevice>,
    permissionsGranted: Boolean,
    contentPadding: PaddingValues,
    onPicked: (String, String?) -> Unit,
    onOpenBtSettings: () -> Unit,
) {
    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(contentPadding)
            .padding(horizontal = 16.dp, vertical = 12.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        if (!permissionsGranted) {
            Text(
                "Bluetooth and notification permissions are required to pair.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.error,
            )
        }
        if (devices.isEmpty()) {
            Box(
                modifier = Modifier.fillMaxSize(),
                contentAlignment = Alignment.Center,
            ) {
                Column(horizontalAlignment = Alignment.CenterHorizontally) {
                    Text(
                        stringResource(R.string.pairing_empty),
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Spacer(Modifier.padding(8.dp))
                    OutlinedButton(onClick = onOpenBtSettings) {
                        Text(stringResource(R.string.pairing_open_settings))
                    }
                }
            }
        } else {
            LazyColumn(
                modifier = Modifier.fillMaxSize(),
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                items(devices, key = { it.mac }) { d ->
                    DeviceRow(d, onClick = { onPicked(d.mac, d.name) })
                }
            }
        }
    }
}

@Composable
private fun DeviceRow(
    device: BondedDevice,
    onClick: () -> Unit,
) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(vertical = 12.dp, horizontal = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Column(modifier = Modifier.weight(1f)) {
            Text(
                text = device.name ?: device.mac,
                style = MaterialTheme.typography.titleMedium,
                color = MaterialTheme.colorScheme.onSurface,
            )
            Text(
                text = device.mac,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

private data class BondedDevice(val mac: String, val name: String?)

private fun bondedDevices(ctx: Context): List<BondedDevice> {
    val mgr = ctx.getSystemService(Context.BLUETOOTH_SERVICE) as? BluetoothManager
    val adapter: BluetoothAdapter? = mgr?.adapter
    // The `name` and `address` accessors require BLUETOOTH_CONNECT on
    // Android 12+. The caller is gated on the runtime permission above; the
    // try/catch also covers the TOCTOU window where the user revokes the
    // permission between the grant check and this call.
    return try {
        adapter?.bondedDevices
            .orEmpty()
            .map { BondedDevice(it.address, it.name) }
    } catch (_: SecurityException) {
        emptyList()
    }
}
