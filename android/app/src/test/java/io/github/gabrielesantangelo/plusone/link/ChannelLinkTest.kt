package io.github.gabrielesantangelo.plusone.link

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.IOException

/** What a feature's control channel (ChannelLink) sends, to a PC that takes it or stops taking it. */
class ChannelLinkTest {
    /** The tablet's end of a control channel, with room for room messages:
     * then none (the PC takes nothing more). With no timeout a send waits
     * as a blocking write does, until the channel ends. */
    private class Control(@Volatile private var room: Int) : MessageChannel {
        override val device = "fp"
        val sent = mutableListOf<String>()
        val timeouts = mutableListOf<Int>()
        @Volatile var ended = false

        override fun send(data: ByteArray, offset: Int, length: Int, timeoutMs: Int): Boolean {
            if (ended) throw IOException("ended")
            timeouts += timeoutMs
            if (room > 0) {
                room--
                sent += String(data, offset, length)
                return true
            }
            if (timeoutMs >= 0) return false
            while (!ended) Thread.sleep(10)
            throw IOException("ended")
        }

        override fun receive(buffer: ByteArray, timeoutMs: Int): Int = 0

        override fun shutdown() { ended = true }
    }

    private fun message(size: Int) = JSONObject().put("type", "signed").put("marks", "x".repeat(size))

    @Test
    fun eachMessageGoesWhileThePcTakesThem() {
        val control = Control(room = 10)
        val link = ChannelLink(control, "PC") { null }
        link.send(JSONObject().put("type", "pong"))
        link.send(message(ChannelLink.CHUNK))  // longer than a piece, with the JSON around it
        assertEquals(4, control.sent.size)
        assertEquals("pong", JSONObject(control.sent[0]).optString("type"))
        assertEquals("long", JSONObject(control.sent[1]).optString("type"))
        assertEquals(message(ChannelLink.CHUNK).toString(), control.sent[2] + control.sent[3])
        assertTrue(control.timeouts.all { it == ChannelLink.SEND_WAIT })
    }

    @Test(timeout = 10_000)
    fun aPcThatTakesNothingEndsTheChannelInsteadOfHoldingIt() {
        val control = Control(room = 0)
        val link = ChannelLink(control, "PC") { null }
        assertThrows(IOException::class.java) { link.send(JSONObject().put("type", "pong")) }
        assertEquals(listOf(ChannelLink.SEND_WAIT), control.timeouts)
        assertTrue(control.ended)
        // Whoever sends next is not held either.
        assertThrows(IOException::class.java) { link.send(JSONObject().put("type", "pong")) }
    }

    @Test(timeout = 10_000)
    fun aLongMessageCutShortEndsTheChannel() {
        val control = Control(room = 2)  // the header and a piece; not the rest
        val link = ChannelLink(control, "PC") { null }
        assertThrows(IOException::class.java) { link.send(message(3 * ChannelLink.CHUNK)) }
        assertEquals(2, control.sent.size)
        assertTrue(control.ended)  // nothing more on it, read as the rest of that message
    }
}
