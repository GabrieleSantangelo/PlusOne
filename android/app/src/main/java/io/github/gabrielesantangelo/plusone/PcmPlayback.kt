package io.github.gabrielesantangelo.plusone

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.os.SystemClock
import android.util.Log
import java.net.InetAddress
import java.nio.ByteBuffer
import io.github.gabrielesantangelo.plusone.link.Datagrams

/**
 * The PC's sound on the speakers as it is (plusone/pcm.py): 16-bit stereo PCM
 * at 48 kHz, no codec, in sealed packets of 5 ms, played with a plain media
 * AudioTrack, as the tablet's own players do. (libwebrtc played it before:
 * made for calls, its jitter buffer stretched and squeezed the music.)
 *
 * PREFILL packets wait before it starts, against the Wi-Fi's unevenness. The
 * two clocks drift apart (the PC's and the tablet's sound chip, tens of parts
 * per million): the playback rate follows by at most RATE_TRIM, which no ear
 * hears, so the wait neither grows nor runs dry. A packet that does not come
 * is the last one again, fading out.
 *
 * No gap heard is worth more than a few ms less. The Wi-Fi stalling, then
 * handing over what it held at once, grew the wait by as much, every time
 * (up to a second behind, measured on a weak 2.4 GHz network). Now a stall
 * makes the wait a cushion as long as what it held back, and a little more
 * (target, up to MAX_TARGET), so the next one like it is bridged (measured:
 * a 150 ms stall after a 200 ms one, no gap); only what came later than
 * that is left out, as the sound starts again (the track ran dry: the gap
 * was heard already, nothing more is). After a gap the sound starts again
 * with the wait full, not with the first packet back. Once the Wi-Fi is calm the wait
 * goes back to PREFILL where nobody hears it: a packet nearly silent left
 * out (a silence 5 ms shorter), the rate up to RATE_FAST. Only far over the
 * cushion for a whole second is the excess skipped, faded out and in. After
 * any gap (the track played out, a packet lost) the sound fades in: no click.
 */
