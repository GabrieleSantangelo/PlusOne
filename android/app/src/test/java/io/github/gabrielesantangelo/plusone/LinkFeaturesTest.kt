package io.github.gabrielesantangelo.plusone

import io.github.gabrielesantangelo.plusone.link.Handler
import io.github.gabrielesantangelo.plusone.link.Link
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.ByteArrayOutputStream
import java.io.Closeable
import java.io.IOException
import java.io.OutputStream
import java.net.InetAddress

/** The features over the link's core (LinkFeatures): a control channel's
 * requests, and the channels for its packets the PC opens with it. */
class LinkFeaturesTest {
    /** A control channel: the PC's messages as they come on it, a payload's
     * pieces among them (here, what one reads as if it were a message). */
    private class Control(vararg messages: JSONObject) : Link("PC", "fp") {
        private val messages = ArrayDeque(messages.toList())
        val sent = mutableListOf<String>()
        override val address: InetAddress get() = throw IOException("no address")
        override fun send(message: JSONObject) { sent += message.optString("type") }
        override fun receive(maxHeader: Int): JSONObject = messages.removeFirstOrNull() ?: throw IOException("the PC closed it")
        override fun readPayload(size: Long, out: OutputStream) { receive() }
        override fun hold(silentMs: Int) {}
        override fun close() {}
    }

    /** Signing's: a payload with "sign" (a PDF), none with "got". */
    private val asked = mutableListOf<String>()
    private val handler = object : Handler {
        override val types = setOf("sign", "got")
        override fun maxPayload(type: String) = if (type == "sign") 100L else 0L
        override fun handle(link: Link, request: JSONObject, size: Long): Boolean {
            if (size > 0) link.readPayload(size, ByteArrayOutputStream())
            asked += request.optString("type")
            return true
        }
        override fun closed(link: Link) { asked += "closed" }
    }

    private fun request(type: String, size: Int = 0) = JSONObject().put("type", type).apply { if (size > 0) put("size", size) }

    @Test
    fun aRequestNotKnownHereIsPassedOver() {
        val control = Control(request("ping"), request("a-later-pcs-request"), request("sign", 10), request("payload"),
            request("got"))
        assertThrows(IOException::class.java) { LinkFeatures.serve(control, handler) }
        assertEquals(listOf("pong"), control.sent)
        assertEquals(listOf("sign", "got", "closed"), asked)
    }

    @Test
    fun aPayloadOfARequestNotKnownHereIsNotReadAsTheNextRequest() {
        // A later PC's request with a payload, whose piece reads as a request of signing's.
        val control = Control(request("a-later-pcs-request", 10), request("got"))
        assertThrows(IllegalArgumentException::class.java) { LinkFeatures.serve(control, handler) }
        assertEquals(listOf("closed"), asked)
    }

    @Test
    fun aPingsPayloadIsNotReadAsTheNextRequest() {
        val control = Control(request("ping", 10), request("got"))
        assertThrows(IllegalArgumentException::class.java) { LinkFeatures.serve(control, handler) }
        assertEquals(emptyList<String>(), control.sent)
        assertEquals(listOf("closed"), asked)
    }

    @Test
    fun askedAsEachRequestGoesToTheHandlerAlone() {
        val control = Control(request("ping"), request("a-later-pcs-request"), request("got"), request("got"))
        assertThrows(IOException::class.java) { LinkFeatures.serve(control, handler) { asked += "asked" } }
        assertEquals(listOf("asked", "got", "asked", "got", "closed"), asked)
    }

    /** A channel of the PC's for packets: whether it was closed. */
    private class Channel : Closeable {
        @Volatile var closed = false
        override fun close() { closed = true }
    }

    @Test
    fun theChannelsASessionDidNotTakeCloseAsItEnds() {
        val parked = Parked<Channel>()
        val pcm = Channel().also { parked.park("pc", "speakers", "pcm", it) }
        val asked = parked.count()  // its request: refused, the speakers off
        parked.close("pc", "speakers", until = asked)
        assertTrue(pcm.closed)
        assertNull(parked.take("pc", "speakers", "pcm", 0))
    }

    @Test
    fun aLaterSessionsChannelsAndOtherFeaturesStay() {
        val parked = Parked<Channel>()
        val old = Channel().also { parked.park("pc", "speakers", "pcm", it) }
        val asked = parked.count()
        // The next session's, opened as this one ends; another feature's; another PC's.
        val later = Channel().also { parked.park("pc", "speakers", "pcm", it) }
        val media = Channel().also { parked.park("pc", "display", "media", it) }
        val other = Channel().also { parked.park("other", "speakers", "pcm", it) }
        parked.close("pc", "speakers", until = asked)
        assertTrue(old.closed)
        assertFalse(later.closed || media.closed || other.closed)
        assertSame(later, parked.take("pc", "speakers", "pcm", 0))
        assertSame(media, parked.take("pc", "display", "media", 0))
        assertSame(other, parked.take("other", "speakers", "pcm", 0))
    }

    @Test
    fun theNewestIsTakenTheOlderOnesClosed() {
        val parked = Parked<Channel>()
        val old = Channel().also { parked.park("pc", "display", "media", it) }
        val new = Channel().also { parked.park("pc", "display", "media", it) }
        assertSame(new, parked.take("pc", "display", "media", 0))
        assertTrue(old.closed)
        assertFalse(new.closed)
        parked.close("pc", "display", until = parked.count())  // taken: the session's own now
        assertFalse(new.closed)
    }
}
