package io.github.gabrielesantangelo.plusone.link

import java.io.Closeable
import java.io.IOException

/** What a session's packets travel over: the link of before's Datagrams,
 * or the link's channels (LinkPackets). The packets are the same. */
interface Packets : Closeable {
    /** body to the PC; throws if it cannot go. From one thread at a time. */
    fun send(body: ByteArray)

    /** The next body from the PC: blocks; throws once closed. From one thread. */
    fun receive(): ByteArray
}

/** A session's packets on the link's channels (docs/LINK.md): a realtime
 * one both ways, and an input one for what the fingers and the pen do
 * (the PC keeps only the newest state). */
class LinkPackets(private val media: LinkChannel, private val input: LinkChannel?, private val inputKind: Byte)
    : Packets {
    private val buffer = ByteArray(2048)

    override fun send(body: ByteArray) {
        if (input != null && body.isNotEmpty() && body[0] == inputKind) input.send(body) else media.send(body)
    }

    override fun receive(): ByteArray {
        val length = media.receive(buffer)
        if (length <= 0) throw IOException("the channel ended")
        return buffer.copyOf(length)
    }

    override fun close() {
        media.close()
        input?.close()
    }
}
