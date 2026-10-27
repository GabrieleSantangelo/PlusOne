package io.github.gabrielesantangelo.plusone.link

import java.nio.ByteBuffer
import javax.crypto.Cipher
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

/**
 * Packets sealed with AES-GCM under a session's key, which the PC handed over
 * the paired TLS link (Datagrams; plusone/link/datagrams.py): nonce (12
 * bytes: 4 for the direction, 8 counting, never the same twice) | sealed body.
 */
class Sealed(key: ByteArray) {
    private val key = SecretKeySpec(key, "AES")
    private val opener = Cipher.getInstance("AES/GCM/NoPadding")
    private val sealer = Cipher.getInstance("AES/GCM/NoPadding")
    private var sent = 0L

    /** The body of a packet from the PC, or null if it is not one sealed with
     * this key. From one thread. */
    fun open(packet: ByteArray, length: Int): ByteArray? {
        if (length < NONCE + TAG + 1 || packet[3] != FROM_PC) return null
        return try {
            opener.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(TAG * 8, packet, 0, NONCE))
            opener.doFinal(packet, NONCE, length - NONCE)
        } catch (e: Exception) {
            null
        }
    }

    /** body as a packet for the PC. */
    @Synchronized
    fun seal(body: ByteArray): ByteArray {
        val nonce = ByteBuffer.allocate(NONCE).putInt(FROM_APP.toInt()).putLong(sent++).array()
        sealer.init(Cipher.ENCRYPT_MODE, key, GCMParameterSpec(TAG * 8, nonce))
        return nonce + sealer.doFinal(body)
    }

    companion object {
        /** A packet's count, from its nonce (opened: the PC's, then). */
        fun count(packet: ByteArray): Long = ByteBuffer.wrap(packet, 4, 8).long

        const val FROM_PC: Byte = 1
        const val FROM_APP: Byte = 2
        const val NONCE = 12
        const val TAG = 16
    }
}
