package io.github.gabrielesantangelo.plusone

import android.app.Activity
import android.hardware.usb.UsbAccessory
import android.hardware.usb.UsbManager
import android.os.Bundle

/** Android opens it when the PC, by cable, makes the tablet its accessory
 * (link/daemon/src/usb.rs): the accessory to the service, nothing shown. */
class UsbActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Exported: what another app sends here starts nothing.
        intent.getParcelableExtra(UsbManager.EXTRA_ACCESSORY, UsbAccessory::class.java)
            ?.takeIf { ours(intent.action, it.manufacturer, it.model) }
            ?.let { accessory -> CompanionService.cable(this, accessory) }
        finish()
    }

    companion object {
        /** Plus One's accessory just plugged in, as res/xml/accessory_filter.xml has it. */
        fun ours(action: String?, manufacturer: String?, model: String?): Boolean =
            action == UsbManager.ACTION_USB_ACCESSORY_ATTACHED && manufacturer == "Plus One" && model == "Plus One"
    }
}
