package io.github.gabrielesantangelo.plusone

import org.junit.Assert.assertEquals
import org.junit.Test

/** The speakers' thresholds (PcmPlayback): as much sound whatever a packet's length. */
class PcmPlaybackTest {
    private fun PcmPlayback.Counts.all() =
        listOf(prefill, maxHeld, burst, maxHole, trimEvery, slack, maxTarget, over, overFor)

    @Test
    fun inPacketsOf5MsTheyAreAsBefore() {
        val counts = PcmPlayback.Counts(48_000, 240)
        assertEquals(listOf(8, 200, 12, 40, 200, 4, 40, 20, 200), counts.all())
        assertEquals(0.01, counts.smoothing, 1e-12)
    }

    @Test
    fun inPacketsOf20MsTheyAreAsMuchSound() {
        // Opus's 20 ms (l27): 40 ms to start with is 2 packets, not 8 (160 ms).
        val counts = PcmPlayback.Counts(48_000, 960)
        assertEquals(listOf(2, 50, 3, 10, 50, 1, 10, 5, 50), counts.all())
        assertEquals(0.04, counts.smoothing, 1e-12)
    }

    @Test
    fun atAnotherRateToo() {
        assertEquals(listOf(4, 100, 6, 20, 100, 2, 20, 10, 100), PcmPlayback.Counts(44_100, 441).all())  // 10 ms
        assertEquals(listOf(80, 2000, 120, 400, 2000, 40, 400, 200, 2000), PcmPlayback.Counts(96_000, 48).all())
    }
}
