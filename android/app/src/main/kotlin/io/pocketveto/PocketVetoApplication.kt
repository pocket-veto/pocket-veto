package io.pocketveto

import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import io.pocketveto.service.NotifFactory

/**
 * Application subclass: creates the two notification channels once at
 * process start so the [io.pocketveto.service.BluetoothService] foreground notification and the
 * [NotifFactory] approval notification can post without further setup.
 *
 * Channels must exist *before* any notification is posted on Android 8+; we
 * create them unconditionally in [onCreate] — creating an existing channel
 * is a no-op, so this is safe across process restarts.
 */
class PocketVetoApplication : Application() {

    override fun onCreate() {
        super.onCreate()
        createNotificationChannels()
    }

    private fun createNotificationChannels() {
        val nm = getSystemService(NOTIFICATION_SERVICE) as NotificationManager

        // Low-importance: the foreground service runs silently once connected.
        val persistent = NotificationChannel(
            NotifFactory.CHANNEL_PERSISTENT,
            getString(R.string.channel_persistent_name),
            NotificationManager.IMPORTANCE_LOW,
        ).apply {
            description = getString(R.string.channel_persistent_description)
            setShowBadge(false)
        }

        // High-importance: approval requests must break through Doze.
        val approval = NotificationChannel(
            NotifFactory.CHANNEL_APPROVAL,
            getString(R.string.channel_approval_name),
            NotificationManager.IMPORTANCE_HIGH,
        ).apply {
            description = getString(R.string.channel_approval_description)
            enableVibration(true)
            // Matches the vibrate pattern in NotifFactory.buildApprovalNotification.
            vibrationPattern = longArrayOf(0, 200, 100, 200)
            // CATEGORY_CALL notifications bypass Doze on Android 12+; the
            // channel-level lockscreen visibility is set to public so the
            // action buttons are visible without unlocking.
            lockscreenVisibility = android.app.Notification.VISIBILITY_PUBLIC
        }

        nm.createNotificationChannels(listOf(persistent, approval))
    }
}
