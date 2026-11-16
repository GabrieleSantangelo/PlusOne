package io.github.gabrielesantangelo.plusone.link

import java.security.MessageDigest
import javax.crypto.Mac
import javax.crypto.spec.SecretKeySpec

/**
 * What the app advertises over Bluetooth LE for the PCs it is paired with,
 * where multicast does not go and the PC's last address no longer holds: its
 * own address on the network it is on, which only that PC can read. The same
 * in the link's daemon (ble.rs, where the format is told in full):
 *     manufacturer data, company [COMPANY]: [VERSION], then a block for each PC ([MOST] at most)
 *     block = nonce (1) ‖ ciphertext (6: IPv4 ‖ port) ‖ mac (5)
 * It is only a hint: the PC dials the address, and who answers is checked
 * by its certificate.
 */
object BleBlocks {
    const val COMPANY = 0xFFFF
    const val VERSION = 1
    const val SLOT = 900L  // seconds, as the tags' epoch
    const val BLOCK = 12
    const val MOST = 2  // blocks: 31 bytes less the 4 of the manufacturer data's header

    /** The first byte of the advertisement of a tablet whose user holds pairing open (ble.rs). */
    const val PAIRING = 2
    /** The service data of its scan response (16-bit UUID 0xFFFF) holds its name. */
    const val NAME_UUID = "0000ffff-0000-1000-8000-00805f9b34fb"
    const val NAME_MOST = 27  // bytes: 31 less the 2 of the header and the 2 of the UUID

    fun slot(nowMs: Long = System.currentTimeMillis()) = nowMs / 1000 / SLOT

    /** The block that tells address (IPv4 octets, port) to the PC, in slot. */
    fun seal(app: String, pc: String, slot: Long, nonce: Int, ip: ByteArray, port: Int): ByteArray {
        require(ip.size == 4 && port in 1..65535)
        val key = key(app, pc)
        val plain = ip + byteArrayOf((port shr 8).toByte(), port.toByte())
        val stream = part(key, 'k', slot, nonce, ByteArray(0))
        val ciphertext = ByteArray(6) { (plain[it].toInt() xor stream[it].toInt()).toByte() }
        return byteArrayOf(nonce.toByte()) + ciphertext + part(key, 'm', slot, nonce, ciphertext).copyOf(BLOCK - 7)
    }

    /** The manufacturer data for these PCs (the first [MOST]): none if there is nothing to tell. */
    fun data(app: String, pcs: List<String>, ip: ByteArray, port: Int, slot: Long, nonce: () -> Int): ByteArray =
        byteArrayOf(VERSION.toByte()) + pcs.take(MOST).fold(ByteArray(0)) { all, pc -> all + seal(app, pc, slot, nonce(), ip, port) }

    /**
     * What a tablet with pairing open advertises, in the clear (there is no key
     * between it and a PC yet, as with earphones to be paired): PAIRING, its id
     * (the start of its fingerprint: 8 bytes), where it listens. The code both
     * screens show is what pairs.
     */
    fun pairingData(app: String, ip: ByteArray, port: Int): ByteArray {
        require(ip.size == 4 && port in 1..65535 && app.length >= 16)
        return byteArrayOf(PAIRING.toByte()) + bytes(app.substring(0, 16)) + ip +
            byteArrayOf((port shr 8).toByte(), port.toByte())
    }

    /** The name as the scan response holds it: UTF-8, cut to NAME_MOST bytes between two characters. */
    fun nameData(name: String): ByteArray {
        var bytes = name.trim().toByteArray()
        if (bytes.size <= NAME_MOST) return bytes
        var end = NAME_MOST
        while (end > 0 && (bytes[end].toInt() and 0xC0) == 0x80) end--  // not in the middle of a character
        bytes = bytes.copyOf(end)
        return bytes
    }

    /** The first IPv4 address of the list, as its four octets. */
    fun ipv4(addresses: List<String>): ByteArray? = addresses.firstNotNullOfOrNull { address ->
        val octets = address.split('.')
        if (octets.size != 4) return@firstNotNullOfOrNull null
        val numbers = octets.map { it.toIntOrNull()?.takeIf { n -> n in 0..255 } ?: return@firstNotNullOfOrNull null }
        ByteArray(4) { numbers[it].toByte() }
    }

    private fun key(app: String, pc: String): SecretKeySpec {
        val digest = MessageDigest.getInstance("SHA-256").run {
            update("plus-one ble 1".toByteArray())
            update(bytes(app))
            update(bytes(pc))
            digest()
        }
        return SecretKeySpec(digest, "HmacSHA256")
    }

    private fun part(key: SecretKeySpec, label: Char, slot: Long, nonce: Int, ciphertext: ByteArray): ByteArray =
        Mac.getInstance("HmacSHA256").run {
            init(key)
            update(label.code.toByte())
            for (shift in 56 downTo 0 step 8) update((slot shr shift).toByte())
            update(nonce.toByte())
            update(ciphertext)
            doFinal()
        }

    private fun bytes(hex: String) = ByteArray(hex.length / 2) { hex.substring(2 * it, 2 * it + 2).toInt(16).toByte() }
}
