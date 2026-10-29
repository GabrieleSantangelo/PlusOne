package io.github.gabrielesantangelo.plusone

import io.github.gabrielesantangelo.plusone.link.Packets
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.IOException
import java.nio.ByteBuffer
import java.util.concurrent.CountDownLatch
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/** What the PC sends, however it is made: the session reads on, its memory bounded. */
class DisplaySessionTest {
    /** bodies, then nothing; read: the receiving thread asked for the one after them all. */
    private class FromPc(bodies: List<ByteArray>) : Packets {
        private val queue = LinkedBlockingQueue(bodies)
        val read = CountDownLatch(bodies.size + 1)
        override fun send(body: ByteArray) {}
        override fun receive(): ByteArray {
            read.countDown()
            return queue.poll(2, TimeUnit.SECONDS) ?: throw IOException("closed")
        }
        override fun close() {}
    }

    private val uncaught = LinkedBlockingQueue<Throwable>()
    private val handler = Thread.getDefaultUncaughtExceptionHandler()
    private var session: DisplaySession? = null

    init {
        Thread.setDefaultUncaughtExceptionHandler { _, e -> uncaught.add(e) }
    }

    @After
    fun restore() {
        session?.stop()
        Thread.setDefaultUncaughtExceptionHandler(handler)
    }

    private fun video(number: Int, index: Int, count: Int, h264: Int = 1): ByteArray =
        ByteBuffer.allocate(1 + DisplaySession.VIDEO_HEADER + h264).put(DisplaySession.VIDEO).put(0).putInt(number)
            .putShort(index.toShort()).putShort(count.toShort()).putLong(0).array()

    /** The frames being put together once the session has read all of bodies. */
    private fun waitingAfter(bodies: List<ByteArray>): Int {
        val packets = FromPc(bodies)
        val session = DisplaySession(packets).also { session = it }
        session.start()
        assertTrue("the receiving thread ended: ${uncaught.peek()}", packets.read.await(20, TimeUnit.SECONDS))
        assertNull(uncaught.peek())
        val frames = DisplaySession::class.java.getDeclaredField("frames").apply { isAccessible = true }
        return synchronized(session) { (frames.get(session) as Map<*, *>).size }
    }

    @Test
    fun packetsTooShortForTheirKindAreDroppedAndTheNextOnesRead() {
        val short = listOf(
            byteArrayOf(),
            byteArrayOf(DisplaySession.PONG),
            ByteArray(1 + DisplaySession.PONG_SIZE - 1).also { it[0] = DisplaySession.PONG },
            byteArrayOf(DisplaySession.VIDEO, 1, 0, 0),
            video(1, 0, 2, h264 = 0),
            byteArrayOf(DisplaySession.KEYFRAME),
        )
        // Then a frame whole: put together and given on, nothing left waiting.
        assertEquals(0, waitingAfter(short + video(2, 0, 2) + video(2, 1, 2)))
    }

    @Test
    fun framesNeverWholeAreKeptOnlyTheNewestFew() {
        val never = (1..1000).map { video(it, 0, 2) }
        val tooMany = video(1001, 0, DisplaySession.MAX_PARTS + 1)
        assertEquals(DisplaySession.MAX_FRAMES, waitingAfter(never + tooMany))
    }

    @Test
    fun aFrameWholeAfterOnesGivenUpIsStillPutTogether() {
        val never = (1..1000).map { video(it, 0, 2) }
        assertEquals(0, waitingAfter(never + video(1001, 0, 1)))
    }
}
