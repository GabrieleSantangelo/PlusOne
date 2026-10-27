package io.github.gabrielesantangelo.plusone

import android.os.Handler
import android.os.Looper
import org.json.JSONObject
import java.io.File
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CopyOnWriteArraySet
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicLong

/**
 * The PDFs the PC sent to sign: a queue, in the order they came. Five
 * PDFs sent at once wait here; the signing screen shows one, and the next
 * once it is done or cancelled.
 *
 * A job outlives the connection it came on: if the link drops while the
 * user signs, the PC connects again and asks for it ("resume"). Its answer
 * is kept until the PC says it got it ("got"): bytes handed to a link that
 * died on the way are not an answer delivered.
 */
object Jobs {
    class Job(val id: String, val file: File, val title: String, val pc: String, val pen: JSONObject?,
              /** The PDF is the job's (deleted with it), not one opened by hand. */
              val ownsFile: Boolean = true,
              /** The certificate fingerprint of the PC it came from: only that PC may ask for it again. */
              val peer: String = "",
              /** The PDF's SHA-256, as it came: the answer says it, so the PC
               * puts the marks on that PDF only. */
              val sha256: String = "",
              /** The PC shares its saved signatures (it sent them): ours go back if we share them too. */
              val sharing: Boolean = false) {
        val order = counter.incrementAndGet()
        var result: JSONObject? = null
        var finished = false
        /** Sends the job's answer to the PC; true if it went out. */
        var deliver: ((Job) -> Boolean)? = null
        /** The link it came or came back on; null when that went (lostAt). */
        var link: Any? = null
        var lostAt = 0L
        /** What was drawn on it while another document is shown, and on which page. */
        var draft: Map<Int, List<Mark>>? = null
        var draftPage = 0
    }

    private val counter = AtomicLong()
    private val jobs = ConcurrentHashMap<String, Job>()
    // Not on the UI thread; one thread per answer, so a PC that stops
    // reading does not hold back the others' answers.
    private val network = Executors.newCachedThreadPool()
    private val listeners = CopyOnWriteArraySet<() -> Unit>()
    private val main = Handler(Looper.getMainLooper())

    val ids: Set<String> get() = jobs.keys.toSet()

    /** Still to sign, in the order they came. */
    fun waiting(): List<Job> = jobs.values.filter { !it.finished }.sortedBy { it.order }

    fun get(id: String?): Job? = id?.let { jobs[it] }?.takeIf { !it.finished }

    /** Bytes of the PDFs the queue keeps. */
    fun bytes(): Long = jobs.values.filter { it.ownsFile }.sumOf { it.file.length() }

    /** listener runs on the main thread whenever the queue changes. */
    fun listen(listener: () -> Unit) = listeners.add(listener)
    fun unlisten(listener: () -> Unit) = listeners.remove(listener)
    private fun changed() = main.post { listeners.forEach { it() } }

    /** Adds job to the queue; false if one with its id is there already. */
    fun start(job: Job, link: Any?, deliver: (Job) -> Boolean): Boolean {
        job.link = link
        job.deliver = deliver
        if (jobs.putIfAbsent(job.id, job) != null) return false
        changed()
        return true
    }

    /** The signing screen's answer: the marks, or null if the user cancelled. */
    fun finish(id: String, result: JSONObject?) {
        val job = jobs[id] ?: return
        val deliver = synchronized(job) {
            if (job.finished) return
            job.result = result
            job.finished = true
            job.draft = null
            job.deliver
        }
        changed()
        if (deliver != null) network.execute { deliver(job) }
    }

    /** The PC (peer) asks again on a new connection; false if it has no such job. */
    fun resume(id: String, peer: String, link: Any, deliver: (Job) -> Boolean): Boolean {
        val job = jobs[id]?.takeIf { it.peer == peer } ?: return false
        val finished = synchronized(job) {
            job.link = link
            job.lostAt = 0
            job.deliver = deliver
            job.finished
        }
        if (finished) deliver(job)
        return true
    }

    /** The PC (peer) has the answer: the job is over. */
    fun acknowledge(id: String, peer: String): Boolean =
        jobs[id]?.takeIf { it.finished && it.peer == peer }?.let { jobs.remove(id) } != null

    /** A connection ended: its jobs wait for the PC to come back. */
    fun detach(link: Any) {
        val now = System.currentTimeMillis()
        for (job in jobs.values) synchronized(job) {
            if (job.link === link) {
                job.link = null
                job.lostAt = now
            }
        }
    }

    /** Jobs whose PC has not been back for so long that it gave up
     * (sign_jobs.py RESUME_FOR): taken off the queue, and returned. */
    fun expire(after: Long = EXPIRE): List<Job> {
        val now = System.currentTimeMillis()
        val gone = jobs.values.filter { it.lostAt > 0 && now - it.lostAt > after }
        gone.forEach { jobs.remove(it.id) }
        if (gone.isNotEmpty()) changed()
        return gone
    }

    /** A PDF opened by hand (debug builds, for trying the screen out): what is
     * drawn on it goes to a file next to the app's files. */
    fun local(file: File, out: File): Job {
        val id = "local-" + System.nanoTime()
        val job = Job(id, file, file.name, "", null, ownsFile = false)
        start(job, null) {
            it.result?.let { result -> out.writeText(result.toString()) }
            acknowledge(id, "")
            true
        }
        return job
    }

    const val EXPIRE = 15 * 60_000L  // ms: the PC looks for the tablet for 10 minutes
}
