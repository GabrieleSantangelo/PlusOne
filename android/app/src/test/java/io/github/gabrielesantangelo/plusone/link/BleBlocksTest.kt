package io.github.gabrielesantangelo.plusone.link

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class BleBlocksTest {
    private val app = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
    private val pc = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100"
    private val other = "0102030405060708090a0b0c0d0e0f100102030405060708090a0b0c0d0e0f10"
    private fun hex(bytes: ByteArray) = bytes.joinToString("") { "%02x".format(it) }

    @Test fun theSameBlockAsTheLinksDaemon() {
        // The vector ble.rs has too.
        val block = BleBlocks.seal(app, pc, 1_000, 7, byteArrayOf(192.toByte(), 168.toByte(), 1, 112), 47101)
        assertEquals("07e980cedfb1742b53e008ca", hex(block))
    }

    @Test fun oneBlockForEachPcTwoAtMost() {
        val ip = byteArrayOf(10, 0, 0, 2)
        val data = BleBlocks.data(app, listOf(pc, other, pc), ip, 1, 5, { 3 })
        assertEquals(1 + 2 * BleBlocks.BLOCK, data.size)
        assertEquals(BleBlocks.VERSION, data[0].toInt())
        assertArrayEquals(BleBlocks.seal(app, other, 5, 3, ip, 1), data.copyOfRange(1 + BleBlocks.BLOCK, data.size))
        assertEquals(1, BleBlocks.data(app, emptyList(), ip, 1, 5, { 3 }).size)
    }

    @Test fun theFirstIpv4AddressIsTold() {
        assertArrayEquals(byteArrayOf(192.toByte(), 168.toByte(), 1, 5), BleBlocks.ipv4(listOf("2001:db8::1", "192.168.1.5", "10.0.0.1")))
        assertNull(BleBlocks.ipv4(listOf("2001:db8::1", "300.1.1.1", "a.b.c.d")))
    }

    @Test fun theAdvertisementFitsTheAirWithItsHeader() {
        // 31 bytes: length, type and company identifier (4), then the data.
        assertEquals(true, 4 + 1 + BleBlocks.MOST * BleBlocks.BLOCK <= 31)
    }

    @Test fun theSamePairingAdvertisementAsTheLinksDaemon() {
        // The vector ble.rs has too.
        val data = BleBlocks.pairingData(app, byteArrayOf(192.toByte(), 168.toByte(), 1, 112), 47101)
        assertEquals("020011223344556677c0a80170b7fd", hex(data))
        assertEquals(15, data.size)
        assertEquals(true, 4 + data.size <= 31)
    }

    @Test fun theNameFitsTheScanResponseWithoutCuttingACharacter() {
        assertEquals("Tab S10 FE+", String(BleBlocks.nameData("  Tab S10 FE+ ")))
        val long = BleBlocks.nameData("Tab S10 FE+ di Gabriele Santangelo")
        assertEquals(true, long.size <= BleBlocks.NAME_MOST)
        // Two bytes a character, cut on an odd byte: the whole character before it stays.
        val accents = BleBlocks.nameData("é".repeat(20))
        assertEquals(26, accents.size)
        assertEquals("é".repeat(13), String(accents))
    }
}
