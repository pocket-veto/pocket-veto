package io.pocketveto.service

import android.app.Service
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothSocket
import android.content.Intent
import android.content.SharedPreferences
import android.os.IBinder
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.ServiceCompat
import io.pocketveto.data.AgentRegistry
import io.pocketveto.data.ClientMessage
import io.pocketveto.data.Decision
import io.pocketveto.data.FrameCodec
import io.pocketveto.data.ServerMessage
import io.pocketveto.service.BluetoothService.Companion.PREF_TARGET_MAC
import io.pocketveto.util.Reconnect
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.IOException
import java.util.UUID
import java.util.concurrent.atomic.AtomicReference
import kotlin.time.Duration.Companion.milliseconds

/**
 * Foreground service owning the RFCOMM socket to the PC.
 *
 * ## Lifecycle
 *
 * `onStartCommand` is called in three flavors:
 * 1. Once with `action == null` (or `ACTION_START`) — the initial start.
 *    Promotes the service to the foreground with the persistent notification,
 *    then launches [runSocketLoop] on a coroutine.
 * 2. With `action == DecisionReceiver.ACTION_DECISION` — a forwarded
 *    decision from the notification action buttons or the dashboard's inline
 *    buttons. We do NOT start a second socket loop; we enqueue a
 *    [ClientMessage.ApprovalDecision] on the [pendingDecisions] flow, which
 *    the live socket loop drains and writes to the current socket's output.
 * 3. With `action == ACTION_STOP` — an explicit shutdown requested by the
 *    dashboard's Stop button. Calls [shutdown] (which cancels the loop,
 *    closes the socket, removes the foreground notification, and stops the
 *    service) and returns `START_NOT_STICKY` so the system does not recreate
 *    a service the user deliberately killed.
 *
 * ## Shutdown
 *
 * The service can be torn down two ways:
 * - The [ACTION_STOP] intent (sent by the dashboard's Stop button) — handled
 *   in [onStartCommand].
 * - [onTaskRemoved] — fired when the user swipes the app out of the recents
 *   list. We treat the swipe as an explicit "stop the approval gate" signal
 *   and call [shutdown].
 *
 * Both paths converge on [shutdown], which delegates resource release to
 * [releaseResources] and then drops the foreground state with
 * `stopForeground(STOP_FOREGROUND_REMOVE)` plus `stopSelf`. [onDestroy]
 * calls only [releaseResources] since the system is already tearing the
 * service down and `stopForeground`/`stopSelf` would be redundant there.
 *
 * The app does NOT auto-start on device reboot: there is no
 * `BOOT_COMPLETED` receiver and no `RECEIVE_BOOT_COMPLETED` permission in
 * the manifest. `START_STICKY` (returned for the start/decision paths) only
 * causes the system to recreate the service after a low-memory kill — it
 * does not resurrect the service across a reboot or a force-stop.
 *
 * ## Socket loop
 *
 * [runSocketLoop] is a `suspend fun` that loops forever:
 * - look up the bonded Bluetooth device matching the target MAC (stored in
 *   [SharedPreferences] under [PREF_TARGET_MAC]),
 * - `createRfcommSocketToServiceRecord(UUID_SPP)` + `connect()`,
 * - call [AgentRegistry.onConnected], reset [Reconnect], and enter
 *   [socketLoop],
 * - on any exception call [AgentRegistry.onDisconnected], sleep
 *   [Reconnect.nextBackoff], and try again.
 *
 * ## Heartbeat
 *
 * On every [ServerMessage.Heartbeat] frame the loop replies with a
 *   [ClientMessage.HeartbeatAck] carrying the same `ts`. The Rust bridge
 *   treats a missing ack for 45s as a heartbeat timeout and forces a
 *   reconnect; the phone side does not enforce a symmetric timeout in v1 —
 *   the read loop throwing on a dead socket is the reconnect trigger.
 */
class BluetoothService : Service() {

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var socketLoopJob: Job? = null

    /** Serializes writes to the current socket's output stream. */
    private val writeMutex = Mutex()

    /**
     * The current socket, set inside [socketLoop] and cleared on exit.
     * Accessed by [writeDecision] from arbitrary coroutines (the dashboard
     * inline buttons, or `onStartCommand` handling a decision intent).
     */
    private val currentSocket = AtomicReference<BluetoothSocket?>(null)

