package io.github.gabrielesantangelo.plusone.link

import java.io.IOException
import java.io.OutputStream
import java.net.InetAddress
import org.json.JSONObject

/** A feature's session over the link's core (docs/LINK.md): its control
 * channel carries the same JSON messages as the link of before, so its
 * Handler is the same; its packets come on channels of their own, which
 * the PC opens for it (taken with channel()). No payloads: those need a
 * bulk channel. */
class ChannelLink(private val control: LinkChannel, pc: String, private val parked: (String) -> LinkChannel?)
    : Link(pc, control.device) {
    private val buffer = ByteArray(256 shl 10)

    override val address: InetAddress get() = throw IOException("no address over the link: its channels")

    override fun send(message: JSONObject) = control.send(message.toString().toByteArray())

    override fun receive(maxHeader: Int): JSONObject {
        val length = control.receive(buffer)
        if (length <= 0) throw IOException("the PC closed it")
        return JSONObject(String(buffer, 0, length))
    }

    override fun readPayload(size: Long, out: OutputStream) = throw IOException("no payload on a control channel")

    /** Until the PC says "stop" or closes it (or the link's session is over:
     * its own liveness, no silence to time out here). */
    override fun hold(silentMs: Int) {
        while (receive().optString("type") != "stop") continue
    }

    override fun close() = control.shutdown()

    override fun channel(name: String): LinkChannel? = parked(name)
}
