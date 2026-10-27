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
class ChannelLink(private val control: LinkChannel, pc: String, private val parked: (String) -> LinkChannel?)
    : Link(pc, control.device) {
    private val buffer = ByteArray(256 shl 10)

    override val address: InetAddress get() = throw IOException("no address over the link: its channels")

    override fun send(message: JSONObject) {
        val data = message.toString().toByteArray()
        synchronized(this) {  // a long one's pieces together, whatever else is sent meanwhile
            if (data.size <= CHUNK) return control.send(data)
            control.send(JSONObject().put("type", "long").put("size", data.size).toString().toByteArray())
            for (start in data.indices step CHUNK) control.send(data, start, minOf(CHUNK, data.size - start))
        }
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
    }
}
