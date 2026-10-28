package io.github.gabrielesantangelo.plusone.link

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class PairingPolicyTest {
    @Test
    fun aStrangerPairsOnlyInsideTheWindow() {
        assertFalse(PairingPolicy.allowed(now = 1_000, windowUntil = 0, known = false, wantsPair = true))
        assertTrue(PairingPolicy.allowed(now = 1_000, windowUntil = 121_000, known = false, wantsPair = true))
        assertFalse(PairingPolicy.allowed(now = 121_000, windowUntil = 121_000, known = false, wantsPair = true))
        assertFalse(PairingPolicy.allowed(now = 1_000, windowUntil = 121_000, known = false, wantsPair = false))
    }

    @Test
    fun aPairedPcNeedsNoWindow() {
        assertTrue(PairingPolicy.allowed(now = 1_000, windowUntil = 0, known = true, wantsPair = false))
    }
}
