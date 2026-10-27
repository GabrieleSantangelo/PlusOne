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
 * is the last one backwards, fading out (one, then silence), keeping time;
 * more than MAX_HOLE missing, the sound is taken up where it is now.
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
    private var arrived = 0L       // uptime ms the last packet came (under the lock)
    // What came at once after nothing for STALL_MS: its start, and how many in BURST_MS (under the lock).
    private var burstAt = 0L
    private var burst = 0
    private var underrunsSeen = 0  // the track's underruns the player knows of
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
    private val datagrams = try {
        Datagrams(key, pc, pcPort, receiveBuffer = 1 shl 20)
    } catch (e: Exception) {
        track.release()
        throw e
    }
    /** This end's port, for the PC. */
    val port get() = datagrams.port
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
                } else {
                    // Full (the Wi-Fi held back more than a second): the oldest
                    // go, they are the most out of date.
                    if (packets.size >= MAX_HELD) {
                        packets.remove(oldest())
                        late++
                    }
                    packets[number] = body.copyOfRange(5, body.size)
                    val now = SystemClock.uptimeMillis()
                    if (now - arrived >= STALL_MS) {
                        burstAt = now
                        burst = 0
                    }
                    if (now - burstAt <= BURST_MS) burst++
                    arrived = now
                    lock.notifyAll()
                }
            }
        }
    }

    private fun play() {
        var last = ByteArray(bytes)
        var played = 0
        var fadeIn = false     // the next packet starts from silence (after any gap)
        var concealed = false  // what was written last stood in for a lost packet
        var over = 0           // packets in a row the wait has been far over the cushion
        var refilling = false  // the track ran dry: the wait fills up again before it plays
        var skip = false       // what a stall held back is late: to be left out
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
                    underrunsSeen = track.underrunCount
                    track.play()
                }
                val underruns = track.underrunCount
                if (underruns > underrunsSeen) {
                    // A gap was heard (the track ran dry): it starts again from
                    // a full wait, not from the next packet (the next
                    // unevenness of the Wi-Fi would be another gap).
                    underrunsSeen = underruns
                    refilling = true
                    fadeIn = true
                }
                val enough = if (refilling) aim else 1
                if (packets.size < enough) {
                    // Nothing yet, the Wi-Fi slow: the track still plays what it
                    // holds, so nothing is written meanwhile. (It was flushed
                    // after 30 ms: what it held, thrown away, was the sound
                    // that jumped.)
                    lock.wait(WAIT_MS)
                    if (packets.size < enough && SystemClock.uptimeMillis() - arrived >= STOPPED_MS) {
                        // The PC stopped (a player paused, its silence not sent):
                        // rest, and start from a full wait when it comes back.
                        // What little came is old by then; the track played the rest.
                        late += packets.size
                        packets.clear()
                        refilling = false
                        started = false
                        track.pause()
                    }
                    return@synchronized null
                }
                if (refilling) {
                    refilling = false
                    val first = oldest()
                    if (first - next > 0) missed += first - next  // lost meanwhile
                    next = first
                    held = packets.size.toDouble()
                }
                if (stalled()) skip = true
                if (skip && fadeIn) {
                    // At a gap (the sound starts from silence): left out as is.
                    skipTo(target)
                    skip = false
                }
                var got = packets.remove(next)
                if (got == null) {
                    // Missing, and later ones here: stood in for (keeping time,
                    // so the wait stays as it is), unless so many are missing
                    // that the sound is better taken up where it is now.
                    val first = oldest()
                    if (first - next > MAX_HOLE) {
                        missed += first - next
                        next = first
                        got = packets.remove(next)
                        fadeIn = true
                    }
                }
                next++
                held = held * 0.99 + packets.size * 0.01
                over = if (packets.size > minOf(target + OVER, MAX_TARGET + SLACK)) over + 1 else 0
                if (got != null && (skip || over >= OVER_FOR)) {
                    // What the Wi-Fi held back is late (or far over the cushion
                    // for a second, no stall seen): back to the cushion now,
                    // this packet faded out, the next faded in.
                    over = 0
                    skip = false
                    skipTo(target)
                    fadeIn = true
                    concealed = true  // faded out to silence: a packet lost next stays silent
                    return@synchronized fadeOut(got)
                }
                if (got != null && held > aim + SLACK && packets.size > aim && quiet(got)) {
                    dropped++
                    return@synchronized null  // the wait too long: a silence a packet shorter
                }
                when {
                    got == null -> (if (concealed) ByteArray(bytes) else conceal(last))
                        .also { missed++; fadeIn = true; concealed = true }
                    fadeIn -> rampUp(got).also { fadeIn = false; concealed = false }
                    else -> got.also { concealed = false }
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

    /** Once what came at once after a stall is all in (under the lock): the
     * stall was as long as it held back. The cushion grows to bridge the
     * next one like it; whether more waits than that, which is late now. */
    private fun stalled(): Boolean {
        if (burst <= BURST_PACKETS || SystemClock.uptimeMillis() - burstAt <= BURST_MS) return false
        target = maxOf(target, minOf(MAX_TARGET, burst + SLACK))
        aim = target
        lastStall = SystemClock.uptimeMillis()
        burst = 0
        return packets.size > target + SLACK
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

    /** A packet fading out, to silence. */
    private fun fadeOut(packet: ByteArray): ByteArray {
        val out = ByteArray(bytes)
        val samples = ByteBuffer.wrap(packet).order(java.nio.ByteOrder.LITTLE_ENDIAN).asShortBuffer()
        val faded = ByteBuffer.wrap(out).order(java.nio.ByteOrder.LITTLE_ENDIAN).asShortBuffer()
        val n = samples.limit()
        for (i in 0 until n) faded.put(i, (samples.get(i) * (n - i) / n).toShort())
        return out
    }

    /** For a lost packet: the last one backwards, fading out. Backwards, it
     * goes on from where the last one ended (no click), into silence. */
    private fun conceal(last: ByteArray): ByteArray {
        val out = ByteArray(bytes)
        val samples = ByteBuffer.wrap(last).order(java.nio.ByteOrder.LITTLE_ENDIAN).asShortBuffer()
        val faded = ByteBuffer.wrap(out).order(java.nio.ByteOrder.LITTLE_ENDIAN).asShortBuffer()
        for (f in 0 until frames) {
            val from = frames - 1 - f
            for (c in 0 until CHANNELS) {
                faded.put(f * CHANNELS + c, (samples.get(from * CHANNELS + c) * (frames - f) / frames).toShort())
            }
        }
        return out
    }

    companion object {
        const val PCM: Byte = 7
        const val STATS: Byte = 5
        const val CHANNELS = 2
        const val PREFILL = 8         // packets of 5 ms: 40 ms
        const val MAX_HELD = 200      // a second: more is a stuck clock, not jitter
        const val WAIT_MS = 30L
        const val STALL_MS = 30L      // nothing came for this long, then...
        const val BURST_MS = 20L      // ...in this long...
        const val BURST_PACKETS = 12  // ...far more than come in it anyway (the PC sends a few at a time): held back
        const val MAX_HOLE = 40       // packets missing that are stood in for (200 ms); more: a jump
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
