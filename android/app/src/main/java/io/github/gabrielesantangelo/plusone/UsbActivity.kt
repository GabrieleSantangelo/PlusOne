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
            ?.takeIf { ours(intent.action, it.manufacturer, it.model, attached()) }
            ?.let { accessory -> CompanionService.cable(this, accessory) }
        finish()
    }

    /** The accessories plugged in now, as manufacturer and model (no
     * permission needed for those, unlike equals()'s serial); none if
     * Android does not say. */
    private fun attached(): List<Pair<String?, String?>> = runCatching {
        getSystemService(UsbManager::class.java)?.accessoryList.orEmpty().map { it.manufacturer to it.model }
    }.getOrDefault(emptyList())

    companion object {
        /** Plus One's accessory just plugged in, as res/xml/accessory_filter.xml
         * has it, and one of attached (a made-up one is not). */
        fun ours(action: String?, manufacturer: String?, model: String?, attached: List<Pair<String?, String?>>): Boolean =
            action == UsbManager.ACTION_USB_ACCESSORY_ATTACHED && plusOne(manufacturer, model) &&
                attached.any { (manufacturer, model) -> plusOne(manufacturer, model) }

        private fun plusOne(manufacturer: String?, model: String?) = manufacturer == "Plus One" && model == "Plus One"
    }
}
