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
import java.io.Closeable
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong

/**
 * The app's features over the link's core (docs/LINK.md), each served
 * there by its Handler of the link of before: the PC opens the feature's
 * "control" channel, whose messages are the same requests and answers
 * (ChannelLink), then the channels for its packets ("media" and "input"
 * for the desktop, "pcm" for the speakers), before the request that needs
 * them; a payload (a PDF to sign) follows its request on the control
 * channel. Each comes on a thread of its own: those wait here (Parked) for
 * the control channel's handler to take them, or for its session's end.
 */
class LinkFeatures(private val service: CompanionService, private val core: LinkCore, handlers: List<Handler>) {
    private val parked = Parked<LinkChannel>()

    init {
        for (handler in handlers) for (type in handler.types) {
            core.serve(type, handler.linkVersion(type)) { channel -> handle(handler, channel) }
        }
    }

    private fun handle(handler: Handler, channel: LinkChannel) {
        if (channel.name != "control") {
            parked.park(channel.device, channel.feature, channel.name, channel)
            return
        }
        // The name it was paired with, else the one it says: in notifications, to be read.
        val pc = LinkServer.readable(Paired.all(service)[channel.device]
            ?: core.devices().firstOrNull { it.optString("id") == channel.device }?.optString("name").orEmpty())
        val link = ChannelLink(channel, pc) { name -> parked.take(channel.device, channel.feature, name, WAIT) }
        var asked = 0L  // the channels parked as the PC's last request came: its own, or older
        try {
            serve(link, handler) { asked = parked.count() }
        } catch (e: java.io.IOException) {
            // The PC closed it, or the session is over.
        } catch (e: Throwable) {
            // Errors too: a PC's deeply nested JSON overflows the parser's
            // stack, and an Error left uncaught ends the process.
            Log.w(CompanionService.TAG, "link: ${channel.feature}: $e")
        } finally {
            channel.close()
            // What the session did not take (refused: the speakers off, say)
            // goes with it; a later session's, parked meanwhile, stays.
            parked.close(channel.device, channel.feature, until = asked)
        }
    }

    companion object {
        const val WAIT = 5_000L  // ms for each channel

        /** The PC's requests on a feature's control channel, to its handler
         * as on the link of before (Requests: ping answered, a payload's size
         * checked before anything is passed over, so that no payload is read
         * as the next message), until the PC says bye, the handler takes the
         * link, or it goes. */
        internal fun serve(link: Link, handler: Handler, asked: () -> Unit = {}) =
            Requests(listOf(handler)).serve(link, otherVersion = false, asked)
    }
}

/** The PC's channels for its features' packets, each parked here until the
 * handler of its feature's control channel takes it: the PC opens them
 * after that channel, before the request that needs them. Counted as they
 * come, so that what a session did not take is closed as it ends, not what
 * a later one's PC opened meanwhile. */
internal class Parked<C : Closeable> {
    private class Car<T>(val channel: T, val number: Long)

    private val count = AtomicLong()
    private val queues = ConcurrentHashMap<String, LinkedBlockingQueue<Car<C>>>()

    private fun queue(device: String, feature: String, name: String) =
        queues.getOrPut("$device/$feature/$name") { LinkedBlockingQueue() }

    fun park(device: String, feature: String, name: String, channel: C) =
        queue(device, feature, name).put(Car(channel, count.incrementAndGet()))

    /** How many have come so far. */
    fun count(): Long = count.get()

    /** The newest channel of that name the PC opened for feature, within
     * waitMs (older ones were a session's that never came: closed). */
    fun take(device: String, feature: String, name: String, waitMs: Long): C? {
        val queue = queue(device, feature, name)
        var car = queue.poll(waitMs, TimeUnit.MILLISECONDS) ?: return null
        while (true) {
            val newer = queue.poll() ?: break
            car.channel.close()
            car = newer
        }
        return car.channel
    }

    /** The PC's (device) channels for feature still parked, of the first
     * until to come: closed. */
    fun close(device: String, feature: String, until: Long) {
        for ((key, queue) in queues) {
            if (!key.startsWith("$device/$feature/")) continue
            for (car in queue) if (car.number <= until && queue.remove(car)) car.channel.close()
        }
    }
}
