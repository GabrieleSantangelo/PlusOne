package io.github.gabrielesantangelo.plusone

import android.util.Log
import io.github.gabrielesantangelo.plusone.link.ChannelLink
import io.github.gabrielesantangelo.plusone.link.Handler
import io.github.gabrielesantangelo.plusone.link.LinkChannel
import io.github.gabrielesantangelo.plusone.link.LinkCore
import org.json.JSONObject
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * The app's features over the link's core (docs/LINK.md), each served
 * there by its Handler of the link of before: the PC opens the feature's
 * "control" channel, whose messages are the same requests and answers
 * (ChannelLink), and the channels for its packets first ("media" and
 * "input" for the desktop, "pcm" for the speakers). Each comes on a thread
 * of its own: those wait here for the control channel's handler to take
 * them.
 */
class LinkFeatures(private val service: CompanionService, private val core: LinkCore, handlers: List<Handler>) {
    private val waiting = ConcurrentHashMap<String, LinkedBlockingQueue<LinkChannel>>()

    init {
        for (handler in handlers) for (type in handler.types) core.serve(type) { channel -> handle(handler, channel) }
    }

    private fun handle(handler: Handler, channel: LinkChannel) {
        if (channel.name != "control") {
            queue(channel.device, channel.feature, channel.name).put(channel)
            return
        }
        val pc = core.devices().firstOrNull { it.optString("id") == channel.device }?.optString("name") ?: "PC"
        val link = ChannelLink(channel, pc) { name -> take(channel.device, channel.feature, name) }
        try {
            while (true) {
                val message = link.receive()
                when (message.optString("type")) {
                    "ping" -> { link.send(JSONObject().put("type", "pong")); continue }
                    "bye" -> break
                }
                if (message.optString("type") !in handler.types) continue
                if (!handler.handle(link, message, 0)) break
            }
        } catch (e: java.io.IOException) {
            // The PC closed it, or the session is over.
        } catch (e: Exception) {
            Log.w(CompanionService.TAG, "link: ${channel.feature}: $e")
        } finally {
            handler.closed(link)
            channel.close()
        }
    }

    private fun queue(device: String, feature: String, name: String) =
        waiting.getOrPut("$device/$feature/$name") { LinkedBlockingQueue() }

    /** The newest channel of that name the PC opened for feature, within
     * WAIT (older ones were a session's that never came: closed). */
    private fun take(device: String, feature: String, name: String): LinkChannel? {
        val queue = queue(device, feature, name)
        var channel = queue.poll(WAIT, TimeUnit.MILLISECONDS) ?: return null
        while (true) {
            val newer = queue.poll() ?: break
            channel.close()
            channel = newer
        }
        return channel
    }

    companion object {
        const val WAIT = 5_000L  // ms for each channel
    }
}
