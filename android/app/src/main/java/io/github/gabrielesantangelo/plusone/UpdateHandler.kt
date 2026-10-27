package io.github.gabrielesantangelo.plusone

import android.util.Log
import io.github.gabrielesantangelo.plusone.link.Handler
import io.github.gabrielesantangelo.plusone.link.Link
import org.json.JSONObject
import java.io.File

/** The paired PC's update of this app (plusone/updates.py): taken, checked,
 * installed (this process ends with it), unless the PC uses the tablet now
 * or the user does not take updates from that PC ("off": PcOptions). Also
 * from a PC of another protocol version: that is how they meet again. Over
 * the link, "update" is served at the app's versionCode: how a PC that
 * reaches the tablet only by cable knows it. */
class UpdateHandler(private val service: CompanionService) : Handler {
    override val types = setOf("update")
    override fun maxPayload(type: String) = Updates.MAX_APK
    override val acrossVersions = true
    override fun linkVersion(type: String) = service.appVersion().toInt()

    override fun handle(link: Link, request: JSONObject, size: Long): Boolean {
        require(size in 1..Updates.MAX_APK) { "bad update" }
        if (!PcOptions.updates(service, link.fingerprint)) {
            // Not from this PC: its payload read past (it follows the request), kept nowhere.
            link.readPayload(size, object : java.io.OutputStream() {
                override fun write(b: Int) {}
                override fun write(b: ByteArray, off: Int, len: Int) {}
            })
            Log.i(CompanionService.TAG, "update refused: off for this PC")
            link.refuse("off")
            return false
        }
        val file = File(service.cacheDir, "update.apk")
        try {
            file.outputStream().use { link.readPayload(size, it) }
            val reason = if (service.busy()) "busy" else Updates.check(service, file, request.optString("sha256"))
            if (reason != null) {
                Log.i(CompanionService.TAG, "update refused: $reason")
                link.refuse(reason)
                return false
            }
            link.send(JSONObject().put("type", "updating"))
            Log.i(CompanionService.TAG, "updating to ${request.optLong("version")}")
            Updates.install(service, file)
        } finally {
            file.delete()  // the installer's session has its own copy
        }
        return false
    }
}
