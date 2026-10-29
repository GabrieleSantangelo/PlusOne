package io.github.gabrielesantangelo.plusone

import android.hardware.usb.UsbManager
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** The cable's activity is exported: only Plus One's accessory, attached, goes on to the service. */
class UsbActivityTest {
    private val attached = UsbManager.ACTION_USB_ACCESSORY_ATTACHED
    private val plugged = listOf<Pair<String?, String?>>("Plus One" to "Plus One")

    @Test
    fun plusOnesAccessoryJustAttachedIsOurs() {
        assertTrue(UsbActivity.ours(attached, "Plus One", "Plus One", plugged))
        assertTrue(UsbActivity.ours(attached, "Plus One", "Plus One", listOf("Other" to "Other") + plugged))
    }

    @Test
    fun anotherAccessoryOrAnotherActionIsNot() {
        assertFalse(UsbActivity.ours(attached, "Plus One", "Other", plugged))
        assertFalse(UsbActivity.ours(attached, "Other", "Plus One", plugged))
        assertFalse(UsbActivity.ours(attached, null, null, plugged))
        assertFalse(UsbActivity.ours(null, "Plus One", "Plus One", plugged))
        assertFalse(UsbActivity.ours("android.intent.action.MAIN", "Plus One", "Plus One", plugged))
        assertFalse(UsbActivity.ours(UsbManager.ACTION_USB_ACCESSORY_DETACHED, "Plus One", "Plus One", plugged))
    }

    @Test
    fun plusOnesAccessoryMadeUpWithNoneAttachedIsNot() {
        assertFalse(UsbActivity.ours(attached, "Plus One", "Plus One", emptyList()))
        assertFalse(UsbActivity.ours(attached, "Plus One", "Plus One", listOf("Plus One" to "Other", null to null)))
    }
}
