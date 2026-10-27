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
        intent.getParcelableExtra(UsbManager.EXTRA_ACCESSORY, UsbAccessory::class.java)?.let { accessory ->
            CompanionService.cable(this, accessory)
        }
        finish()
    }
}
