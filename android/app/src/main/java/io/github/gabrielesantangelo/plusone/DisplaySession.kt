package io.github.gabrielesantangelo.plusone

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.Executors
import io.github.gabrielesantangelo.plusone.link.Packets

/**
 * The PC's desktop on this screen. The PC sends each encoded frame (H.264)
 * as soon as it has it, cut into UDP packets sealed with AES-GCM under a key
 * it handed over the paired TLS link for this session. A frame goes to
 * MediaCodec as soon as it is whole and is shown as soon as it is decoded:
 * no jitter buffer. A frame with a packet missing is dropped, and a keyframe
 * asked for (the next frames would be decoded against what is not there).
 *
 * A packet: nonce (12 bytes: 4 for the direction, 8 counting) | sealed body.
 * Bodies from the PC, after a kind byte:
 *   VIDEO  flags (1: keyframe) | frame u32 | index u16 | count u16 | captured i64 | H.264
 *   PONG   our ping's time i64 | the PC's time i64
 * and to the PC: KEYFRAME, PING (now i64, shown u8), STATS (see stats()),
 * INPUT (DisplayInput).
 * Times are each side's monotonic clock in nanoseconds; PING and PONG tell
 * the offset between them, so the delay from the PC's capture to the frame
 * decoded here is measured, not guessed. (Not to the glass: MediaCodec's
 * "rendered" times only gave back the time it was released with.)
 */
class DisplaySession(private val packets: Packets) {
    @Volatile private var running = true
    // Packets to the PC go out in order from this thread (input comes on
    // the main one, where Android allows no network).
    private val outbox = Executors.newSingleThreadExecutor()

    // MediaCodec in asynchronous mode: its callbacks on this thread, the
    // input buffers it frees queued for the receiving thread.
    private val callbacks = HandlerThread("plusone-display").apply { start() }
    private val handler = Handler(callbacks.looper)
    private val free = ConcurrentLinkedQueue<Int>()
    @Volatile private var codec: MediaCodec? = null
    private var surface: Surface? = null  // attach()'s, for a decoder made again after an error
    @Volatile private var restarts = 0    // decoders made again since one last decoded a frame
    private val main = Handler(android.os.Looper.getMainLooper())

    /** Ends the session from here (Back, Stop): set by the service. */
    var onEnd: (() -> Unit)? = null
    fun end() = onEnd?.invoke()

    /** Whether the desktop is on screen: told to the PC with every ping
     * (and at once), which captures and encodes nothing while it is not. */
    @Volatile var visible = true  // being opened: not "hidden" before it ever showed
        private set
    /** Told when the desktop goes on or off screen (the service's Wi-Fi lock). */
    var onVisible: ((Boolean) -> Unit)? = null
    fun show(visible: Boolean) {
        this.visible = visible
        ping()
        onVisible?.invoke(visible)
    }

    private fun ping() = send(ByteBuffer.allocate(10).put(PING).putLong(System.nanoTime())
        .put(if (visible) 1 else 0).array())

    // Frames being put together, by number.
    private class Frame(val count: Int, val keyframe: Boolean, val captured: Long) {
        val parts = arrayOfNulls<ByteArray>(count)
        var have = 0
        val first = System.nanoTime()  // its first packet came
    }
    private val frames = HashMap<Int, Frame>()
    private var newest = -1          // the newest frame given to the decoder
    @Volatile private var needKeyframe = true  // until one comes, the others cannot be decoded
    private var askedAt = 0L

    // Measured: when each frame was captured and its first packet came, until it is decoded.
    // Under its own lock: the decoder's callbacks must never wait for
    // attach() or detach(), which wait for the decoder.
    private val timing = HashMap<Long, LongArray>()
    private val captureToDecoded = ArrayList<Int>()  // tenths of a millisecond
    private val firstToDecoded = ArrayList<Int>()
    private var shown = 0
    private var lost = 0
    private var offset = 0L          // PC clock - ours
    private var bestRtt = Long.MAX_VALUE
    private var offsetAt = 0L

    private val receiver = Thread(::receive, "plusone-display-in")
    private val ticker = Thread(::tick, "plusone-display-tick")

    fun start() {
        receiver.start()
        ticker.start()
    }

    fun stop() {
        running = false
        outbox.shutdownNow()
        packets.close()
        detach()
        callbacks.quitSafely()
    }

