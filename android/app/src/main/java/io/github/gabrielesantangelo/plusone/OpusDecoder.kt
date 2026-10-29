package io.github.gabrielesantangelo.plusone

import android.media.MediaCodec
import android.media.MediaCodecList
import android.media.MediaFormat
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * The PC's sound in Opus (plusone/pcm.py sends it so while the link's budget
 * is low): each 5 ms packet back to PCM with the system's decoder, in the
 * order they come (its state runs on from one to the next). No pre-skip: the
 * encoder's own delay stays (a few ms), and each packet comes out whole.
 */
class OpusDecoder(rate: Int, channels: Int) {
    private val codec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS)
    private val info = MediaCodec.BufferInfo()
    /** The numbers of the packets in the decoder, oldest first: each sound
     * that comes out is the oldest's. Not its timestamp: Android's decoder
     * gives the first one's and the samples since (5000 µs a packet), and a
     * packet read as 4999 missed before it was concealed: the sound crackled. */
    private val inside = ArrayDeque<Int>()

    init {
        val format = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, rate, channels)
        format.setByteBuffer("csd-0", ByteBuffer.wrap(head(channels, rate)))
        format.setByteBuffer("csd-1", ByteBuffer.wrap(nanoseconds(0)))           // the codec's delay
        format.setByteBuffer("csd-2", ByteBuffer.wrap(nanoseconds(80_000_000)))  // seeking's preroll
        codec.configure(format, null, null, 0)
        codec.start()
    }

    /** The packet numbered number (its Opus at offset, length bytes): what
     * came out, each (number, its PCM: 16-bit, interleaved); usually this
     * one alone, maybe one before it the decoder held. */
    fun decode(packet: ByteArray, offset: Int, length: Int, number: Int): List<Pair<Int, ByteArray>> {
        val input = codec.dequeueInputBuffer(WAIT_US)
        if (input < 0) return emptyList()
        codec.getInputBuffer(input)!!.apply {
            clear()
            put(packet, offset, length)
        }
        codec.queueInputBuffer(input, 0, length, number.toLong() and 0xffffffffL, 0)
        inside.addLast(number)
        while (inside.size > HELD_MOST) inside.removeFirst()  // eaten with nothing out: not waited for
        val out = ArrayList<Pair<Int, ByteArray>>(1)
        var wait = WAIT_US
        while (true) {
            val index = codec.dequeueOutputBuffer(info, wait)
            if (index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED) continue
            if (index < 0) return out
            val pcm = ByteArray(info.size)
            codec.getOutputBuffer(index)!!.apply {
                position(info.offset)
                get(pcm)
            }
            codec.releaseOutputBuffer(index, false)
            val of = inside.removeFirstOrNull() ?: continue
            if (pcm.isNotEmpty()) out.add(of to pcm)
            wait = 0  // more only if it has them now
        }
    }

    fun release() {
        try {
            codec.stop()
        } catch (e: IllegalStateException) {
        }
        codec.release()
    }

    companion object {
        const val WAIT_US = 5_000L
        const val HELD_MOST = 8  // packets

        /** RFC 7845's identification header: version 1, no pre-skip, no
         * gain, one stream (mapping family 0). */
        fun head(channels: Int, rate: Int): ByteArray = ByteBuffer.allocate(19).order(ByteOrder.LITTLE_ENDIAN)
            .put("OpusHead".toByteArray(Charsets.US_ASCII)).put(1).put(channels.toByte())
            .putShort(0).putInt(rate).putShort(0).put(0).array()

        private fun nanoseconds(value: Long): ByteArray =
            ByteBuffer.allocate(8).order(ByteOrder.nativeOrder()).putLong(value).array()

        /** Whether this tablet has an Opus decoder (any Android since 5 should). */
        fun available(): Boolean = try {
            MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.any { info ->
                !info.isEncoder && info.supportedTypes.any { it.equals(MediaFormat.MIMETYPE_AUDIO_OPUS, true) }
            }
        } catch (e: RuntimeException) {
            false
        }
    }
}
