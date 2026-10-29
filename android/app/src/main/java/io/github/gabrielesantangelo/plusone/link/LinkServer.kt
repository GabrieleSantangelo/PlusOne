package io.github.gabrielesantangelo.plusone.link

import android.content.Context
import android.util.Log
import org.json.JSONObject
import java.net.InetAddress
import java.security.SecureRandom
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.RejectedExecutionException
import java.util.concurrent.ScheduledFuture
import java.util.concurrent.SynchronousQueue
import java.util.concurrent.ThreadPoolExecutor
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicReference
import javax.net.ssl.SSLSocket

const val TAG = "PlusOne"

/**
 * Plus One's connectivity on the tablet (the PC's is plusone/link): the
 * links of the paired PCs, under the app's services (signing, the desktop,
 * the speakers, the camera and the microphone, updates), which never touch
 * a socket of their own.
 *
 * The primitives:
 * - Listeners (Listener.kt): how the PCs reach the app (TCP today), each
 *   connection a TLS socket (Identity: the app's certificate);
 * - Announcers (Announcer.kt): how the PCs learn where (mDNS today);
 * - trust: Identity, Paired, and the pairing here;
 * - links (Link.kt): a paired PC's, whose requests go each to the Handler
 *   of its type (Requests);
 * - Datagrams (Datagrams.kt): a session's sealed UDP.
 *
 * A PC's connection: the hello each way, the pairing when the user asked
 * for it on the PC (numeric comparison, the user's consent here), then its
 * requests. A PC of another protocol version may only update the app.
 */
