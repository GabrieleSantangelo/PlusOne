package io.github.gabrielesantangelo.plusone

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
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
 * hears, so the wait neither grows nor runs dry. The Wi-Fi stalling, then
 * handing over what it held at once, grows the wait by as much, which that
 * trim took a minute to take back (the sound later and later behind the
 * picture): over the wait, a packet nearly silent is left out (nobody hears
 * a silence 5 ms shorter), and far over it the rate goes up to RATE_FAST
 * for a few seconds. A packet that does not come is the last one again,
 * fading out.
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
    private var quietFor = 0L    // ms with nothing to play
    private var held = PREFILL.toDouble()  // packets waiting, smoothed
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
        while (running) {
            val data: ByteArray = synchronized(lock) {
                if (!started) {
                    // The first packets: wait for PREFILL of them, from the oldest.
                    while (running && packets.size < PREFILL) lock.wait()  // stop() wakes it too
                    if (!running) return
                    next = packets.keys.minWithOrNull { a, b -> a - b }!!
                    started = true
                    track.play()
                }
                var got = packets.remove(next)
                if (got == null && packets.isEmpty()) {
                    // Nothing yet, the Wi-Fi slow: the track still plays what it
                    // holds (tens of ms), so nothing is written meanwhile. (It
                    // was flushed after 30 ms: what it held, thrown away, was
                    // the sound that jumped.)
                    lock.wait(WAIT_MS)
                    got = packets.remove(next)
                }
                if (got == null && packets.isEmpty()) {
                    quietFor += WAIT_MS
                    if (quietFor >= STOPPED_MS) {
                        // The PC stopped (a player paused, its silence not
                        // sent): rest, and start from a full wait when it comes
                        // back. Nothing thrown away: the track has played it all.
                        quietFor = 0
                        started = false
                        track.pause()
                    }
                    return@synchronized null
                }
                quietFor = 0
                next++
                held = held * 0.99 + packets.size * 0.01
                if (got != null && held > PREFILL + SLACK && packets.size > PREFILL && quiet(got)) {
                    dropped++
                    return@synchronized null  // the wait too long: a silence a packet shorter
                }
                got ?: fade(last).also { missed++ }
            } ?: continue
            last = data
            track.write(data, 0, data.size)
            written += frames
            if (++played % TRIM_EVERY == 0) trim()
        }
    }

    /** The playback rate, a hair faster while more than PREFILL waits, slower while less. */
    private fun trim() {
        val error = (held - PREFILL) / PREFILL
        val most = if (held > 2 * PREFILL) RATE_FAST else RATE_TRIM
        val factor = 1.0 + (error * RATE_TRIM * 2).coerceIn(-RATE_TRIM, most)
        track.playbackRate = (rate * factor).toInt()
        // STATS: frames waiting here | missed | late | left out | frames from
        // here to the speaker (waiting, and in the track not yet played).
        val report = ByteBuffer.allocate(21).put(STATS).putInt((held * frames).toInt()).putInt(missed)
            .putInt(late).putInt(dropped).putInt((held * frames).toInt() + inTrack()).array()
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
        const val STOPPED_MS = 300L   // nothing for this long: the PC stopped
        const val TRIM_EVERY = 200    // packets: once a second
        const val RATE_TRIM = 0.002   // at most 0.2 %
        const val RATE_FAST = 0.01    // far over the wait (twice PREFILL): 1 %, for seconds
        const val SLACK = 4           // packets over PREFILL before quiet ones are left out
        const val QUIET = 330         // of 32767: -40 dBFS
    }
}
