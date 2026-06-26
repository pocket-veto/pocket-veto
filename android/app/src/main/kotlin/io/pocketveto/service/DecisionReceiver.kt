package io.pocketveto.service

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.RemoteInput
import io.pocketveto.data.Decision
import io.pocketveto.service.DecisionReceiver.Companion.ACTION_DECISION
import io.pocketveto.service.DecisionReceiver.Companion.EXTRA_AGENT_ID
import io.pocketveto.service.DecisionReceiver.Companion.EXTRA_APPROVAL_ID
import io.pocketveto.service.DecisionReceiver.Companion.EXTRA_DECISION
import io.pocketveto.service.DecisionReceiver.Companion.EXTRA_IS_REPLY
import io.pocketveto.service.DecisionReceiver.Companion.EXTRA_NOTIF_ID
import io.pocketveto.service.DecisionReceiver.Companion.EXTRA_REPLY_TEXT

/**
 * BroadcastReceiver fired by the action buttons on an approval notification
 * (Allow / Deny / Reply…).
 *
 * A `BroadcastReceiver` is short-lived: by the time `onReceive` returns the
 * system may tear the process down. We therefore do NOT attempt to write the
 * decision frame directly from here. Instead we forward the decision to
 * [BluetoothService] via [Context.startService] with an [Intent] carrying
 * the decision fields as extras. The service's [BluetoothService.onStartCommand]
 * recognizes the [ACTION_DECISION] intent action and writes the
 * `ApprovalDecision` frame on the live socket from the socket-loop
 * coroutine, which is the only safe place to touch the socket.
 *
 * Extras contract (mirrors the keys written by [NotifFactory]):
 * - [EXTRA_APPROVAL_ID]: String (the approval id to decide on)
 * - [EXTRA_AGENT_ID]: String (the agent that owns the approval, for logging)
 * - [EXTRA_DECISION]: String, the [Decision] enum name (ALLOW/DENY/ASK/DEFER)
 * - [EXTRA_NOTIF_ID]: Int, the notification id to dismiss
 * - [EXTRA_IS_REPLY]: Boolean, true iff this is the RemoteInput Reply action
 *   (the actual text is in the RemoteInput bundle under [EXTRA_REPLY_TEXT])
 */
class DecisionReceiver : BroadcastReceiver() {

    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != ACTION_DECISION) return
        val approvalId = intent.getStringExtra(EXTRA_APPROVAL_ID) ?: return
        val decisionName = intent.getStringExtra(EXTRA_DECISION) ?: return
        val decision = runCatching { Decision.valueOf(decisionName) }.getOrNull() ?: return
        val notifId = intent.getIntExtra(EXTRA_NOTIF_ID, -1)

        // Extract RemoteInput text if present. For Reply… the user types a
        // note; we send it as `note` on the ApprovalDecision frame. If the
        // user dismissed the keyboard without typing, treat as null and let
        // the decision still go through.
        val note: String? = if (intent.getBooleanExtra(EXTRA_IS_REPLY, false)) {
            RemoteInput.getResultsFromIntent(intent)?.getString(EXTRA_REPLY_TEXT)
                ?.takeIf { it.isNotBlank() }
        } else {
            intent.getStringExtra(EXTRA_NOTE)
        }

        // Forward to the service. We use startService (not startForegroundService)
        // because the service is already running in the foreground; this just
        // delivers a new intent to onStartCommand. The service ignores the
        // "decision" intent for foreground-promotion purposes.
        val forward = Intent(context, BluetoothService::class.java).apply {
            action = ACTION_DECISION
            putExtra(EXTRA_APPROVAL_ID, approvalId)
            putExtra(EXTRA_DECISION, decision.name)
            if (note != null) putExtra(EXTRA_NOTE, note)
        }
        context.startService(forward)

        // Dismiss the notification once the decision has been forwarded.
        if (notifId != -1) {
            NotificationManagerCompat.from(context).cancel(notifId)
        }
    }

    companion object {
        const val ACTION_DECISION = "io.pocketveto.action.DECISION"
        const val EXTRA_APPROVAL_ID = "approval_id"
        const val EXTRA_AGENT_ID = "agent_id"
        const val EXTRA_DECISION = "decision"
        const val EXTRA_NOTIF_ID = "notif_id"
        const val EXTRA_NOTE = "note"
        const val EXTRA_IS_REPLY = "is_reply"
        const val EXTRA_REPLY_TEXT = "reply_text"
    }
}
