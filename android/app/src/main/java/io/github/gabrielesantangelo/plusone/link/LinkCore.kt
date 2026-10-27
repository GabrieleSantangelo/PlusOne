package io.github.gabrielesantangelo.plusone.link

import android.os.ParcelFileDescriptor
import android.system.Os
import android.system.OsConstants
import android.system.StructPollfd
import android.util.Log
import org.json.JSONObject
import java.io.Closeable
import java.nio.ByteBuffer
import java.security.PrivateKey
import java.security.Signature
import java.security.cert.X509Certificate

/**
 * The link's core (link/android, Rust; docs/LINK.md) in the app: one
 * session with each paired PC, channels in it for the features. The same
 * code as the PC's plus-one-link; this is its Kotlin side.
 *
 * A feature serves (the PC's channels for it come to its handler, each on
 * a thread of its own) or opens a channel; a channel is a message socket
 * (one write, one message), a [LinkChannel].
 */
class LinkCore(name: String, certificate: X509Certificate, key: PrivateKey) : Closeable {
    private val handle: Long
    private val served = java.util.concurrent.ConcurrentHashMap<String, (LinkChannel) -> Unit>()
    @Volatile private var stopped = false
    @Volatile var onEvent: ((JSONObject) -> Unit)? = null

    /** Signs for the TLS handshake (from Rust, on its thread): the key never
     * leaves the keystore. */
    private class Signer(private val key: PrivateKey) {
        @Suppress("unused")  // called through JNI
        fun sign(data: ByteArray): ByteArray =
            Signature.getInstance("SHA256withECDSA").run { initSign(key); update(data); sign() }
    }

    init {
        handle = nativeStart(name, certificate.encoded, Signer(key))
        check(handle != 0L) { "the link's core did not start" }
        Thread(::events, "link-events").apply { isDaemon = true }.start()
    }

    /** The PCs trusted: their certificates' fingerprints (Paired). */
    fun setPaired(fingerprints: Collection<String>) = nativeSetPaired(handle, fingerprints.toTypedArray())

    /** PCs connect here (0: any port): the port. */
    fun listen(port: Int = 0): Int = request(JSONObject().put("op", "listen").put("port", port)).getInt("port")

    /** fd (a USB accessory's: path "usb") to the core, which owns it until
     * the cable is out: the PC's sessions on it, one after the other. */
    fun accept(fd: Int, path: String) {
        request(JSONObject().put("op", "accept_fd").put("fd", fd).put("path", path))
    }

    /** A stream the app made (a Bluetooth socket, bridged: path "bt") to the
     * core, which owns fd from then on: the PC makes its session (or a path
     * for the one there is) on it. */
    fun acceptStream(fd: Int, path: String) {
        request(JSONObject().put("op", "accept_stream").put("fd", fd).put("path", path))
    }

    /** A feature of the app's: the PC's channels for it to handler. */
    fun serve(feature: String, version: Int = 1, handler: (LinkChannel) -> Unit) {
        served[feature] = handler
        request(JSONObject().put("op", "serve").put("feature", feature).put("version", version))
    }

    fun unserve(feature: String) {
        served.remove(feature)
        request(JSONObject().put("op", "unserve").put("feature", feature))
    }

    /** A channel with a PC (its id: the fingerprint), once it takes it. */
    fun open(device: String, feature: String, name: String, kind: String): LinkChannel {
        val reply = request(JSONObject().put("op", "open").put("device", device).put("feature", feature)
            .put("channel", name).put("class", kind))
        return LinkChannel(reply.getInt("fd"), device, feature, name, kind, reply.optInt("id"))
    }

    /** The PCs with a session now: [{"id", "name", "features", "rtt_ms", "dropped"}]. */
    fun devices(): List<JSONObject> {
        val devices = request(JSONObject().put("op", "devices")).getJSONArray("devices")
        return List(devices.length()) { devices.getJSONObject(it) }
    }

    private fun request(message: JSONObject): JSONObject {
        val reply = JSONObject(nativeRequest(handle, message.toString()))
        if (!reply.optBoolean("ok")) throw java.io.IOException(reply.optString("error", "refused"))
        return reply
    }

    private fun events() {
        while (!stopped) {
            val event = JSONObject(nativeNext(handle, EVENT_WAIT) ?: continue)
            if (event.optString("event") == "incoming") {
                val channel = LinkChannel(event.getInt("fd"), event.optString("device"), event.optString("feature"),
                    event.optString("channel"), event.optString("class"), event.optInt("id"))
                val handler = served[channel.feature]
                if (handler == null) { channel.close(); continue }
                // The handler's from then on: it closes it (or keeps it, for another to use).
                Thread({
                    try { handler(channel) } catch (e: Exception) {
                        Log.w(TAG, "link: ${channel.feature}: $e")
                        channel.close()
                    }
                }, "link-${channel.feature}").start()
            } else {
                Log.i(TAG, "link: ${event.optString("event")} ${event.optString("name", event.optString("device").take(8))}")
                onEvent?.invoke(event)
            }
        }
    }

    override fun close() {
        stopped = true
        nativeStop(handle)
    }

    private external fun nativeStart(name: String, certificate: ByteArray, signer: Any): Long
    private external fun nativeSetPaired(handle: Long, fingerprints: Array<String>)
    private external fun nativeRequest(handle: Long, request: String): String
    private external fun nativeNext(handle: Long, timeoutMs: Int): String?
    private external fun nativeStop(handle: Long)

    companion object {
        const val TAG = "PlusOne"
        const val CONTROL = "control"
        const val INPUT = "input"
        const val REALTIME = "realtime"
        const val SERVICE_TYPE = "_plusone-link._tcp"
        private const val EVENT_WAIT = 500  // ms: how soon close() is noticed

        init { System.loadLibrary("plusonelink") }
    }
}

/** A channel's end: a message socket. [receive] -1 nothing in time, 0 its
 * end (closed by the PC, or the session over), else one message's length.
 * A served feature's handler owns the channels it is given. */
class LinkChannel(fd: Int, val device: String, val feature: String, val name: String, val kind: String,
                  val id: Int) : Closeable {
    private val socket = ParcelFileDescriptor.adoptFd(fd)
    val fd: java.io.FileDescriptor get() = socket.fileDescriptor

    /** One message (waits for room). */
    fun send(data: ByteArray, offset: Int = 0, length: Int = data.size - offset) {
        Os.write(fd, data, offset, length)
    }

    fun send(buffer: ByteBuffer) {
        Os.write(fd, buffer)
    }

    fun receive(buffer: ByteArray, timeoutMs: Int = -1): Int {
        if (timeoutMs >= 0) {
            val poll = arrayOf(StructPollfd().apply { this.fd = this@LinkChannel.fd; events = OsConstants.POLLIN.toShort() })
            if (Os.poll(poll, timeoutMs) == 0) return -1
        }
        return Os.read(fd, buffer, 0, buffer.size)
    }

    fun receive(buffer: ByteBuffer): Int = Os.read(fd, buffer)

    /** Its end, now: whoever waits in receive() gets 0 (a close alone would
     * not wake it). */
    fun shutdown() {
        try { Os.shutdown(fd, OsConstants.SHUT_RDWR) } catch (e: android.system.ErrnoException) { /* closed already */ }
    }

    override fun close() {
        shutdown()
        socket.close()
    }
}
