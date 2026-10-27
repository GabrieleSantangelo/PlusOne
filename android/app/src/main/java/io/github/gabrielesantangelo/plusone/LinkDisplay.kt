package io.github.gabrielesantangelo.plusone

import android.util.Log
import io.github.gabrielesantangelo.plusone.link.LinkChannel
import io.github.gabrielesantangelo.plusone.link.LinkPackets
import org.json.JSONObject
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * The PC's desktop over the link (docs/LINK.md; plusone/appdisplay.py,
 * LinkPackets). The PC opens three channels for "display": "control" (its
 * request and this answer, held until either side ends it), "media"
 * (realtime: frames, pings, the stats) and "input" (the fingers and the
 * pen). Each comes on a thread of its own: media and input wait here for
 * control's to take them. The desktop is then shown as over the link of
 * before (DisplayHandler.show), the packets the same.
 */
class LinkDisplay(private val service: CompanionService) {
    private val waiting = ConcurrentHashMap<String, LinkedBlockingQueue<LinkChannel>>()

    fun handle(channel: LinkChannel) {
        if (channel.name != "control") {
            queue(channel.device, channel.name).put(channel)
            return
        }
        channel.use { run(it) }
    }

    private fun queue(device: String, name: String) = waiting.getOrPut("$device/$name") { LinkedBlockingQueue() }

    /** The newest channel of that name from device, within WAIT (older ones
     * were a session's that never came: closed). */
    private fun take(device: String, name: String): LinkChannel? {
        val queue = queue(device, name)
        var channel = queue.poll(WAIT, TimeUnit.MILLISECONDS) ?: return null
        while (true) {
            val newer = queue.poll() ?: break
            channel.close()
            channel = newer
        }
        return channel
    }

    private fun run(control: LinkChannel) {
        val buffer = ByteArray(64 shl 10)
        val length = control.receive(buffer, WAIT.toInt())
        if (length <= 0) return
        val request = JSONObject(String(buffer, 0, length))
        if (request.optString("type") != "display") return
        val media = take(control.device, "media")
        val input = take(control.device, "input")
        val refusal = when {
            media == null || input == null -> "no channels"
            !DisplayHandler.canShow(service) -> "background"
            else -> null
        }
        if (refusal != null) {
            Log.i(CompanionService.TAG, "link display: refused ($refusal)")
            control.send(JSONObject().put("type", "refused").put("reason", refusal).toString().toByteArray())
            media?.close()
            input?.close()
            return
        }
        val pc = service.link?.devices()?.firstOrNull { it.optString("id") == control.device }?.optString("name") ?: "PC"
        DisplayHandler.show(service, DisplaySession(LinkPackets(media!!, input!!, DisplaySession.INPUT)), pc,
            ready = { control.send(it.toString().toByteArray()) },
            // Until the PC closes it, or the session is over (shutdown from end).
            hold = { while (control.receive(buffer) > 0) { /* nothing else comes on it */ } },
            end = { control.shutdown() })
    }

    companion object {
        const val WAIT = 5_000L  // ms for the request, and for each channel
    }
}
