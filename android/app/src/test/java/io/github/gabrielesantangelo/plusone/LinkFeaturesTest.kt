package io.github.gabrielesantangelo.plusone

import io.github.gabrielesantangelo.plusone.link.Handler
import io.github.gabrielesantangelo.plusone.link.Link
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.io.OutputStream
import java.net.InetAddress

/** The features over the link's core (LinkFeatures): a control channel's requests. */
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
}
