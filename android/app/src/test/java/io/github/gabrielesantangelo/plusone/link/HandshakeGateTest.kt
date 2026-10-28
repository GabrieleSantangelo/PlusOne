package io.github.gabrielesantangelo.plusone.link

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class HandshakeGateTest {
    @Test
    fun oneHostCannotHoldUpThePairedPc() {
        var now = 0L
        val gate = HandshakeGate { now }
        val admitted = (1..8).count { gate.start("10.0.0.66") }
        assertEquals(HandshakeGate.AT_ONCE, admitted)
        assertTrue(gate.start("10.0.0.2"))  // another address still gets in
    }

    @Test
    fun newOnesComeAtARate() {
        var now = 0L
        val gate = HandshakeGate { now }
        var started = 0
        repeat(10) { if (gate.start("10.0.0.66")) { started++; gate.done("10.0.0.66") } }
        assertEquals(HandshakeGate.BURST.toInt(), started)
        assertFalse(gate.start("10.0.0.66"))
        now += 1_000_000_000L  // a second later: one more
        assertTrue(gate.start("10.0.0.66"))
    }
}
