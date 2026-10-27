package io.github.gabrielesantangelo.plusone.link

import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.ByteArrayOutputStream
import java.io.DataOutputStream
import java.io.EOFException
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/** A link's requests, the PC's side written as plusone/link/wire.py does. */
class RequestsTest {
    private val listening = ServerSocket(0, 1, InetAddress.getLoopbackAddress())
    private val pc = Socket(listening.inetAddress, listening.localPort).apply { soTimeout = 2000 }
    private val app = listening.accept()
    private val out = DataOutputStream(pc.getOutputStream())
    private val answers = Wire(pc)
    private val log = LinkedBlockingQueue<String>()

    @After
    fun close() {
        pc.close()
        app.close()
        listening.close()
    }

    /** sign takes a payload and the next request; display takes the link. */
    private val handlers = listOf(
        object : Handler {
            override val types = setOf("sign", "got")
            override fun maxPayload(type: String) = if (type == "sign") 100L else 0L
            override fun handle(link: Link, request: JSONObject, size: Long): Boolean {
                val payload = ByteArrayOutputStream().also { link.readPayload(size, it) }
                log.add("${request.optString("type")} ${payload.size()}")
                link.send(JSONObject().put("type", "done"))
                return true
            }
            override fun closed(link: Link) { log.add("closed") }
        },
        object : Handler {
            override val types = setOf("display", "update")
            override val acrossVersions = true
            override fun handle(link: Link, request: JSONObject, size: Long): Boolean {
                log.add(request.optString("type"))
                return false
            }
        },
    )

    private fun serve(otherVersion: Boolean = false): Thread = Thread {
        try {
            Requests(handlers).serve(Link(app, Wire(app), "PC", "fp"), otherVersion)
        } catch (e: Exception) {
            log.add("ended: ${e.javaClass.simpleName}")
        } finally {
            app.close()
        }
    }.apply { start() }

    private fun send(header: JSONObject, payload: ByteArray? = null) {
        if (payload != null) header.put("size", payload.size)
        val bytes = header.toString().toByteArray()
        out.writeInt(bytes.size)
        out.write(bytes)
        payload?.let { out.write(it) }
        out.flush()
    }

    private fun request(type: String) = JSONObject().put("type", type)

    private fun next() = log.poll(2, TimeUnit.SECONDS)

    private fun hangsUp(): Boolean = try {
        answers.receive()
        false
    } catch (e: EOFException) {
        true
    }

    @Test
    fun eachRequestGoesToItsHandler() {
        val serving = serve()
        send(request("ping"))
        assertEquals("pong", answers.receive().optString("type"))
        send(request("sign"), ByteArray(40))
        assertEquals("done", answers.receive().optString("type"))
        assertEquals("sign 40", next())
        send(request("a-later-pcs-request"))  // not known here: the link stays
        send(request("got"))
        assertEquals("done", answers.receive().optString("type"))
        assertEquals("got 0", next())
        send(request("display"))  // takes the link: when it ends, so does the link
        assertEquals("display", next())
        assertEquals("closed", next())
        assertTrue(hangsUp())
        serving.join(2000)
    }

    @Test
    fun aPayloadWhereNoneGoesEndsTheLink() {
        serve()
        send(request("ping"), ByteArray(4))
        assertEquals("closed", next())
        assertEquals("ended: IllegalArgumentException", next())
        assertTrue(hangsUp())
    }

    @Test
    fun tooBigAPayloadEndsTheLink() {
        serve()
        send(request("sign"), ByteArray(101))
        assertEquals("closed", next())
        assertEquals("ended: IllegalArgumentException", next())
    }

    @Test
    fun anotherVersionOnlyUpdates() {
        serve(otherVersion = true)
        send(request("ping"))
        assertEquals("pong", answers.receive().optString("type"))
        send(request("update"))
        assertEquals("update", next())
        assertEquals("closed", next())
    }

    @Test
    fun anotherVersionIsNotServedTheRest() {
        serve(otherVersion = true)
        send(request("got"))
        assertEquals("closed", next())
        assertTrue(hangsUp())
    }

    @Test
    fun byeEndsTheLink() {
        serve()
        send(request("bye"))
        assertEquals("closed", next())
        assertTrue(hangsUp())
    }
}
