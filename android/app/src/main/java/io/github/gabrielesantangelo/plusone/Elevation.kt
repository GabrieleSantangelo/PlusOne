package io.github.gabrielesantangelo.plusone

import java.util.concurrent.CompletableFuture
import java.util.concurrent.TimeUnit

/**
 * The app brought to the front for the camera and the microphone (launch:
 * CaptureActivity), which a foreground service may take only from there:
 * asked for both at once, one launch and one wait. A launch that never
 * came to the front (its activity never resumed) is forgotten once its
 * wait is over: the next ask launches again, not waiting on it for ever.
 */
class Elevation(private val launch: () -> Unit) {
    private var pending: CompletableFuture<Boolean>? = null

    /** Whether the app came to the front within ms, launched unless a
     * launch is under way. */
    fun await(ms: Long): Boolean {
        val waiting = synchronized(this) {
            pending?.takeIf { !it.isDone } ?: CompletableFuture<Boolean>().also {
                pending = it
                launch()
            }
        }
        return try {
            waiting.get(ms, TimeUnit.MILLISECONDS)
        } catch (e: Exception) {  // not in time, or interrupted
            synchronized(this) { if (pending === waiting) pending = null }
            false
        }
    }

    /** The app is in front (CaptureActivity, MainActivity): whoever waits goes on. */
    @Synchronized
    fun elevated() {
        pending?.complete(true)
    }
}
