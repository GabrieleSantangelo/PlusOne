package io.github.gabrielesantangelo.plusone

import android.provider.Settings
import android.util.Log
import io.github.gabrielesantangelo.plusone.link.Handler
import io.github.gabrielesantangelo.plusone.link.Link
import io.github.gabrielesantangelo.plusone.link.Wire
import org.json.JSONObject
import java.io.File
import java.security.DigestOutputStream
import java.security.MessageDigest
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong

/**
 * PDFs to sign (plusone/sign_jobs.py): "sign" brings one (its payload) as a
 * job, which joins the queue and opens the signing screen; its answer goes
 * back on the link, or on the one the PC comes back on ("resume") should
 * that drop; "got" ends it. The answer says the PDF's SHA-256 (the PC puts
 * the marks on that PDF only); the saved signatures go both ways only with
 * a PC both sides share them with (PcOptions).
 */
class SignHandler(private val service: CompanionService) : Handler {
    override val types = setOf("sign", "resume", "got")
    override fun maxPayload(type: String) = if (type == "sign") Wire.MAX_PAYLOAD else 0

    // An upload's bytes and slot, from before it starts until it is a job.
    private val incoming = AtomicLong()
    private val arriving = AtomicInteger()

    override fun handle(link: Link, request: JSONObject, size: Long): Boolean {
        val job = request.optString("job")
        when (request.optString("type")) {
            "sign" -> sign(link, request, size)
            "resume" -> if (Jobs.resume(job, link.fingerprint, link) { answer(link, it) }) {
                link.send(JSONObject().put("type", "resumed").put("job", job))
            } else {
                link.send(JSONObject().put("type", "unknown-job").put("job", job))
            }
            "got" -> if (Jobs.acknowledge(job, link.fingerprint)) jobFile(job)?.delete()
        }
        return true
    }

    /** Its jobs wait for the PC to come back. */
    override fun closed(link: Link) = Jobs.detach(link)

    /** Where a job's PDF goes; null for an id that is not the PC's 32 hex digits. */
    private fun jobFile(job: String): File? =
        if (JOB_ID.matches(job)) File(File(service.cacheDir, "jobs").apply { mkdirs() }, "$job.pdf") else null

    /** A PDF to sign: it joins the queue, and the signing screen opens on
     * it (or, already open on another, says it came); its answer goes back. */
    private fun sign(link: Link, message: JSONObject, size: Long) {
        val job = message.optString("job")
        val file = jobFile(job)
        require(file != null && size in 1..Wire.MAX_PAYLOAD && job !in Jobs.ids) { "bad job" }
        // Room for it: a PC, even a paired one, does not get to fill the tablet.
        // Taken before the upload, not checked then written: uploads at once
        // count each other too.
        val reserved = incoming.addAndGet(size)
        val slots = arriving.incrementAndGet()
        val digest = MessageDigest.getInstance("SHA-256")
        try {
            require(Jobs.waiting().size + slots <= MAX_JOBS && Jobs.bytes() + reserved <= MAX_JOB_BYTES &&
                file.parentFile!!.usableSpace > reserved + FREE_SPACE) { "no room for another PDF" }
            DigestOutputStream(file.outputStream(), digest).use { link.readPayload(size, it) }
        } catch (e: Throwable) {
            file.delete()
            throw e
        } finally {
            incoming.addAndGet(-size)
            arriving.decrementAndGet()
        }
        try {
            // Signatures added or removed on the PC since the last job, if shared with it.
            val sharing = message.has("signatures")
            if (sharing && PcOptions.shareSignatures(service, link.fingerprint)) {
                message.optJSONObject("signatures")?.let { Signatures.merge(service, it) }
            }
            val title = message.optString("title", file.name).take(200)
            val sha256 = digest.digest().joinToString("") { "%02x".format(it) }
            // With the PC's pen, so both sides sign alike.
            val added = Jobs.start(Jobs.Job(job, file, title, link.pc, message.optJSONObject("pen"),
                peer = link.fingerprint, sha256 = sha256, sharing = sharing), link) {
                answer(link, it)
            }
            require(added) { "a job of that id already" }
        } catch (e: Throwable) {
            file.delete()  // half a PDF is of no use
            throw e
        }
        // Opening over whatever is on screen needs "Display over other apps";
        // without it, the notification (the queue's) is there to tap.
        if (Settings.canDrawOverlays(service)) service.startActivity(SignActivity.open(service, job))
        link.send(JSONObject().put("type", "opened").put("job", job))
    }

    /** Sends a job's answer (and the saved signatures, if both sides share
     * them); false if the link is gone (the PC asks again). The job stays
     * until the PC says it got it. */
    private fun answer(link: Link, job: Jobs.Job): Boolean = try {
        val result = job.result
        val message = JSONObject().put("job", job.id)
        if (result != null) message.put("type", "signed").put("marks", result).put("sha256", job.sha256)
        else message.put("type", "cancelled")
        if (job.sharing && PcOptions.shareSignatures(service, job.peer)) {
            // For the PC to merge, unless that would make the answer more
            // than the PC takes (link/wire.py: 16 MB).
            val withSignatures = JSONObject(message.toString()).put("signatures", Signatures.toJson(service))
            link.send(if (withSignatures.toString().length < MAX_ANSWER) withSignatures else message)
        } else {
            link.send(message)
        }
        true
    } catch (e: Exception) {
        Log.w(CompanionService.TAG, "could not answer job ${job.id} yet: $e")
        false
    }

    companion object {
        const val MAX_ANSWER = 15 * 1024 * 1024  // characters of an answer, under the PC's 16 MB
        const val MAX_JOBS = 50                  // PDFs waiting at once
        const val MAX_JOB_BYTES = 1024L * 1024 * 1024
        const val FREE_SPACE = 200L * 1024 * 1024  // left free on the tablet, at least
        private val JOB_ID = Regex("[0-9a-f]{32}")
    }
}