    /**
     * Decisions waiting to be written once a socket is live. The socket loop
     * drains this in addition to reading frames. Decisions posted while
     * disconnected are buffered (the SharedFlow has a small replay/buffer —
     * see the constructor) and dropped only if the buffer overflows, which
     * for v1 is acceptable since the user can re-tap a button.
     */
    private val _pendingDecisions = MutableSharedFlow<ClientMessage.ApprovalDecision>(
        replay = 0,
        extraBufferCapacity = 16,
    )
    private val pendingDecisions = _pendingDecisions.asSharedFlow()

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> {
                // Explicit shutdown from the dashboard Stop button. Tear
                // everything down and tell the system NOT to recreate us —
                // the user killed this on purpose, so START_STICKY would be
                // wrong here.
                shutdown()
                return START_NOT_STICKY
            }

            DecisionReceiver.ACTION_DECISION -> {
                val approvalId = intent.getStringExtra(DecisionReceiver.EXTRA_APPROVAL_ID)
                val decisionName = intent.getStringExtra(DecisionReceiver.EXTRA_DECISION)
                val note = intent.getStringExtra(DecisionReceiver.EXTRA_NOTE)
                if (approvalId != null && decisionName != null) {
                    val decision = runCatching { Decision.valueOf(decisionName) }.getOrNull()
                    if (decision != null) {
                        enqueueDecision(approvalId, decision, note)
                    }
                }
                // We do NOT return START_STICKY-relevant behavior from a
                // decision-only intent; the loop job is already running.
            }

