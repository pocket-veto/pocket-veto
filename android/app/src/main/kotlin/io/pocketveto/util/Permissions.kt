package io.pocketveto.util

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.ui.platform.LocalContext
import androidx.core.content.ContextCompat

/**
 * Runtime permission helpers for PocketVeto.
 *
 * On Android 12+ (our minSdk), `BLUETOOTH_CONNECT` and `BLUETOOTH_SCAN` are
 * runtime permissions. `POST_NOTIFICATIONS` became a runtime permission on
 * Android 13+ (API 33). This file centralizes the grant checks and provides a
 * Compose-side launcher that requests all of them at once on first launch.
 */

/** The full set of runtime permissions PocketVeto needs at first launch. */
val REQUIRED_RUNTIME_PERMISSIONS: List<String> = buildList {
    add(Manifest.permission.BLUETOOTH_CONNECT)
    add(Manifest.permission.BLUETOOTH_SCAN)
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
        add(Manifest.permission.POST_NOTIFICATIONS)
    }
}

/** True iff every permission in [REQUIRED_RUNTIME_PERMISSIONS] is granted. */
fun hasAllRuntimePermissions(ctx: Context): Boolean =
    REQUIRED_RUNTIME_PERMISSIONS.all { isGranted(ctx, it) }

/** True iff both Bluetooth runtime permissions are granted. */
fun hasBluetoothPermissions(ctx: Context): Boolean =
    isGranted(ctx, Manifest.permission.BLUETOOTH_CONNECT) &&
            isGranted(ctx, Manifest.permission.BLUETOOTH_SCAN)

/**
 * True iff `POST_NOTIFICATIONS` is granted. On API < 33 the permission does
 * not exist and notifications are always allowed, so we return `true`.
 */
fun hasNotificationPermission(ctx: Context): Boolean {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return true
    return isGranted(ctx, Manifest.permission.POST_NOTIFICATIONS)
}

private fun isGranted(ctx: Context, permission: String): Boolean =
    ContextCompat.checkSelfPermission(ctx, permission) ==
            PackageManager.PERMISSION_GRANTED

/**
 * Compose helper that requests [REQUIRED_RUNTIME_PERMISSIONS] on first
 * composition and exposes the current grant state via [PermissionsResult].
 *
 * Call once from the launcher activity's `setContent`. The launcher is
 * triggered only if any permission is missing, so re-composition after a
 * rotation will not re-prompt the user.
 */
@Composable
fun rememberPermissionRequest(
    onResult: (Map<String, Boolean>) -> Unit = {},
): PermissionsResult {
    val ctx = LocalContext.current
    val granted = remember { mutableStateOf(hasAllRuntimePermissions(ctx)) }
    val launcher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions(),
    ) { result ->
        granted.value = result.values.all { it }
        onResult(result)
    }
    LaunchedEffect(Unit) {
        if (!granted.value) launcher.launch(REQUIRED_RUNTIME_PERMISSIONS.toTypedArray())
    }
    return PermissionsResult(
        allGranted = granted.value,
        request = { launcher.launch(REQUIRED_RUNTIME_PERMISSIONS.toTypedArray()) },
    )
}

/** Result of [rememberPermissionRequest]. */
data class PermissionsResult(
    val allGranted: Boolean,
    val request: () -> Unit,
)
