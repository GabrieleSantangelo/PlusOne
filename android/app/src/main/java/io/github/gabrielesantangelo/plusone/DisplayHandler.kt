package io.github.gabrielesantangelo.plusone

import android.content.Intent
import android.provider.Settings
import io.github.gabrielesantangelo.plusone.link.Handler
import io.github.gabrielesantangelo.plusone.link.Link
import org.json.JSONObject

/** The PC's desktop on this screen (DisplaySession, DisplayActivity), until
 * the PC hangs up or says stop. The key for its packets comes over the
 * link, fresh for each session (plusone/appdisplay.py). */
class DisplayHandler(private val service: CompanionService) : Handler {
    override val types = setOf("display")

    override fun handle(link: Link, request: JSONObject, size: Long): Boolean {
        val key = request.optString("key").chunked(2).mapNotNull { it.toIntOrNull(16)?.toByte() }.toByteArray()
        val port = request.optInt("port")
        require(key.size == 32 && port in 1..65535) { "bad display request" }
        if (!(Settings.canDrawOverlays(service) || Front.shown)) {
            link.refuse("background")
            return false
        }
        val session = DisplaySession(key, link.address, port)
        // Ended here (Back, Stop): the link closes, and the PC with it.
        session.onEnd = { Thread { link.close() }.start() }
        session.onVisible = { service.lowLatency(false) }  // no low-latency Wi-Fi while nothing is shown
        // This one takes the screen: the one before ends, its PC told (its
        // link closes), not left encoding for nobody with its monitor up.
        val before = synchronized(DisplaySession) {
            DisplaySession.current.also { DisplaySession.current = session }
        }
        before?.stop()
        before?.end()
        session.start()
        try {
            val mode = service.getSystemService(android.hardware.display.DisplayManager::class.java)
                .getDisplay(android.view.Display.DEFAULT_DISPLAY).mode
            link.send(JSONObject().put("type", "display-ready").put("port", session.datagrams.port)
                .put("width", maxOf(mode.physicalWidth, mode.physicalHeight))
                .put("height", minOf(mode.physicalWidth, mode.physicalHeight))
                .put("refresh", mode.refreshRate.toDouble()))
            service.startActivity(Intent(service, DisplayActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            service.displaying = link.pc
            service.showSessions()
            service.lowLatency(true)
            link.hold()
        } finally {
            session.stop()
            synchronized(DisplaySession) { if (DisplaySession.current === session) DisplaySession.current = null }
            android.os.Handler(service.mainLooper).post { DisplayActivity.ended(session) }
            if (DisplaySession.current == null) service.displaying = null
            service.showSessions()
            service.lowLatency(false)
        }
        return false
    }
}
