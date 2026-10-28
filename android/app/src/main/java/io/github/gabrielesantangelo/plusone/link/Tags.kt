package io.github.gabrielesantangelo.plusone.link

import java.nio.ByteBuffer
import java.security.MessageDigest

/**
 * What the app announces on the network instead of its id (SEC-15): a tag
 * per paired PC, which only that PC can tell is its tablet's, and only for
 * a quarter of an hour. Nobody else on the network can follow the tablet,
 * nor learn which PCs it is paired with. The same on the PC
 * (plusone/link/tags.py) and in the link's daemon (mdns.rs):
 *     tag = SHA-256(app's fingerprint ‖ PC's fingerprint ‖ epoch, 8 bytes BE)[:8]
 *     epoch = Unix time / EPOCH
 */
object Tags {
    const val EPOCH = 900L  // seconds
    const val MOST = 8      // PCs announced: a TXT entry is 255 bytes at most

    fun tag(app: String, pc: String, epoch: Long): String {
        val digest = MessageDigest.getInstance("SHA-256").run {
            update(bytes(app))
            update(bytes(pc))
            update(ByteBuffer.allocate(8).putLong(epoch).array())
            digest()
        }
        return digest.take(8).joinToString("") { "%02x".format(it) }
    }

    fun epoch(nowMs: Long = System.currentTimeMillis()) = nowMs / 1000 / EPOCH

    /** The tags for these PCs now, as the TXT entry "t" holds them. */
    fun announced(app: String, pcs: Collection<String>, epoch: Long = epoch()): String =
        pcs.sorted().take(MOST).joinToString(",") { tag(app, it, epoch) }

    private fun bytes(hex: String) = ByteArray(hex.length / 2) { hex.substring(2 * it, 2 * it + 2).toInt(16).toByte() }
}
