package io.github.gabrielesantangelo.plusone.link

import android.util.Log
import java.net.BindException
import java.net.InetAddress
import java.net.NetworkInterface
import javax.net.ssl.SSLServerSocket
import javax.net.ssl.SSLSocket

/** A way the PCs reach the app (plusone/link/transport.py on the PC): it
 * hands over each connection as a TLS socket not yet through its handshake
 * (Identity.serverSocket's policy), which LinkServer then authenticates and
 * speaks over. */
interface Listener {
    /** Its kind, as the PC's transports and the announcements name it. */
    val kind: String
    /** Listens until stop(), on a thread of its own: listening(where) once it
     * does (where it is reached: a port), accepted() for each connection. */
    fun start(listening: (Int) -> Unit, accepted: (SSLSocket) -> Unit)
    fun stop()
}

/** TCP on whatever network the tablet is on (a Wi-Fi Direct group would be
 * another path to it, not another listener): at port, where the PC tries
 * first before discovery says where the app is, or another if it is taken. */
class TcpListener(private val port: Int = LinkServer.PORT) : Listener {
    override val kind = "tcp"
    @Volatile private var server: SSLServerSocket? = null
    @Volatile private var stopped = false

    override fun start(listening: (Int) -> Unit, accepted: (SSLSocket) -> Unit) {
        Thread({ serve(listening, accepted) }, "plusone-tcp").start()
    }

    override fun stop() = synchronized(this) {
        stopped = true
        server?.close()
        Unit
    }

    private fun serve(listening: (Int) -> Unit, accepted: (SSLSocket) -> Unit) {
        // Not listening is not ready: tried again (the keystore's first key
        // may fail once) instead of a "Ready" nobody can reach.
        var socket: SSLServerSocket? = null
        while (socket == null) {
            if (stopped) return
            socket = try {
                try { Identity.serverSocket(port) } catch (e: BindException) { Identity.serverSocket(0) }
            } catch (e: Exception) {
                Log.e(TAG, "cannot listen, again in a while", e)
                Thread.sleep(LISTEN_RETRY)
                null
            }
        }
        synchronized(this) {
            // Stopped while the first key was being made: nothing to leave behind.
            if (stopped) return socket.close()
            server = socket
        }
        listening(socket.localPort)
        while (!socket.isClosed) {
            val client = try {
                socket.accept() as SSLSocket
            } catch (e: Exception) {
                if (socket.isClosed) break
                Log.w(TAG, "accept: $e")  // out of fds a moment, say: not the end of listening
                Thread.sleep(200)
                continue
            }
            // Never the PC: it comes over the network. Another app here, maybe,
            // posing as one to be paired with a tap on a question it timed.
            if (fromHere(client.inetAddress)) {
                Log.w(TAG, "a connection from this tablet itself: closed")
                runCatching { client.close() }
                continue
            }
            // Small messages go at once (pairing's, the answers to the PC's
            // pings), not held back by Nagle's algorithm for delayed ACKs.
            try { client.tcpNoDelay = true } catch (e: Exception) {}
            accepted(client)
        }
    }

    /** Whether a connection comes from this tablet: loopback, or one of its
     * own addresses (an app here can reach the Wi-Fi one too). */
    private fun fromHere(address: InetAddress?): Boolean =
        address == null || address.isLoopbackAddress || address.isAnyLocalAddress ||
            runCatching { NetworkInterface.getByInetAddress(address) != null }.getOrDefault(false)

    companion object {
        const val LISTEN_RETRY = 30_000L  // ms before trying to listen again
    }
}
