package io.github.gabrielesantangelo.plusone.link

import org.json.JSONObject
import java.io.OutputStream
import java.net.InetAddress
import java.net.Socket

/** A service of the app on the links: the request types it answers. */
interface Handler {
    val types: Set<String>
    /** The most a request of type may carry (0: none). */
    fun maxPayload(type: String): Long = 0
    /** Whether it answers a PC of another protocol version too (an update). */
    val acrossVersions: Boolean get() = false
    /** One request on link: true to read the link's next one, false when
     * this one took the link (a session, over now: the link closes). */
    fun handle(link: Link, request: JSONObject, size: Long): Boolean
    /** The link went, whatever ended it. */
    fun closed(link: Link) {}
}

/** A paired PC's link: pc its name, fingerprint its certificate's. */
class Link(private val socket: Socket, private val wire: Wire, val pc: String, val fingerprint: String) {
    /** The PC's address. */
    val address: InetAddress get() = socket.inetAddress

    fun send(message: JSONObject) = wire.send(message)

    fun receive(maxHeader: Int = Wire.MAX_HEADER): JSONObject = wire.receive(maxHeader)

    fun readPayload(size: Long, out: OutputStream) = wire.readPayload(size, out)

    fun refuse(reason: String) = send(JSONObject().put("type", "refused").put("reason", reason))

    /** A session held: until the PC says "stop" or the link goes, or
     * nothing comes for silentMs (the PC pings while it holds one). */
    fun hold(silentMs: Int = LinkServer.SILENT_PC) {
        socket.soTimeout = silentMs
        while (wire.receive().optString("type") != "stop") continue
    }

    /** From any thread: the link ends (whatever waits on it wakes). */
    fun close() {
        runCatching { socket.close() }
    }
}

/** A link's requests, after its hello: each to the Handler of its type,
 * until the PC says bye, a handler takes the link, or it goes. Ping is
 * answered here, for every service. */
class Requests(private val handlers: List<Handler>) {
    private val byType = handlers.flatMap { handler -> handler.types.map { it to handler } }.toMap()

    /** Blocks while link lasts; otherVersion: the PC speaks another
     * protocol version, which only an update is answered for. */
    fun serve(link: Link, otherVersion: Boolean) {
        try {
            while (true) {
                val message = link.receive()
                val type = message.optString("type")
                val size = message.optLong("size", 0)
                val handler = byType[type]
                // A payload only where one goes: else it would be read as the next message.
                require(size == 0L || size in 1..(handler?.maxPayload(type) ?: 0)) { "unexpected payload" }
                when (type) {
                    "ping" -> { link.send(JSONObject().put("type", "pong")); continue }
                    "bye" -> return
                }
                if (otherVersion && handler?.acrossVersions != true) return
                if (handler == null) continue  // a request of a later PC: not known here
                if (!handler.handle(link, message, size)) return
            }
        } finally {
            handlers.forEach { it.closed(link) }  // a job waits for the PC to come back, say
        }
    }
}
