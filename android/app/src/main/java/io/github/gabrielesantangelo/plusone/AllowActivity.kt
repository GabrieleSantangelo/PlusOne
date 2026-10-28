package io.github.gabrielesantangelo.plusone

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Bundle
import android.provider.Settings

/**
 * "Allow" on the notification that a PC wants the camera or the microphone
 * (Notifications.askDevice): Android's question if needed, then the switch
 * on, and gone. Not exported: only that notification opens it, so no other
 * app can turn the PC's camera on by starting it.
 */
class AllowActivity : Activity() {
    private var kind = ""
    private var pc = ""  // the PC that asked: the only one allowed by this yes

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        kind = intent.getStringExtra(EXTRA_KIND)?.takeIf { it in PERMISSIONS } ?: return finish()
        pc = intent.getStringExtra(EXTRA_PC) ?: return finish()
        Notifications.cancelDevice(this, kind)
        val permission = PERMISSIONS.getValue(kind)
        when {
            checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED -> allowed(true)
            savedInstanceState != null -> Unit  // recreated while Android asks
            shouldShowRequestPermissionRationale(permission) || !Options.asked(this, kind) ->
                requestPermissions(arrayOf(permission), 1)
            else -> {  // denied for good: only the app's settings can change it
                startActivity(Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.parse("package:$packageName")))
                finish()
            }
        }
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, results)
        Options.setAsked(this, kind)
        allowed(results.firstOrNull() == PackageManager.PERMISSION_GRANTED)
    }

    private fun allowed(granted: Boolean) {
        if (granted) {
            if (kind == MainActivity.CAMERA) Options.setCamera(this, true) else Options.setMicrophone(this, true)
            PcOptions.setDevice(this, pc, kind, true)
            // In front now: the service takes the camera or microphone at once,
            // and the PC (which asks again every few seconds) gets it.
            CompanionService.instance?.elevate()
        }
        finish()
    }

    companion object {
        const val EXTRA_KIND = "kind"
        const val EXTRA_PC = "pc"
        private val PERMISSIONS = mapOf(MainActivity.CAMERA to Manifest.permission.CAMERA,
            MainActivity.MICROPHONE to Manifest.permission.RECORD_AUDIO)
    }
}
