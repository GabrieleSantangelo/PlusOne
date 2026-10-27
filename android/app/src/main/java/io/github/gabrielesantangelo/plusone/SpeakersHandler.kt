package io.github.gabrielesantangelo.plusone

import android.util.Log
import io.github.gabrielesantangelo.plusone.link.Handler
import io.github.gabrielesantangelo.plusone.link.Link
import org.json.JSONObject

/** The PC's sound on the speakers (PcmPlayback), while one of its programs
 * plays and the user lets it (Plus One → For the PC). The request's "sdp"
 * is the session's key and the PC's port (JSON; plusone/pcm.py). */
class SpeakersHandler(private val service: CompanionService) : Handler {
    override val types = setOf("speakers")

    override fun handle(link: Link, request: JSONObject, size: Long): Boolean {
        if (!Options.speakers(service)) {
            link.refuse("off")
            return false
        }
        val playback = try {
            val offer = JSONObject(request.optString("sdp"))
            val key = offer.optString("key").chunked(2).mapNotNull { it.toIntOrNull(16)?.toByte() }.toByteArray()
            val port = offer.optInt("port")
            require(key.size == 32 && port in 1..65535 && offer.optInt("channels") == PcmPlayback.CHANNELS &&
                offer.optInt("rate") in 8000..96000 && offer.optInt("frames") in 48..480) { "bad speakers request" }
            PcmPlayback(key, link.address, port, offer.optInt("rate"), offer.optInt("frames"))
        } catch (e: Exception) {
            Log.w(CompanionService.TAG, "speakers not available: $e")
            link.refuse("unavailable")
            return false
        }
        service.began(playback, CompanionService.Session("speakers", link))
        try {
            playback.start()
            link.send(JSONObject().put("type", "answer")
                .put("sdp", JSONObject().put("port", playback.port).toString()))
            service.showSessions()
            service.lowLatency(true)
            link.hold()
        } finally {
            playback.stop()
            service.ended(playback)
        }
        return false
    }
}
