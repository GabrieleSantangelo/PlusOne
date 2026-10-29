package io.github.gabrielesantangelo.plusone

import android.bluetooth.BluetoothAdapter
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class BtRetriesTest {
    @Test
    fun listensAsBluetoothComesOnAndNotAsItGoes() {
        val retries = BtRetries()
        assertEquals(true, retries.state(BluetoothAdapter.STATE_ON))
        assertEquals(false, retries.state(BluetoothAdapter.STATE_TURNING_OFF))
        assertEquals(false, retries.state(BluetoothAdapter.STATE_OFF))
        assertNull(retries.state(BluetoothAdapter.STATE_TURNING_ON))
        assertNull(retries.state(BluetoothAdapter.ERROR))
    }

    @Test
    fun aFewTriesThenNoMore() {
        val retries = BtRetries()
        assertEquals(BtRetries.WAITS, List(BtRetries.WAITS.size) { retries.failed(started = true, enabled = true) })
        assertNull(retries.failed(started = true, enabled = true))
    }

    @Test
    fun notAgainStoppedOrWithBluetoothOff() {
        val retries = BtRetries()
        assertNull(retries.failed(started = false, enabled = true))
        assertNull(retries.failed(started = true, enabled = false))
        assertEquals(1_000L, retries.failed(started = true, enabled = true))  // those took no try
    }

    @Test
    fun triesAnewOnceBluetoothComesOnOrAPcConnects() {
        val retries = BtRetries()
        repeat(BtRetries.WAITS.size) { retries.failed(started = true, enabled = true) }
        retries.state(BluetoothAdapter.STATE_ON)
        assertEquals(1_000L, retries.failed(started = true, enabled = true))
        retries.failed(started = true, enabled = true)
        retries.reset()
        assertEquals(1_000L, retries.failed(started = true, enabled = true))
    }

    @Test
    fun goingOffKeepsTheCount() {
        val retries = BtRetries()
        repeat(BtRetries.WAITS.size) { retries.failed(started = true, enabled = true) }
        retries.state(BluetoothAdapter.STATE_TURNING_OFF)
        assertNull(retries.failed(started = true, enabled = true))
    }
}
