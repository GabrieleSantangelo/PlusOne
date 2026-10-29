package io.github.gabrielesantangelo.plusone

import android.content.Intent
import android.provider.Settings
import io.github.gabrielesantangelo.plusone.link.Datagrams
import io.github.gabrielesantangelo.plusone.link.Handler
import io.github.gabrielesantangelo.plusone.link.Link
import io.github.gabrielesantangelo.plusone.link.LinkPackets
import org.json.JSONObject

/** The PC's desktop on this screen (DisplaySession, DisplayActivity), until
 * the PC hangs up or says stop. Over the link of before its packets are
 * Datagrams, the key for them coming with the request, fresh for each
 * session (plusone/appdisplay.py); over the link's core, the channels the
 * PC opened for them ("media", "input"). */
class DisplayHandler(private val service: CompanionService) : Handler {
    override val types = setOf("display")

    override fun handle(link: Link, request: JSONObject, size: Long): Boolean {
        val media = link.channel("media")
        val input = if (media != null) link.channel("input") else null
        if (media != null && input == null) media.close()
        val overCore = media != null && input != null
        val key = request.optString("key").chunked(2).mapNotNull { it.toIntOrNull(16)?.toByte() }.toByteArray()
        val port = request.optInt("port")
        require(overCore || (key.size == 32 && port in 1..65535)) { "bad display request" }
        if (!canShow(service)) {
            media?.close()
            input?.close()
            link.refuse("background")
            return false
        }
        // Room for a keyframe's packets at once.
        val datagrams = if (overCore) null else Datagrams(key, link.address, port, receiveBuffer = 4 shl 20)
        val packets = datagrams ?: LinkPackets(media!!, input, DisplaySession.INPUT)
        show(service, DisplaySession(packets), link.pc,
            ready = { link.send(if (datagrams != null) it.put("port", datagrams.port) else it) },
            hold = { link.hold() },
            end = { link.close() })
        return false
    }

    companion object {
        /** Whether the desktop may come up now (over other apps, or this one's in front). */
        fun canShow(service: CompanionService) = Settings.canDrawOverlays(service) || Front.shown

        /** session on screen until hold returns (the PC hung up, or end ended
         * it here: Back, Stop); ready sends the PC "display-ready". */
        fun show(service: CompanionService, session: DisplaySession, pc: String,
                 ready: (JSONObject) -> Unit, hold: () -> Unit, end: () -> Unit) {
            // Ended here (Back, Stop): the link closes, and the PC with it.
            session.onEnd = { Thread { end() }.start() }
            session.onVisible = { service.lowLatency() }  // no low-latency Wi-Fi while nothing is shown
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
                ready(JSONObject().put("type", "display-ready")
                    .put("width", maxOf(mode.physicalWidth, mode.physicalHeight))
                    .put("height", minOf(mode.physicalWidth, mode.physicalHeight))
                    .put("refresh", mode.refreshRate.toDouble()))
                service.startActivity(Intent(service, DisplayActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
                service.displaying = pc
                service.showSessions()
                service.lowLatency()
                hold()
            } finally {
                session.stop()
                synchronized(DisplaySession) { if (DisplaySession.current === session) DisplaySession.current = null }
                android.os.Handler(service.mainLooper).post { DisplayActivity.ended(session) }
                if (DisplaySession.current == null) service.displaying = null
                service.showSessions()
                service.lowLatency()
            }
        }
    }
}
