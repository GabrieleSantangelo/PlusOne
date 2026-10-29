package io.github.gabrielesantangelo.plusone

import io.github.gabrielesantangelo.plusone.link.Link
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.IOException
import java.io.OutputStream
import java.net.InetAddress
import java.util.concurrent.CountDownLatch
import java.util.concurrent.LinkedBlockingQueue

/** PDFs to sign as the PC uploads them (SignHandler): the queue keeps each one's own. */
class SignHandlerTest {
    /** A PC's link uploading a PDF of fill bytes: half of it, then (once
     * gate opens) the rest. */
    private class Upload(private val fill: Byte, private val half: CountDownLatch? = null,
                         private val gate: CountDownLatch? = null) : Link("PC", "fp") {
        val sent = LinkedBlockingQueue<JSONObject>()
        override val address: InetAddress get() = InetAddress.getLoopbackAddress()
        override fun send(message: JSONObject) { sent.add(message) }
        override fun receive(maxHeader: Int): JSONObject = throw IOException("none")
        override fun readPayload(size: Long, out: OutputStream) {
            val first = (size / 2).toInt()
            out.write(ByteArray(first) { fill })
            out.flush()
            half?.countDown()
            gate?.await()
            out.write(ByteArray((size - first).toInt()) { fill })
            out.flush()
        }
        override fun hold(silentMs: Int) {}
        override fun close() {}
    }

    // Off the device the service's cacheDir is null: its "jobs" are here.
    private val handler = SignHandler(CompanionService())
    private val id = "%032x".format(System.nanoTime())
    private val request = JSONObject().put("type", "sign").put("job", id).put("title", "A PDF")

    @After
    fun forget() {
        Jobs.finish(id, null)
        Jobs.acknowledge(id, "fp")
        java.io.File("jobs", "$id.pdf").delete()
        java.io.File("jobs").delete()  // if nothing else is in it
    }

    @Test
    fun anUploadOfAJobQueuedAlreadyLeavesItsPdfAlone() {
        handler.handle(Upload('A'.code.toByte()), request, 1000)
        assertThrows(IllegalArgumentException::class.java) { handler.handle(Upload('B'.code.toByte()), request, 1000) }
        assertEquals("A".repeat(1000), Jobs.get(id)!!.file.readText())
    }

    @Test(timeout = 20_000)
    fun twoUploadsOfOneJobAtOnceLeaveTheQueuedOneItsPdf() {
        val half = CountDownLatch(1)
        val gate = CountDownLatch(1)
        val failed = LinkedBlockingQueue<Throwable>()
        val first = Thread {
            try {
                handler.handle(Upload('A'.code.toByte(), half, gate), request, 1000)
            } catch (e: Throwable) {
                failed.add(e)
            }
        }.apply { start() }
        half.await()
        // The PC sends it again (it timed out waiting, say) while the first is halfway.
        try {
            handler.handle(Upload('B'.code.toByte()), request, 1000)
        } catch (e: IllegalArgumentException) {
            failed.add(e)
        }
        gate.countDown()
        first.join()
        val job = Jobs.get(id)!!
        assertTrue("the queued job's PDF is gone", job.file.exists())
        assertEquals("A".repeat(1000), job.file.readText())
        assertEquals(1, failed.size)  // the second one, refused
    }
}
