package io.github.gabrielesantangelo.plusone.link

import android.content.Context
import android.os.Handler
import android.os.Looper
import android.util.Log
import io.github.gabrielesantangelo.plusone.R
import org.json.JSONObject
import java.util.concurrent.CopyOnWriteArrayList

/**
 * What this tablet is connected to its PCs by, from the link's core's
 * events (LinkCore.onEvent): a session up, moved to another path, waiting
 * for one (detached), gone. Shown on the main screen and over the desktop
 * (PathOverlay). Each path's state (active, suspect, failed) is kept too,
 * and logged when it is in trouble: nothing shows it yet.
 */
object LinkStatus {
    /** A PC with a session: its id (fingerprint), name, path ("usb",
     * "lan"...; null while the session waits for one) and what carries it
     * ("quic", "tcp", "stream": a cable's or Bluetooth's; null with no path,
     * or from a core that does not say). */
    data class Pc(val id: String, val name: String, val path: String?, val transport: String? = null)

    @Volatile var pcs: Map<String, Pc> = emptyMap()
        private set

    /** A path of a PC's as the core last told it (its "path" event):
     * kind ("usb", "lan", "bt"...), transport, state ("active", "suspect",
     * "failed"), round trip, how long nothing was heard on it, its budget
     * (bits a second) and whose ("gcc", "drain", "prior", "none"). */
    data class Path(val id: Long, val kind: String, val transport: String?, val state: String, val rttMs: Double?,
                    val silentMs: Long, val budgetBps: Long?, val budgetFrom: String?)

    /** Each PC's paths by their id; a failed one forgotten. */
    @Volatile var paths: Map<String, Map<Long, Path>> = emptyMap()
        private set
    private val listeners = CopyOnWriteArrayList<(Pc?, Pc?) -> Unit>()
    private val main = Handler(Looper.getMainLooper())

    /** listener(before, now) on the main thread, for each change of a PC's
     * path or transport (a TCP path moved to QUIC: the same path). */
    fun listen(listener: (Pc?, Pc?) -> Unit) = listeners.add(listener)
    fun unlisten(listener: (Pc?, Pc?) -> Unit) = listeners.remove(listener)

    /** The one the user cares about: the PC whose desktop this is, or any. */
    fun current(device: String? = null): Pc? = device?.let { pcs[it] } ?: pcs.values.firstOrNull()

    /** From the core's events thread. */
    @Synchronized
    fun event(event: JSONObject) {
        val id = event.optString("device").ifEmpty { return }
        val before = pcs[id]
        // JSON's null comes as "null" from optString: neither is a path or a transport.
        fun text(key: String) = if (event.isNull(key)) null else event.optString(key).ifEmpty { null }
        val now = when (event.optString("event")) {
            "up" -> Pc(id, event.optString("name").ifEmpty { before?.name.orEmpty() }, text("path"), text("transport"))
            "moved" -> (before ?: Pc(id, "", null)).copy(path = text("path"), transport = text("transport"))
            "detached" -> before?.copy(path = null, transport = null) ?: return
            "gone" -> null
            "path" -> return state(id, event)
            else -> return
        }
        if (now == null) paths = paths - id
        pcs = pcs.toMutableMap().apply { if (now == null) remove(id) else put(id, now) }
        if (before != now) main.post { for (listener in listeners) listener(before, now) }
    }

    /** The core stopped (the service did): no PC has a session with it, and
     * none is shown with one, whatever its events said last (it says no
     * more once closed); no path of theirs kept. */
    @Synchronized
    fun clear() {
        val before = pcs.values.toList()
        paths = emptyMap()
        pcs = emptyMap()
        if (before.isNotEmpty()) main.post { for (pc in before) for (listener in listeners) listener(pc, null) }
    }

    /** The path a session with device is on now: its PC's over the link's
     * core (null while it waits for one); over the link of before (no
     * device), the network's. */
    fun path(device: String?): String? = if (device == null) "lan" else pcs[device]?.path

    /** Whether a session on path goes through the tablet's Wi-Fi: a LAN's,
     * Wi-Fi Direct, the PC's access point, the internet's (the relay); not
     * the cable nor Bluetooth, nor while it waits for a path. */
    fun overWifi(path: String?): Boolean = path != null && path !in setOf("usb", "bt", "bluetooth")

    /** A path's state changed: kept, logged if it is in trouble or out of
     * it; nothing shown changes. Under event's lock. */
    private fun state(pc: String, event: JSONObject) {
        fun text(key: String) = if (event.isNull(key)) null else event.optString(key).ifEmpty { null }
        if (event.isNull("id")) return
        val told = Path(event.optLong("id"), text("kind").orEmpty(), text("transport"), text("state") ?: return,
            if (event.isNull("rtt_ms")) null else event.optDouble("rtt_ms"), event.optLong("silent_ms"),
            if (event.isNull("budget_bps")) null else event.optLong("budget_bps"), text("budget_from"))
        val known = paths[pc].orEmpty()
        val before = known[told.id]
        val now = if (told.state == "failed") known - told.id else known + (told.id to told)
        paths = paths + (pc to now)
        if (told.state != "active" || before?.state == "suspect") {
            Log.i(LinkCore.TAG, "link: the ${told.kind} path (${told.id}) is ${told.state}, " +
                "nothing heard for ${told.silentMs} ms")
        }
    }

    fun icon(path: String?): Int = when (path) {
        "usb" -> R.drawable.ic_cable
        "bt", "bluetooth" -> R.drawable.ic_bluetooth
        null -> R.drawable.ic_sync
        else -> R.drawable.ic_wifi
    }

    /** Its name, as a label ("Cable") or in a sentence ("via cable"). */
    fun name(context: Context, path: String?, inSentence: Boolean = false): String = context.getString(when {
        inSentence && path == "usb" -> R.string.path_cable_lc
        inSentence && path == "relay" -> R.string.path_internet_lc
        else -> PathLabel.place(path)
    })
}
