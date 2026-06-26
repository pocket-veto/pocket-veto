package io.pocketveto.service

import android.app.Notification
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationCompat
import androidx.core.app.RemoteInput
import io.pocketveto.R
import io.pocketveto.data.Decision
import io.pocketveto.data.ServerMessage
import io.pocketveto.service.NotifFactory.approvalNotifId
import io.pocketveto.service.NotifFactory.buildApprovalNotification
import io.pocketveto.service.NotifFactory.buildPersistentNotification
import io.pocketveto.ui.DashboardActivity

/**
 * Builds the two notification styles PocketVeto posts:
 *
 * - [buildPersistentNotification]: the low-importance foreground-service
 *   notification shown while the Bluetooth link is up. Replaces its own
 *   content text with the connection state.
 * - [buildApprovalNotification]: a high-importance notification with
 *   `Allow` / `Deny` / `Reply…` (RemoteInput) actions. Uses
 *   `CATEGORY_CALL` so it bypasses Doze on Android 12+, and a vibrate
 *   pattern set on the channel in `PocketVetoApplication`.
 *
 * Action buttons fire [DecisionReceiver] via a `PendingIntent.getBroadcast`.
 * The receiver forwards the decision to [BluetoothService] by `startService`
 * with an [Intent] carrying the decision fields as extras (see
 * [BluetoothService] for the intent-action contract).
 */
object NotifFactory {

    const val CHANNEL_PERSISTENT = "pocketveto.persistent"
    const val CHANNEL_APPROVAL = "pocketveto.approval"

    /** Foreground-service notification id (constant; reused on update). */
    const val NOTIF_PERSISTENT_ID = 0x1001

    /** Per-approval notification id derived from the approval id's hashCode. */
    fun approvalNotifId(approvalId: String): Int = 0x2000 + approvalId.hashCode()

    /**
     * Persistent low-priority notification for the foreground service.
     * [connected] selects the content text; pass `null` for the
     * disconnected/reconnecting copy.
     */
    fun buildPersistentNotification(
        ctx: Context,
        connected: Boolean,
        targetName: String?,
    ): Notification {
        val text = when {
            connected && targetName != null ->
                ctx.getString(R.string.notif_persistent_connected, targetName)

            connected -> ctx.getString(R.string.notif_persistent_connected, "PC")
            targetName == null -> ctx.getString(R.string.notif_persistent_no_target)
            else -> ctx.getString(R.string.notif_persistent_disconnected)
        }
        val tapIntent = PendingIntent.getActivity(
            ctx,
            0,
            Intent(ctx, DashboardActivity::class.java)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP),
            pendingIntentFlags(),
        )
        return NotificationCompat.Builder(ctx, CHANNEL_PERSISTENT)
            .setSmallIcon(R.drawable.ic_veto)
            .setContentTitle(ctx.getString(R.string.notif_persistent_title))
            .setContentText(text)
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setContentIntent(tapIntent)
            .build()
    }

    /**
     * High-priority notification for an [ServerMessage.ApprovalRequest]. Three actions:
     * Allow, Deny, Reply… (RemoteInput). All three go to [DecisionReceiver].
     */
    fun buildApprovalNotification(
        ctx: Context,
        req: ServerMessage.ApprovalRequest,
    ): Notification {
        val tapIntent = PendingIntent.getActivity(
            ctx,
            req.approvalId.hashCode(),
            Intent(ctx, DashboardActivity::class.java)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP),
            pendingIntentFlags(),
        )

        val allowAction = NotificationCompat.Action.Builder(
            0,
            ctx.getString(R.string.action_allow),
            decisionPendingIntent(ctx, req, Decision.ALLOW, note = null),
        ).build()

        val denyAction = NotificationCompat.Action.Builder(
            0,
            ctx.getString(R.string.action_deny),
            decisionPendingIntent(ctx, req, Decision.DENY, note = null),
        ).build()

        val remoteInput = RemoteInput.Builder(DecisionReceiver.EXTRA_REPLY_TEXT)
            .setLabel(ctx.getString(R.string.notif_reply_label))
            .build()
        val replyAction = NotificationCompat.Action.Builder(
            R.drawable.ic_veto,
            ctx.getString(R.string.action_reply),
            decisionPendingIntent(ctx, req, Decision.ALLOW, note = null, isReply = true),
        )
            .addRemoteInput(remoteInput)
            .setAllowGeneratedReplies(true)
            .build()

        return NotificationCompat.Builder(ctx, CHANNEL_APPROVAL)
            .setSmallIcon(R.drawable.ic_veto)
            .setContentTitle(ctx.getString(R.string.notif_approval_title, req.tool))
            .setContentText(req.summary.take(80))
            .setStyle(NotificationCompat.BigTextStyle().bigText(req.detail))
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setCategory(NotificationCompat.CATEGORY_CALL) // bypass Doze on Android 12+
            .setVibrate(longArrayOf(0, 200, 100, 200))
            .setAutoCancel(true)
            .setContentIntent(tapIntent)
            .addAction(allowAction)
            .addAction(denyAction)
            .addAction(replyAction)
            .build()
    }

    /**
     * Build a [PendingIntent] that broadcasts to [DecisionReceiver] carrying
     * the [approvalNotifId], the chosen [decision], and (for the Reply action) a
     * flag marking the intent as the RemoteInput variant so the receiver
     * knows to extract the typed text.
     */
    private fun decisionPendingIntent(
        ctx: Context,
        req: ServerMessage.ApprovalRequest,
        decision: Decision,
        note: String?,
        isReply: Boolean = false,
    ): PendingIntent {
        val intent = Intent(ctx, DecisionReceiver::class.java).apply {
            action = DecisionReceiver.ACTION_DECISION
            putExtra(DecisionReceiver.EXTRA_APPROVAL_ID, req.approvalId)
            putExtra(DecisionReceiver.EXTRA_AGENT_ID, req.agentId)
            putExtra(DecisionReceiver.EXTRA_DECISION, decision.name)
            putExtra(DecisionReceiver.EXTRA_NOTIF_ID, approvalNotifId(req.approvalId))
            if (isReply) putExtra(DecisionReceiver.EXTRA_IS_REPLY, true)
            if (note != null) putExtra(DecisionReceiver.EXTRA_NOTE, note)
        }
        // The request code must be unique per (approvalId, decision, isReply)
        // so each PendingIntent is distinct and the system does not collapse
        // them. hashCode collisions are tolerable here: the worst case is a
        // stale action that the receiver then re-resolves via the extras.
        val requestCode = 31 * req.approvalId.hashCode() +
                decision.ordinal * 7 +
                if (isReply) 1 else 0
        return PendingIntent.getBroadcast(
            ctx,
            requestCode,
            intent,
            pendingIntentFlags(),
        )
    }

    private fun pendingIntentFlags(): Int =
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
}