    /** The screen to show it on (DisplayActivity's surface). */
    @Synchronized
    fun attach(surface: Surface) {
        detach()
        this.surface = surface
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, MAX_WIDTH, MAX_HEIGHT).apply {
            setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            setInteger(MediaFormat.KEY_PRIORITY, 0)  // real time
        }
        val decoder = try {
            MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        } catch (e: Exception) {
            // None free (another app holds them all): nothing shown, rather
            // than the process gone, the service and its other sessions with it.
            Log.w(CompanionService.TAG, "display: no decoder: $e")
            return
        }
        decoder.setCallback(object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {
                free.add(index)
            }

            override fun onOutputBufferAvailable(codec: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
                decoded(info.presentationTimeUs, System.nanoTime())
                try {
                    codec.releaseOutputBuffer(index, true)  // now: no waiting for a presentation time
                } catch (e: IllegalStateException) {
                }
            }

            override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
                Log.w(CompanionService.TAG, "display: decoder: $e (transient: ${e.isTransient})")
                needKeyframe = true
                // Past a transient error the decoder takes no more input: a
                // new one (on the main thread: these callbacks never wait for it).
                if (!e.isTransient) main.post { restart(codec) }
            }

            override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {
                Log.i(CompanionService.TAG, "display: decoding $format")
            }
        }, handler)
        try {
            decoder.configure(format, surface, null, 0)
            decoder.start()
        } catch (e: Exception) {
            // (Exynos' refused a KEY_OPERATING_RATE, for one.) Nothing shown,
            // rather than the activity gone.
            Log.w(CompanionService.TAG, "display: no decoder: $e")
            decoder.release()
            return
        }
        Log.i(CompanionService.TAG, "display: decoder ${decoder.name}")
        codec = decoder
        // The next frame that is not a keyframe asks for one (from the
        // receiving thread: not the network on this, the main one).
        needKeyframe = true
        askedAt = 0L
    }

    /** A decoder in place of failed, if it is still the one: a few times in
     * a row at most (one that fails at once would fail forever). */
    @Synchronized
    private fun restart(failed: MediaCodec) {
        val surface = surface
        if (codec !== failed || surface == null || !running) return
        if (++restarts > MAX_RESTARTS) {
            Log.w(CompanionService.TAG, "display: the decoder keeps failing: none")
            detach()
            return
        }
        attach(surface)
    }

    @Synchronized
    fun detach() {
        surface = null
        val decoder = codec ?: return
        codec = null
        free.clear()
        try {
            decoder.stop()
        } catch (e: IllegalStateException) {
        }
        decoder.release()
    }

    private fun receive() {
        while (running) {
            val body = try {
                packets.receive()
            } catch (e: Exception) {
                if (running) Log.w(CompanionService.TAG, "display: $e")
                break
            }
            val b = ByteBuffer.wrap(body).order(ByteOrder.BIG_ENDIAN)
            when (b.get()) {
                VIDEO -> video(b)
                PONG -> pong(b.long, b.long)
            }
        }
    }

    @Synchronized
    private fun video(b: ByteBuffer) {
        val keyframe = b.get().toInt() and 1 != 0
        val number = b.int
        val index = b.short.toInt() and 0xffff
        val count = b.short.toInt() and 0xffff
        val captured = b.long
        if (number <= newest || count == 0 || index >= count) return  // old, or replayed
        val frame = frames.getOrPut(number) { Frame(count, keyframe, captured) }
        if (frame.count != count || frame.parts[index] != null) return
        frame.parts[index] = ByteArray(b.remaining()).also { b.get(it) }
        frame.have++
        if (frame.have < frame.count) return
        // Whole: the frames before it that are not are lost.
        val older = frames.keys.filter { it < number }
        if (older.isNotEmpty()) {
            synchronized(timing) { lost += older.size }
            older.forEach { frames.remove(it) }
            needKeyframe = true
        }
        frames.remove(number)
        newest = number
        if (codec == null) {
            // Nothing to decode into (no surface, no decoder): no keyframes
            // asked for meanwhile; attach() asks at the next frame.
            needKeyframe = true
            return
        }
        if (needKeyframe && !frame.keyframe) {
            askKeyframe()
            return
        }
        if (decode(frame, number)) needKeyframe = false else {
            synchronized(timing) { lost++ }
            needKeyframe = true
            askKeyframe()
        }
    }

    /** The frame to the decoder, if it has a buffer free now. */
    private fun decode(frame: Frame, number: Int): Boolean {
        val decoder = codec ?: return false
        val index = free.poll() ?: return false
        var queued = false
        return try {
            val input = decoder.getInputBuffer(index) ?: return false
            input.clear()
            for (part in frame.parts) input.put(part!!)
            synchronized(timing) { timing[number.toLong()] = longArrayOf(frame.captured, frame.first) }
            decoder.queueInputBuffer(index, 0, input.position(), number.toLong(), 0)
            queued = true
            true
        } catch (e: Exception) {
            Log.w(CompanionService.TAG, "display: $e")
            false
        } finally {
            if (!queued) free.add(index)  // still the decoder's to fill: not lost
        }
    }

    private fun decoded(frame: Long, nanos: Long) = synchronized(timing) {
        val (captured, first) = timing.remove(frame) ?: return
        shown++
        restarts = 0
        if (offsetAt != 0L) captureToDecoded += ((nanos + offset - captured) / 100_000).toInt()
        firstToDecoded += ((nanos - first) / 100_000).toInt()
        // Frames that never come out (dropped by the decoder) do not pile up.
        if (timing.size > 64) timing.keys.filter { it < frame }.forEach { timing.remove(it) }
    }

    private fun askKeyframe() {
        val now = System.nanoTime()
        if (now - askedAt < KEYFRAME_AGAIN) return
        askedAt = now
        Log.i(CompanionService.TAG, "display: asking for a keyframe (decoder ${codec != null}, ${free.size} buffers free)")
        send(byteArrayOf(KEYFRAME))
    }

    private fun pong(ours: Long, theirs: Long) = synchronized(timing) {
        val now = System.nanoTime()
        val rtt = now - ours
        // The ping that came back soonest tells the offset best; it ages out.
        if (rtt < bestRtt || now - offsetAt > OFFSET_AGE) {
            bestRtt = rtt
            offset = theirs - (ours + now) / 2
            offsetAt = now
        }
    }

    /** Every second: a ping for the clocks, and what was shown to the PC.
     * Hidden, a ping now and then only (it says hidden, should the one
     * show(false) sent be lost): nothing to tell, the radio left to rest. */
    private fun tick() {
        var ticks = 0
        while (running) {
            if (visible) {
                ping()
                stats()
            } else if (ticks % HIDDEN_PING_EVERY == 0) {
                ping()
            }
            ticks++
            try {
                Thread.sleep(1000)
            } catch (e: InterruptedException) {
                return
            }
        }
    }

    /** STATS: shown u32 | lost u32 | for capture-to-decoded, then from its
     * first packet to decoded: median, 90th percentile and highest, in
     * tenths of a millisecond (u16). */
    private fun stats() = synchronized(timing) {
        val b = ByteBuffer.allocate(1 + 8 + 12).put(STATS).putInt(shown).putInt(lost)
        for (samples in listOf(captureToDecoded, firstToDecoded)) {
            samples.sort()
            for (q in listOf(0.5, 0.9, 1.0)) {
                val v = if (samples.isEmpty()) 0 else samples[((samples.size - 1) * q).toInt()]
                b.putShort(v.coerceIn(0, 0xffff).toShort())
            }
            samples.clear()
        }
        shown = 0
        lost = 0
        send(b.array())
    }

    /** body to the PC, sealed, from the outbox's thread. */
    fun send(body: ByteArray) {
        try {
            outbox.execute {
                try {
                    packets.send(body)
                } catch (e: Exception) {
                    if (running) Log.w(CompanionService.TAG, "display: cannot send: $e")
                }
            }
        } catch (e: java.util.concurrent.RejectedExecutionException) {
            // stopped
        }
    }

    companion object {
        const val VIDEO: Byte = 1
        const val KEYFRAME: Byte = 2
        const val PING: Byte = 3
        const val PONG: Byte = 4
        const val STATS: Byte = 5
        const val INPUT: Byte = 6
        const val MAX_WIDTH = 3840
        const val MAX_HEIGHT = 2400
        const val KEYFRAME_AGAIN = 250_000_000L  // ns
        const val OFFSET_AGE = 10_000_000_000L   // ns
        const val MAX_RESTARTS = 3
        const val HIDDEN_PING_EVERY = 10        // ticks (seconds)

        /** The session being shown, for DisplayActivity. */
        @Volatile var current: DisplaySession? = null
    }
}

/** The display notification's Stop. */
class DisplayStopReceiver : android.content.BroadcastReceiver() {
    override fun onReceive(context: android.content.Context, intent: android.content.Intent) {
        DisplaySession.current?.end()
    }
}
