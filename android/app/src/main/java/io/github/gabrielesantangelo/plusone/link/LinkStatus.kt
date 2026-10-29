package io.github.gabrielesantangelo.plusone.link

import android.content.Context
import android.os.Handler
import android.os.Looper
import io.github.gabrielesantangelo.plusone.R
import org.json.JSONObject
import java.util.concurrent.CopyOnWriteArrayList

/**
 * What this tablet is connected to its PCs by, from the link's core's
 * events (LinkCore.onEvent): a session up, moved to another path, waiting
 * for one (detached), gone. Shown on the main screen and over the desktop
 * (PathOverlay).
 */
object LinkStatus {
    /** A PC with a session: its id (fingerprint), name, path ("usb",
     * "lan"...; null while the session waits for one) and what carries it
     * ("quic", "tcp", "stream": a cable's or Bluetooth's; null with no path,
     * or from a core that does not say). */
    data class Pc(val id: String, val name: String, val path: String?, val transport: String? = null)

    @Volatile var pcs: Map<String, Pc> = emptyMap()
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
            else -> return
        }
        pcs = pcs.toMutableMap().apply { if (now == null) remove(id) else put(id, now) }
        if (before != now) main.post { for (listener in listeners) listener(before, now) }
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
