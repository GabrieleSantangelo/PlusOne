package io.github.gabrielesantangelo.plusone.link

/**
 * Who may start a handshake on the listener, as the core's gate.rs: so many
 * at once from one address, and new ones at a rate from it (a few at once,
 * then one a second). Under MAX_HANDSHAKING in all, one host on the network
 * held every slot and kept the paired PCs out (CC-16).
 */
class HandshakeGate(private val nanos: () -> Long = System::nanoTime) {
    private class Source(var running: Int, var tokens: Double, var refilled: Long)

    private val sources = HashMap<String, Source>()

    /** One more from there, if it may start now; done(from) when it is over. */
    @Synchronized
    fun start(from: String): Boolean {
        val now = nanos()
        if (sources.size >= SOURCES) {
            sources.entries.removeAll { it.value.running == 0 && now - it.value.refilled > QUIET_NS }
            if (sources.size >= SOURCES) return false
        }
        val source = sources.getOrPut(from) { Source(0, BURST, now) }
        source.tokens = minOf(BURST, source.tokens + RATE * (now - source.refilled) / 1e9)
        source.refilled = now
        if (source.running >= AT_ONCE || source.tokens < 1.0) return false
        source.running++
        source.tokens -= 1.0
        return true
    }

    @Synchronized
    fun done(from: String) {
        sources[from]?.let { it.running = maxOf(0, it.running - 1) }
    }

    companion object {
        const val AT_ONCE = 2
        const val BURST = 4.0
        const val RATE = 1.0  // a second
        const val SOURCES = 256
        const val QUIET_NS = 10_000_000_000L
    }
}
