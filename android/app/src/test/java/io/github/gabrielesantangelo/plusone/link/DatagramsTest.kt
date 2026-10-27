package io.github.gabrielesantangelo.plusone.link

import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Test
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress
import java.nio.ByteBuffer
import javax.crypto.Cipher
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

/** The PC's side as plusone/link/datagrams.py has it. */
class DatagramsTest {
    private val key = ByteArray(32) { it.toByte() }
    private val here = InetAddress.getLoopbackAddress()
    private val pc = DatagramSocket(0, here).apply { soTimeout = 2000 }
    private val app = Datagrams(key, here, pc.localPort, receiveBuffer = 1 shl 16)

    @After
    fun close() {
        pc.close()
        app.close()
    }

    private fun seal(count: Long, body: ByteArray, with: ByteArray = key, direction: Int = 1): ByteArray {
        val nonce = ByteBuffer.allocate(12).putInt(direction).putLong(count).array()
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, SecretKeySpec(with, "AES"), GCMParameterSpec(128, nonce))
        return nonce + cipher.doFinal(body)
    }

    private fun fromPc(packet: ByteArray) = pc.send(DatagramPacket(packet, packet.size, here, app.port))

    @Test
    fun onlyThePcsFreshPacketsComeThrough() {
        fromPc(seal(0, byteArrayOf(10)))
        fromPc(seal(2, byteArrayOf(12)))
        fromPc(seal(1, byteArrayOf(11)))                       // late, but not heard before
        fromPc(seal(1, byteArrayOf(11)))                       // a replay
        fromPc(seal(3, byteArrayOf(99), with = ByteArray(32)))  // another session's key
        fromPc(seal(4, byteArrayOf(99), direction = 2))        // as if from the app
        fromPc(seal(5, byteArrayOf(15)))
        assertEquals(listOf(10, 12, 11, 15), List(4) { app.receive()[0].toInt() })
    }

    @Test
    fun answersGoWhereThePcSendsFrom() {
        val other = DatagramSocket(0, here).apply { soTimeout = 2000 }
        try {
            val packet = seal(0, byteArrayOf(1))
            other.send(DatagramPacket(packet, packet.size, here, app.port))
            app.receive()
            app.send(byteArrayOf(7, 8))
            val got = DatagramPacket(ByteArray(2048), 2048)
            other.receive(got)
            val sealed = got.data.copyOf(got.length)
            assertEquals(2, ByteBuffer.wrap(sealed, 0, 4).int)  // FROM_APP
            val cipher = Cipher.getInstance("AES/GCM/NoPadding")
            cipher.init(Cipher.DECRYPT_MODE, SecretKeySpec(key, "AES"), GCMParameterSpec(128, sealed, 0, 12))
            assertArrayEquals(byteArrayOf(7, 8), cipher.doFinal(sealed, 12, sealed.size - 12))
        } finally {
            other.close()
        }
    }
}