class LinkServer(
    private val context: Context,
    handlers: List<Handler>,
    private val host: Host,
    private val listeners: List<Listener>,
    private val announcers: List<Announcer>,
) {
    /** What the app around the links decides. */
    interface Host {
        /** This tablet's name, as the PC shows it. */
        fun deviceName(): String
        /** The app's versionCode: in the hello and the announcements. */
        fun appVersion(): Long
        /** The user's answer to pairing with pc (whose code is code, from
         * the address from, its fingerprint fingerprint): null when no
         * question may be asked now. */
        fun askPairing(pc: String, code: String, from: String, fingerprint: String): Boolean?
        /** Until when (elapsedRealtime ms) a PC not paired may ask to pair:
         * the window the user opens here (PairingPolicy). */
        fun pairingWindowUntil(): Long
        /** The PC withdrew the question (said no there, or went). */
        fun pairingWithdrawn(code: String)
        /** A PC connected (+1) or went (-1): for when the app may rest. */
        fun connected(delta: Int)
    }

    private val requests = Requests(handlers)
    // A connection per PDF: a PC sending a batch has a few dozen at most
    // (sign_jobs.py MAX_JOBS); more connections are turned away.
    private val connections = ThreadPoolExecutor(0, MAX_CONNECTIONS, 60, TimeUnit.SECONDS, SynchronousQueue())
    private val unpaired = AtomicInteger()     // connections not yet known to be a paired PC
    private val handshaking = AtomicInteger()  // connections still in their TLS handshake
    private val gate = HandshakeGate()  // and how many from each address, how often
    private val links = ConcurrentHashMap<String, MutableSet<Link>>()  // each paired PC's open links

    fun start() {
        for (listener in listeners) {
            listener.start(listening = { where -> announce(listener.kind, where) }, accepted = ::accepted)
        }
    }

    fun stop() {
        announcers.forEach { it.stop() }
        listeners.forEach { it.stop() }
        connections.shutdownNow()
    }

    /** The user forgot this PC: what it has open ends now (a capture in
     * progress, a PDF being signed), not when it happens to hang up. */
    fun disconnect(fingerprint: String) {
        links.remove(fingerprint)?.forEach { it.close() }
    }

    private fun announce(kind: String, where: Int) {
        try {
            val id = Identity.fingerprint(Identity.certificate.encoded)
            for (announcer in announcers) announcer.announce(kind, where, id, host.deviceName(), host.appVersion())
        } catch (e: Exception) {
            // On the listener's thread: never its end. The PC still finds the app where it was.
            Log.e(TAG, "not announced: $e")
        }
    }

    private fun accepted(client: SSLSocket) {
        // Connections not through TLS yet, few at a time, and few from one
        // address: strangers that never finish their handshake cannot take
        // every thread from a paired PC.
        val from = client.inetAddress?.hostAddress ?: ""
        if (!gate.start(from)) {
            runCatching { client.close() }
            return
        }
        if (handshaking.incrementAndGet() > MAX_HANDSHAKING) {
            shaken(from)
            runCatching { client.close() }
            return
        }
        try {
            connections.execute { handle(client) }
        } catch (e: RejectedExecutionException) {
            shaken(from)
            runCatching { client.close() }
        }
    }

    /** A handshake over (through, failed or turned away): its slot back. */
    private fun shaken(from: String) {
        handshaking.decrementAndGet()
        gate.done(from)
    }

    /** One PC: hello, pairing if either side does not know the other and the
     * PC asks to pair (its user chose this tablet), then its requests. */
    private fun handle(socket: SSLSocket) {
        val from = socket.inetAddress?.hostAddress ?: ""
        host.connected(1)
        var link: Link? = null
        var stranger = false  // counted in unpaired: not (yet) a paired PC
        var shaking = true  // counted in handshaking
        try {
            socket.use {
                // Nothing waits long for a peer that is not even paired: the
                // whole exchange (TLS, hello, pairing's messages) in
                // HANDSHAKE_DEADLINE, not only each read, and a few such
                // peers at a time (a paired PC is never turned away for them).
                val guard = Wire.WATCHDOG.schedule({ socket.close() }, HANDSHAKE_DEADLINE, TimeUnit.MILLISECONDS)
                socket.soTimeout = HANDSHAKE_TIMEOUT
                socket.startHandshake()
                shaken(from)
                shaking = false
                val pc = socket.session.peerCertificates[0].encoded
                val fingerprint = Identity.fingerprint(pc)
                val known = Paired.contains(context, fingerprint)
                if (!known) {
                    stranger = true
                    if (unpaired.incrementAndGet() > MAX_UNPAIRED) return
                }
                val wire = Wire(socket)
                val hello = wire.receive(Wire.UNPAIRED_HEADER)
                require(hello.optString("type") == "hello") { "expected hello" }
                val pcName = readable(hello.optString("name"))
                if (!PairingPolicy.allowed(android.os.SystemClock.elapsedRealtime(), host.pairingWindowUntil(), known,
                        hello.optBoolean("pair"))) {
                    // Not known here, and the pairing window shut: told only
                    // that, nothing of this tablet (no name, app or address).
                    wire.send(JSONObject().put("type", "hello").put("version", PROTOCOL).put("paired", false)
                        .put("pairing", "closed"))
                    return
                }
                // "at": the address the PC reached, so it can tell this
                // tablet answered itself, not through someone passing the
                // connection on.
                wire.send(JSONObject().put("type", "hello").put("name", host.deviceName())
                    .put("version", PROTOCOL).put("paired", known).put("at", address(socket.localAddress))
                    .put("app", host.appVersion()))
                // Another version: the PC sees ours and says so to its user;
                // a PC that knows this tablet, and this tablet it, may still
                // hand over its update (and nothing else).
                val otherVersion = hello.optInt("version") != PROTOCOL
                if (otherVersion && !(known && hello.optBoolean("paired"))) return
                if (!known || !hello.optBoolean("paired")) {
                    // Only when the user pairs, in Plus One's window on the PC.
                    if (!hello.optBoolean("pair")) return
                    if (!pair(socket, wire, pc, fingerprint, pcName, guard)) return
                    Paired.add(context, fingerprint, pcName)
                }
                guard.cancel(false)
                val opened = SocketLink(socket, wire, pcName, fingerprint)
                link = opened
                // Its links, closed the moment the user forgets it (disconnect()).
                links.computeIfAbsent(fingerprint) { ConcurrentHashMap.newKeySet() }.add(opened)
                if (!Paired.contains(context, fingerprint)) return  // forgotten just now
                if (stranger) {
                    unpaired.decrementAndGet()
                    stranger = false
                }
                // The PC asks "ping" every 15 s while the user signs.
                socket.soTimeout = PAIRED_TIMEOUT
                requests.serve(opened, otherVersion)
            }
        } catch (e: Throwable) {
            // Errors too: a peer's deeply nested JSON overflows the parser's
            // stack, and an uncaught Error on a pool thread ends the process.
            Log.i(TAG, "connection ended: $e")
        } finally {
            if (shaking) shaken(from)
            if (stranger) unpaired.decrementAndGet()
            link?.let { links[it.fingerprint]?.remove(it) }
            host.connected(-1)
        }
    }

    /** Numeric comparison: the app commits to its random number, learns the
     * PC's, shows its own; both screens then show a code made of the two
     * certificates and the two numbers, and the user confirms it on both. */
    private fun pair(socket: SSLSocket, wire: Wire, pc: ByteArray, fingerprint: String, pcName: String,
                     guard: ScheduledFuture<*>): Boolean {
        val mine = Identity.certificate.encoded
        val nonce = ByteArray(16).also { SecureRandom().nextBytes(it) }
        wire.send(JSONObject().put("type", "pair-commit").put("commit", Identity.commitment(nonce, mine, pc)))
        val number = wire.receive(Wire.UNPAIRED_HEADER)
        val pcNonce = hex(number.optString("nonce"))
        require(number.optString("type") == "pair-nonce" && pcNonce?.size == 16) { "expected the PC's number" }
        wire.send(JSONObject().put("type", "pair-nonce").put("nonce", nonce.joinToString("") { "%02x".format(it) }))
        val code = Identity.pairingCode(pc, mine, pcNonce!!, nonce)
        guard.cancel(false)  // the user takes their time
        socket.soTimeout = PAIRING_TIMEOUT
        // The PC's answer may come first (refused there): it closes the
        // question here too, instead of leaving it up for two minutes.
        val answer = AtomicReference<String>()
        val listening = Thread {
            val type = try { wire.receive(Wire.UNPAIRED_HEADER).optString("type") } catch (e: Throwable) { "" }
            answer.set(type)
            if (type != "pair-ok") host.pairingWithdrawn(code)
        }.apply { start() }
        val from = address(socket.inetAddress)
        val accepted = host.askPairing(pcName, code, from, fingerprint) ?: return false  // another question is up, or too soon
        wire.send(JSONObject().put("type", if (accepted) "pair-ok" else "pair-no"))
        listening.join(PAIRING_TIMEOUT.toLong())
        return accepted && answer.get() == "pair-ok"
    }

    /** An address as the PC writes it: IPv4 without the "::ffff:" prefix, no scope. */
    private fun address(address: InetAddress): String =
        address.hostAddress.orEmpty().substringBefore('%').removePrefix("::ffff:")

    private fun hex(s: String): ByteArray? =
        if (s.length % 2 != 0 || !s.all { it in "0123456789abcdef" }) null
        else ByteArray(s.length / 2) { s.substring(2 * it, 2 * it + 2).toInt(16).toByte() }

    companion object {
        const val PROTOCOL = 3
        const val PORT = 47236                 // plusone/link/link.py APP_PORT; another if it is taken
        const val MAX_CONNECTIONS = 32
        const val MAX_UNPAIRED = 4             // connections of peers not yet known, at a time
        const val MAX_HANDSHAKING = 8          // connections still in their TLS handshake, at a time
        const val HANDSHAKE_DEADLINE = 20_000L // ms for an unknown peer's whole handshake
        const val HANDSHAKE_TIMEOUT = 20_000   // ms for TLS, hello and the pairing messages
        const val PAIRING_TIMEOUT = 150_000    // ms: the user compares the codes
        const val PAIRED_TIMEOUT = 90_000      // ms without a word from a paired PC
        // ms without a word from the PC (it pings every 5 s, link.py
        // KEEPALIVE) before a session ends: a PC gone without closing the
        // link (asleep, off the network) does not keep the microphone on.
        const val SILENT_PC = 15_000

        /** The PC's name as it says it, to be read: no control characters, none
         * that turn the text around them (U+202E and the like), not too long. */
        fun readable(name: String): String =
            name.filter { !Character.isISOControl(it) && Character.getType(it) != Character.FORMAT.toInt() }
                .trim().take(64).ifEmpty { "PC" }
    }
}
