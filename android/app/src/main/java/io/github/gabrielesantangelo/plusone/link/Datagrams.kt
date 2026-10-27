package io.github.gabrielesantangelo.plusone.link

import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress

/**
 * Datagrams: what must not wait (frames, input, sound) as UDP packets,
 * sealed with AES-GCM (Sealed) under a key fresh for each session, which
 * the PC hands over the link with the session's request (plusone/link/
 * datagrams.py: Datagrams).
 *
 * Only the PC's address is listened to, and answered where it sends from.
 * A packet whose count was heard already, or is WINDOW or more before the
 * newest, is a replay; one that only came out of order is kept (the
 * modules put a frame's packets, or the sound's, in order themselves).
 */
class Datagrams(key: ByteArray, private val pc: InetAddress, @Volatile private var pcPort: Int, receiveBuffer: Int) {
    private val socket = DatagramSocket(0).apply { receiveBufferSize = receiveBuffer }
    private val sealed = Sealed(key)
    private val buffer = ByteArray(2048)
    private val packet = DatagramPacket(buffer, buffer.size)
    private val heard = Replays()

    /** This end's port, for the PC. */
    val port: Int = socket.localPort

    /** The next body from the PC: blocks; throws once closed. From one thread. */
    fun receive(): ByteArray {
        while (true) {
            socket.receive(packet)
            if (packet.address != pc) continue
            val body = sealed.open(buffer, packet.length) ?: continue  // not sealed with this session's key
            if (!heard.fresh(Sealed.count(buffer))) continue
            pcPort = packet.port  // answers go where the PC sends from
            return body
        }
    }

    /** body to the PC, sealed; throws if it cannot go. From any thread: one
     * at a time, so they leave in the order of their counts (the PC takes
     * only one newer than the newest it heard). */
    @Synchronized
    fun send(body: ByteArray) {
        val sealedBody = sealed.seal(body)
        socket.send(DatagramPacket(sealedBody, sealedBody.size, pc, pcPort))
    }

    fun close() = socket.close()

    companion object {
        const val WINDOW = 1024  // packets: a keyframe's, out of order on the Wi-Fi, fit well within it
    }
}

/** Which counts were heard, of the WINDOW up to the newest: a bit each. */
class Replays(private val window: Int = Datagrams.WINDOW) {
    private val seen = java.util.BitSet(window)
    private var newest = -1L

    /** Whether count is heard for the first time (and is now). */
    fun fresh(count: Long): Boolean {
        if (count < 0) return false
        if (count > newest) {
            // The bits the window moved past are for counts not heard yet.
            if (count - newest >= window) seen.clear()
            else for (c in newest + 1 until count) seen.clear((c % window).toInt())
            newest = count
        } else if (newest - count >= window || seen.get((count % window).toInt())) {
            return false
        }
        seen.set((count % window).toInt())
        return true
    }
}
