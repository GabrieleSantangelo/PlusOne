package io.github.gabrielesantangelo.plusone

import org.junit.Assert.assertArrayEquals
import org.junit.Test

class OpusDecoderTest {
    @Test
    fun theIdentificationHeaderIsRfc7845s() {
        val expected = "OpusHead".toByteArray(Charsets.US_ASCII) + byteArrayOf(
            1, 2,                          // version, channels
            0, 0,                          // no pre-skip
            0x80.toByte(), 0xbb.toByte(), 0, 0,  // 48000, little-endian
            0, 0,                          // no gain
            0,                             // one stream, mapping family 0
        )
        assertArrayEquals(expected, OpusDecoder.head(2, 48000))
    }
}
