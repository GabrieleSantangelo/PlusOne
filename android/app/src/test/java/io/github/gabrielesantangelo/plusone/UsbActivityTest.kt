package io.github.gabrielesantangelo.plusone

import android.hardware.usb.UsbManager
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** The cable's activity is exported: only Plus One's accessory, attached, goes on to the service. */
class UsbActivityTest {
    private val attached = UsbManager.ACTION_USB_ACCESSORY_ATTACHED

    @Test
    fun plusOnesAccessoryJustAttachedIsOurs() {
        assertTrue(UsbActivity.ours(attached, "Plus One", "Plus One"))
    }

    @Test
    fun anotherAccessoryOrAnotherActionIsNot() {
        assertFalse(UsbActivity.ours(attached, "Plus One", "Other"))
        assertFalse(UsbActivity.ours(attached, "Other", "Plus One"))
        assertFalse(UsbActivity.ours(attached, null, null))
        assertFalse(UsbActivity.ours(null, "Plus One", "Plus One"))
        assertFalse(UsbActivity.ours("android.intent.action.MAIN", "Plus One", "Plus One"))
        assertFalse(UsbActivity.ours(UsbManager.ACTION_USB_ACCESSORY_DETACHED, "Plus One", "Plus One"))
    }
}