class PcmPlayback(key: ByteArray, pc: InetAddress, pcPort: Int,
                  private val rate: Int, private val frames: Int) {
    private val datagrams = Datagrams(key, pc, pcPort, receiveBuffer = 1 shl 20)
    /** This end's port, for the PC. */
    val port get() = datagrams.port
    private val bytes = frames * CHANNELS * 2
    private val packets = HashMap<Int, ByteArray>()  // by number, under the lock
    private val lock = Object()
    @Volatile private var running = true
    private var next = 0          // the next to play
    private var started = false
    private var missed = 0
    private var late = 0
    private var dropped = 0      // nearly silent packets left out, the wait too long
    private var written = 0L     // frames given to the track since it started
    private var quietSince = 0L  // uptime ms since nothing was there to play (0: something was)
    private var held = PREFILL.toDouble()  // packets waiting, smoothed
    // The player's thread's: the cushion a stall asked for, and what the wait is brought to.
    private var target = PREFILL
    private var aim = PREFILL
    private var underrunsBefore = 0  // the track's count at the last report
    private var lastStall = -CALM_MS  // uptime ms of the last stall that held packets back
    private val track = AudioTrack.Builder()
        .setAudioAttributes(AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_MEDIA)
            .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
            .build())
        .setAudioFormat(AudioFormat.Builder()
            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
            .setSampleRate(rate)
            .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
            .build())
        .setTransferMode(AudioTrack.MODE_STREAM)
        .setBufferSizeInBytes(maxOf(AudioTrack.getMinBufferSize(rate, AudioFormat.CHANNEL_OUT_STEREO,
            AudioFormat.ENCODING_PCM_16BIT), bytes * 4))
        .build()
    private val receiver = Thread(::receive, "plusone-speakers-in")
    private val player = Thread(::play, "plusone-speakers-out")

    fun start() {
        receiver.start()
        player.start()
    }

    fun stop() {
        running = false
        datagrams.close()
        synchronized(lock) { lock.notifyAll() }
        player.join(1000)
        try {
            track.stop()
        } catch (e: IllegalStateException) {
        }
        track.release()
    }

    private fun receive() {
        while (running) {
            val body = try {
                datagrams.receive()
            } catch (e: Exception) {
                break
            }
            if (body.size != 5 + bytes || body[0] != PCM) continue
            val number = ByteBuffer.wrap(body, 1, 4).int
            synchronized(lock) {
                if (started && number - next < 0) {
                    late++
                } else if (packets.size < MAX_HELD) {
                    packets[number] = body.copyOfRange(5, body.size)
                    lock.notifyAll()
                }
            }
        }
    }

    private fun play() {
        var last = ByteArray(bytes)
        var played = 0
        var fadeIn = false  // the next packet starts from silence (after any gap)
        var over = 0        // packets in a row the wait has been far over the cushion
        while (running) {
            val data: ByteArray = synchronized(lock) {
                if (!started) {
                    // The first packets: wait for PREFILL of them, from the oldest.
                    while (running && packets.size < PREFILL) lock.wait()  // stop() wakes it too
                    if (!running) return
                    next = oldest()
                    held = packets.size.toDouble()
                    started = true
                    fadeIn = true
                    track.play()
                }
                // Once the track played out, it starts again from a full wait,
                // not from the first packet back (the next unevenness of the
                // Wi-Fi would be another gap).
                val enough = if (quietSince != 0L) aim else 1
                if (packets.size < enough) {
                    // Nothing yet, the Wi-Fi slow: the track still plays what it
                    // holds (tens of ms), so nothing is written meanwhile. (It
                    // was flushed after 30 ms: what it held, thrown away, was
                    // the sound that jumped.)
                    lock.wait(WAIT_MS)
                    if (packets.size < enough) {
                        val now = SystemClock.uptimeMillis()
                        if (quietSince == 0L) {
                            quietSince = now  // WAIT_MS with nothing: the track plays out
                        } else if (now - quietSince >= STOPPED_MS) {
                            // The PC stopped (a player paused, its silence not
                            // sent): rest, and start from a full wait when it comes
                            // back. Nothing thrown away: the track has played it all.
                            quietSince = 0
                            started = false
                            track.pause()
                        }
                        return@synchronized null
                    }
                }
                if (quietSince != 0L) {
                    // What the Wi-Fi held comes at once, but a packet at a
                    // time: all of it in, before saying how much it was.
                    val until = SystemClock.uptimeMillis() + BURST_MS
                    while (running && SystemClock.uptimeMillis() < until) lock.wait(maxOf(1, until - SystemClock.uptimeMillis()))
                    quietSince = 0
                    stalled()
                    val first = oldest()
                    if (first - next > 0) missed += first - next  // lost meanwhile
                    next = first
                    held = packets.size.toDouble()
                    fadeIn = true
                }
                val got = packets.remove(next)
                next++
                held = held * 0.99 + packets.size * 0.01
                over = if (packets.size > target + OVER) over + 1 else 0
                if (got != null && over >= OVER_FOR) {
                    // Far over the cushion for a second, no stall seen: back to it now.
                    over = 0
                    skipTo(target)
                    fadeIn = true
                    return@synchronized fade(got)
                }
                if (got != null && held > aim + SLACK && packets.size > aim && quiet(got)) {
                    dropped++
                    return@synchronized null  // the wait too long: a silence a packet shorter
                }
                when {
                    got == null -> fade(last).also { missed++; fadeIn = true }
                    fadeIn -> rampUp(got).also { fadeIn = false }
                    else -> got
                }
            } ?: continue
            last = data
            track.write(data, 0, data.size)
            written += frames
            if (++played % TRIM_EVERY == 0) trim()
        }
    }

    /** The oldest packet's number (under the lock, with some there). */
    private fun oldest() = packets.keys.minWithOrNull { a, b -> a - b }!!

    /** After the track played out for want of packets (under the lock): if
     * more came than the wait aims at, the Wi-Fi held them back. The
     * cushion grows to bridge the next such stall; what came later than it
     * can is late. */
    private fun stalled() {
        if (packets.size <= aim) return  // nothing held back: the PC was silent, say
        // As long as what was held back, a little more for the Wi-Fi's unevenness.
        target = maxOf(target, minOf(MAX_TARGET, packets.size + SLACK))
        aim = target
        lastStall = SystemClock.uptimeMillis()
        skipTo(target)
    }

    /** The oldest packets left out (counted late), but the newest n (under the lock). */
    private fun skipTo(n: Int) {
        val numbers = packets.keys.sortedWith { a, b -> a - b }
        val gone = numbers.size - n
        if (gone <= 0) return
        for (number in numbers.subList(0, gone)) packets.remove(number)
        late += gone
        next = numbers[gone]
        held = n.toDouble()
    }

    /** The playback rate, a hair faster while more than the aim waits,
     * slower while less. Once the Wi-Fi is calm the aim is PREFILL again,
     * and the cushion follows the wait down as it gets there. */
    private fun trim() {
        if (SystemClock.uptimeMillis() - lastStall > CALM_MS) {
            target = maxOf(PREFILL, minOf(target, kotlin.math.ceil(held).toInt()))
            aim = PREFILL
        } else {
            aim = target
        }
        val error = (held - aim) / aim
        val most = if (held > 2 * aim) RATE_FAST else RATE_TRIM
        val factor = 1.0 + (error * RATE_TRIM * 2).coerceIn(-RATE_TRIM, most)
        track.playbackRate = (rate * factor).toInt()
        // STATS: frames waiting here | missed | late | left out | frames from
        // here to the speaker (waiting, and in the track not yet played) |
        // gaps heard (the track ran dry).
        val underruns = track.underrunCount
        val report = ByteBuffer.allocate(25).put(STATS).putInt((held * frames).toInt()).putInt(missed)
            .putInt(late).putInt(dropped).putInt((held * frames).toInt() + inTrack())
            .putInt(underruns - underrunsBefore).array()
        underrunsBefore = underruns
        missed = 0
        late = 0
        dropped = 0
        try {
            datagrams.send(report)
        } catch (e: Exception) {
            if (running) Log.w(CompanionService.TAG, "speakers: cannot report: $e")
        }
    }

    /** Frames written to the track and not played yet (its buffer, the
     * mixer's, the speaker's), from where Android says it is; 0 unknown. */
    private fun inTrack(): Int {
        val stamp = android.media.AudioTimestamp()
        if (!track.getTimestamp(stamp)) return 0
        val played = stamp.framePosition + (System.nanoTime() - stamp.nanoTime) * rate / 1_000_000_000
        return (written - played).coerceIn(0, rate.toLong()).toInt()
    }

    /** Whether a packet is nearly silent: its loudest sample under QUIET. */
    private fun quiet(packet: ByteArray): Boolean {
        val samples = ByteBuffer.wrap(packet).order(java.nio.ByteOrder.LITTLE_ENDIAN).asShortBuffer()
        for (i in 0 until samples.limit()) if (kotlin.math.abs(samples.get(i).toInt()) > QUIET) return false
        return true
    }

    /** A packet fading in, from silence. */
    private fun rampUp(packet: ByteArray): ByteArray {
        val out = ByteArray(bytes)
        val samples = ByteBuffer.wrap(packet).order(java.nio.ByteOrder.LITTLE_ENDIAN).asShortBuffer()
        val faded = ByteBuffer.wrap(out).order(java.nio.ByteOrder.LITTLE_ENDIAN).asShortBuffer()
        val n = samples.limit()
        for (i in 0 until n) faded.put(i, (samples.get(i) * i / n).toShort())
        return out
    }

    /** The last packet again, fading out: a lost one is barely heard. */
    private fun fade(last: ByteArray): ByteArray {
        val out = ByteArray(bytes)
        val samples = ByteBuffer.wrap(last).order(java.nio.ByteOrder.LITTLE_ENDIAN).asShortBuffer()
        val faded = ByteBuffer.wrap(out).order(java.nio.ByteOrder.LITTLE_ENDIAN).asShortBuffer()
        val n = samples.limit()
        for (i in 0 until n) faded.put(i, (samples.get(i) * (n - i) / n).toShort())
        return out
    }

    companion object {
        const val PCM: Byte = 7
        const val STATS: Byte = 5
        const val CHANNELS = 2
        const val PREFILL = 8         // packets of 5 ms: 40 ms
        const val MAX_HELD = 200      // a second: more is a stuck clock, not jitter
        const val WAIT_MS = 30L
        const val BURST_MS = 20L      // for all of what the Wi-Fi held to come in, after it stalled
        const val STOPPED_MS = 300L   // nothing for this long more: the PC stopped
        const val TRIM_EVERY = 200    // packets: once a second
        const val RATE_TRIM = 0.002   // at most 0.2 %
        const val RATE_FAST = 0.01    // far over the aim (twice it): 1 %, for seconds
        const val SLACK = 4           // packets over the aim before quiet ones are left out
        const val MAX_TARGET = 40     // packets: the longest cushion, 200 ms
        const val CALM_MS = 10_000L   // no stall for this long: the wait comes back down
        const val OVER = 20           // packets over the cushion that are far over it...
        const val OVER_FOR = 200      // ...for this many packets (a second): skipped
        const val QUIET = 330         // of 32767: -40 dBFS
    }
}
