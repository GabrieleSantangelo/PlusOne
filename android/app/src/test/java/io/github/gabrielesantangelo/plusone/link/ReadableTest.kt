package io.github.gabrielesantangelo.plusone.link

import org.junit.Assert.assertEquals
import org.junit.Test

/** A PC's name as the notifications and the main screen show it. */
class ReadableTest {
    @Test
    fun controlAndBidiCharactersAreTakenOut() {
        assertEquals("Officegpj.exe", LinkServer.readable("Office‮gpj.exe"))
        assertEquals("Line oneLine two", LinkServer.readable("Line one\nLine two\u0000"))
        assertEquals("PC of Ann", LinkServer.readable("⁧PC of Ann⁩‏"))
    }

    @Test
    fun itIsAtMost64CharactersAndNeverEmpty() {
        assertEquals("x".repeat(64), LinkServer.readable("x".repeat(1000)))
        assertEquals("PC", LinkServer.readable(""))
        assertEquals("PC", LinkServer.readable("‮\n "))
    }
}
