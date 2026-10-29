package io.github.gabrielesantangelo.plusone

import android.util.Log
import io.github.gabrielesantangelo.plusone.link.ChannelLink
import io.github.gabrielesantangelo.plusone.link.Handler
import io.github.gabrielesantangelo.plusone.link.Link
import io.github.gabrielesantangelo.plusone.link.LinkChannel
import io.github.gabrielesantangelo.plusone.link.LinkCore
import io.github.gabrielesantangelo.plusone.link.LinkServer
import io.github.gabrielesantangelo.plusone.link.Paired
import io.github.gabrielesantangelo.plusone.link.Requests
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * The app's features over the link's core (docs/LINK.md), each served
 * there by its Handler of the link of before: the PC opens the feature's
 * "control" channel, whose messages are the same requests and answers
 * (ChannelLink), and the channels for its packets first ("media" and
 * "input" for the desktop, "pcm" for the speakers); a payload (a PDF to
 * sign) follows its request on the control channel. Each comes on a thread
 * of its own: those wait here for the control channel's handler to take
 * them.
 */
class LinkFeatures(private val service: CompanionService, private val core: LinkCore, handlers: List<Handler>) {
    private val waiting = ConcurrentHashMap<String, LinkedBlockingQueue<LinkChannel>>()

    init {
        for (handler in handlers) for (type in handler.types) {
            core.serve(type, handler.linkVersion(type)) { channel -> handle(handler, channel) }
        }
    }

    private fun handle(handler: Handler, channel: LinkChannel) {
        if (channel.name != "control") {
            queue(channel.device, channel.feature, channel.name).put(channel)
            return
        }
        // The name it was paired with, else the one it says: in notifications, to be read.
        val pc = LinkServer.readable(Paired.all(service)[channel.device]
            ?: core.devices().firstOrNull { it.optString("id") == channel.device }?.optString("name").orEmpty())
        val link = ChannelLink(channel, pc) { name -> take(channel.device, channel.feature, name) }
        try {
            serve(link, handler)
        } catch (e: java.io.IOException) {
            // The PC closed it, or the session is over.
        } catch (e: Throwable) {
            // Errors too: a PC's deeply nested JSON overflows the parser's
            // stack, and an Error left uncaught ends the process.
            Log.w(CompanionService.TAG, "link: ${channel.feature}: $e")
        } finally {
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

        /** The PC's requests on a feature's control channel, to its handler
         * as on the link of before (Requests: ping answered, a payload's size
         * checked before anything is passed over, so that no payload is read
         * as the next message), until the PC says bye, the handler takes the
         * link, or it goes. */
        internal fun serve(link: Link, handler: Handler) = Requests(listOf(handler)).serve(link, otherVersion = false)
    }
}
