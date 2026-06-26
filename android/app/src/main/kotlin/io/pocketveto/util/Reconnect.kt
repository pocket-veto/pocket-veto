package io.pocketveto.util

import io.pocketveto.util.Reconnect.nextBackoff
import io.pocketveto.util.Reconnect.reset
import java.util.concurrent.atomic.AtomicInteger

/**
 * Reconnect-with-backoff helper.
 *
 * Schedule mirrors `pocket_veto_bt::bridge::BACKOFF_SCHEDULE` on the Rust side:
 * `[1s, 2s, 4s, 8s, 30s cap]`. [nextBackoff] returns the current slot and
 * advances the index up to the cap; [reset] returns the index to zero on a
 * successful connect. The index is held in an [AtomicInteger] so the object
 * is safe to call from the socket-loop coroutine without external locking.
 */
object Reconnect {

    private val backoff = longArrayOf(1_000L, 2_000L, 4_000L, 8_000L, 30_000L)
    private val idx = AtomicInteger(0)

    /**
     * The current backoff slot in milliseconds, then advance the index up
     * to the cap. After the cap is reached, every subsequent call returns
     * the last (cap) value.
     */
    fun nextBackoff(): Long {
        val current = idx.get().coerceAtMost(backoff.size - 1)
        idx.set((current + 1).coerceAtMost(backoff.size))
        return backoff[current]
    }

    /** Reset the index to zero. Call on a successful connect. */
    fun reset() {
        idx.set(0)
    }

    /** Current index (mainly for tests / logging). */
    fun currentIndex(): Int = idx.get()
}
