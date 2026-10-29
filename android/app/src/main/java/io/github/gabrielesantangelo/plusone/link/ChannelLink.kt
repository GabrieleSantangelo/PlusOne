package io.github.gabrielesantangelo.plusone.link

import java.io.IOException
import java.io.OutputStream
import java.net.InetAddress
import org.json.JSONObject

/** A feature's session over the link's core (docs/LINK.md): its control
 * channel carries the same JSON messages as the link of before, so its
 * Handler is the same; its packets come on channels of their own, which
 * the PC opens for it (taken with channel()). A payload (a PDF, the app)
 * follows its header on the control channel in pieces of at most CHUNK
 * bytes, as it follows on the socket of the link of before; so does a
 * message longer than that (a signed document's marks): "long" says how
 * long, the pieces follow (plusone/link/channels.py). */
class ChannelLink(private val control: MessageChannel, pc: String, private val parked: (String) -> LinkChannel?)
    : Link(pc, control.device) {
    private val buffer = ByteArray(256 shl 10)

    override val address: InetAddress get() = throw IOException("no address over the link: its channels")

    override fun send(message: JSONObject) {
        val data = message.toString().toByteArray()
        synchronized(this) {  // a long one's pieces together, whatever else is sent meanwhile
            if (data.size <= CHUNK) return put(data)
            put(JSONObject().put("type", "long").put("size", data.size).toString().toByteArray())
            for (start in data.indices step CHUNK) put(data, start, minOf(CHUNK, data.size - start))
        }
    }

    /** One message, once the PC makes room for it, within SEND_WAIT: else
     * it takes nothing (stuck, or gone and the session not knowing yet), and
     * the channel ends. Not left holding the lock and whoever sends next,
     * nor half a long message read on as the next one. */
    private fun put(data: ByteArray, offset: Int = 0, length: Int = data.size) {
        if (control.send(data, offset, length, SEND_WAIT)) return
        control.shutdown()
        throw IOException("the PC took nothing for ${SEND_WAIT / 1000} s")
    }

    /** The next message, a long one put together (at most maxHeader bytes). */
    override fun receive(maxHeader: Int): JSONObject {
        val message = JSONObject(String(buffer, 0, next()))
        if (message.optString("type") != "long") return message
        val size = message.optLong("size", -1)
        if (size !in 1..maxHeader.toLong()) throw IOException("a long message of $size bytes")
        val data = java.io.ByteArrayOutputStream(size.toInt())
        while (data.size() < size) data.write(buffer, 0, next())
        if (data.size().toLong() != size) throw IOException("a long message longer than it said")
        return JSONObject(data.toString(Charsets.UTF_8.name()))
    }

    override fun readPayload(size: Long, out: OutputStream) {
        var left = size
        while (left > 0) {
            val length = next()
            if (length > left) throw IOException("more payload than its size")
            out.write(buffer, 0, length)
            left -= length
        }
    }

    private fun next(): Int {
        val length = control.receive(buffer)
        if (length <= 0) throw IOException("the PC closed it")
        return length
    }

    /** Until the PC says "stop" or closes it (or the link's session is over:
     * its own liveness, no silence to time out here). */
    override fun hold(silentMs: Int) {
        while (receive().optString("type") != "stop") continue
    }

    override fun close() = control.shutdown()

    override fun channel(name: String): LinkChannel? = parked(name)

    companion object {
        const val CHUNK = 64 shl 10  // bytes of a payload or a long message per channel message
        const val SEND_WAIT = 30_000  // ms for room for each message or piece: the PC's IO_TIMEOUT (plusone/link/link.py)
    }
}

/** A channel's end as a feature's session uses it: a [LinkChannel] (a
 * test's, off the device). */
interface MessageChannel {
    /** The PC's id (its certificate's fingerprint). */
    val device: String

    /** One message, once there is room for it within timeoutMs (-1: as
     * long as it takes): false if there was none in time, nothing sent. */
    fun send(data: ByteArray, offset: Int, length: Int, timeoutMs: Int): Boolean

    /** One message into buffer: its length; 0 the channel's end, -1
     * nothing within timeoutMs (-1: as long as it takes). */
    fun receive(buffer: ByteArray, timeoutMs: Int = -1): Int

    /** Its end, now: whoever waits on it wakes. */
    fun shutdown()
}