            else -> {
                // Initial start (action null or ACTION_START). Promote to
                // foreground and launch the socket loop.
                startForeground(
                    NotifFactory.NOTIF_PERSISTENT_ID,
                    NotifFactory.buildPersistentNotification(
                        this,
                        connected = false,
                        targetName = prefs().getString(PREF_TARGET_NAME, null),
                    ),
                )
                if (socketLoopJob == null || socketLoopJob?.isActive != true) {
                    socketLoopJob = scope.launch { runSocketLoop() }
                }
            }
        }
        return START_STICKY
    }

    /**
     * Called when the user removes the app from the recents list. We treat
     * the swipe as an explicit "stop the approval gate" signal: tear down the
     * socket and stop the service, mirroring the [ACTION_STOP] path.
     *
     * This is the documented Android hook for the swipe gesture on a started,
     * non-bound service. Some OEM task managers may skip it; the Stop button
     * is the deterministic shutdown path, and this is the best-effort hook
     * for the swipe.
     */
    override fun onTaskRemoved(rootIntent: Intent?) {
        shutdown()
        super.onTaskRemoved(rootIntent)
    }

    override fun onDestroy() {
        // The system is tearing us down; only release resources, do not call
        // stopForeground/stopSelf (redundant here). [shutdown] calls this
        // same release path plus the foreground/self-stop teardown.
        releaseResources()
        super.onDestroy()
    }

    /**
     * User-initiated shutdown: release the socket/coroutines, drop the
     * foreground notification, and stop the service. Used by both the
     * [ACTION_STOP] intent and [onTaskRemoved].
     */
    private fun shutdown() {
        releaseResources()
        ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    /**
     * Idempotent resource release shared by [shutdown] and [onDestroy]:
     * cancel the coroutine scope, close + clear the socket, and push the
     * disconnected state to the registry.
     */
    private fun releaseResources() {
        scope.cancel()
        currentSocket.get()?.runCatching { close() }
        currentSocket.set(null)
        AgentRegistry.onDisconnected(null)
    }

    /**
     * Public entry point for the dashboard's inline Allow/Deny/Reply buttons.
     * Enqueues a decision the same way the notification receiver does; the
     * socket loop will write it on the next drain.
     */
    fun writeDecision(approvalId: String, decision: Decision, note: String?) {
        enqueueDecision(approvalId, decision, note)
    }

    private fun enqueueDecision(approvalId: String, decision: Decision, note: String?) {
        val msg = ClientMessage.ApprovalDecision(
            approvalId = approvalId,
            decision = decision,
            note = note,
        )
        // tryEmit is non-suspending; safe to call from onStartCommand's
        // main-thread path. If the buffer is full (no socket for a long
        // time), the decision is dropped — the user can re-tap.
        _pendingDecisions.tryEmit(msg)
    }

    private suspend fun runSocketLoop() {
        while (true) {
            val mac = prefs().getString(PREF_TARGET_MAC, null)
            if (mac == null) {
                AgentRegistry.onDisconnected(null)
                delay(Reconnect.nextBackoff().milliseconds)
                continue
            }
            AgentRegistry.onConnecting()
            try {
                val device = bondedDeviceFor(mac)
                    ?: throw IOException("no bonded device with mac=$mac")
                val socket = device.createRfcommSocketToServiceRecord(UUID_SPP)
                socket.connect()
                currentSocket.set(socket)
                Reconnect.reset()
                AgentRegistry.onConnected()
                updatePersistentNotif(connected = true)
                socketLoop(socket)
            } catch (e: Throwable) {
                AgentRegistry.onDisconnected(e)
                updatePersistentNotif(connected = false)
                currentSocket.get()?.runCatching { close() }
                currentSocket.set(null)
                delay(Reconnect.nextBackoff().milliseconds)
            }
        }
    }

    /**
     * Read frames in a loop, dispatch to [AgentRegistry], reply to
     * heartbeats, and drain any pending decisions posted while we were
     * reading. The decision-drain is launched as a sibling coroutine so it
     * does not block frame reads.
     */
    private suspend fun socketLoop(socket: BluetoothSocket) {
        val input = DataInputStream(socket.inputStream)
        val output = DataOutputStream(socket.outputStream)
        val drainJob = scope.launch {
            pendingDecisions.collect { decision ->
                writeClientMessageSafe(
                    output, ClientMessage.ApprovalDecision(
                        approvalId = decision.approvalId,
                        decision = decision.decision,
                        note = decision.note,
                    )
                )
            }
        }
        try {
            while (true) {
                val msg = withContext(Dispatchers.IO) {
                    FrameCodec.readServerMessage(input)
                }
                AgentRegistry.dispatch(msg)
                when (msg) {
                    is ServerMessage.Heartbeat -> {
                        writeClientMessageSafe(
                            output,
                            ClientMessage.HeartbeatAck(ts = msg.ts),
                        )
                    }

                    is ServerMessage.ApprovalRequest -> {
                        postApprovalNotification(msg)
                    }

                    else -> Unit
                }
            }
        } finally {
            drainJob.cancel()
            currentSocket.get()?.runCatching { close() }
            currentSocket.set(null)
            // Any IOException from readServerMessage propagates out of this
            // function (try/finally without catch re-throws), back into
            // runSocketLoop's catch block, which handles backoff + registry.
        }
    }

    /** Write a [ClientMessage] frame under [writeMutex]; swallow socket errors. */
    private suspend fun writeClientMessageSafe(output: DataOutputStream, msg: ClientMessage) {
        writeMutex.withLock {
            try {
                withContext(Dispatchers.IO) {
                    FrameCodec.writeClientMessage(output, msg)
                }
            } catch (e: IOException) {
                // The socket is dying; the read loop will throw next and
                // trigger reconnect. Just log via the registry state.
                AgentRegistry.onDisconnected(e)
            }
        }
    }

    private fun postApprovalNotification(req: ServerMessage.ApprovalRequest) {
        val nm = NotificationManagerCompat.from(this)
        if (!nm.areNotificationsEnabled()) return
        try {
            nm.notify(
                NotifFactory.approvalNotifId(req.approvalId),
                NotifFactory.buildApprovalNotification(this, req)
            )
        } catch (_: SecurityException) {
            // POST_NOTIFICATIONS revoked between areNotificationsEnabled and notify.
        }
    }

    private fun updatePersistentNotif(connected: Boolean) {
        val nm = NotificationManagerCompat.from(this)
        if (!nm.areNotificationsEnabled()) return
        try {
            nm.notify(
                NotifFactory.NOTIF_PERSISTENT_ID,
                NotifFactory.buildPersistentNotification(
                    this,
                    connected = connected,
                    targetName = prefs().getString(PREF_TARGET_NAME, null),
                ),
            )
        } catch (_: SecurityException) {
            // POST_NOTIFICATIONS revoked between areNotificationsEnabled and notify.
        }
    }

    private fun bondedDeviceFor(mac: String): android.bluetooth.BluetoothDevice? =
        try {
            bluetoothAdapter()?.bondedDevices?.firstOrNull { it.address == mac }
        } catch (_: SecurityException) {
            // BLUETOOTH_CONNECT not granted; treat as no bonded device.
            null
        }

    @Suppress("ReturnCount") // framework getter; null adapter is unrecoverable
    private fun bluetoothAdapter(): BluetoothAdapter? {
        val mgr = getSystemService(BLUETOOTH_SERVICE) as? BluetoothManager
        return mgr?.adapter
    }

    private fun prefs(): SharedPreferences =
        getSharedPreferences(PREFS_NAME, MODE_PRIVATE)

    companion object {
        /** Standard Serial Port Profile UUID. Must match the Rust side. */
        val UUID_SPP: UUID = UUID.fromString("00001101-0000-1000-8000-00805F9B347F")

        const val PREFS_NAME = "pocketveto"
        const val PREF_TARGET_MAC = "target_mac"
        const val PREF_TARGET_NAME = "target_name"

        /** Intent action to (re)start the socket loop. */
        const val ACTION_START = "io.pocketveto.action.START"

        /** Intent action to shut down the service and tear down the socket. */
        const val ACTION_STOP = "io.pocketveto.action.STOP"
    }
}
