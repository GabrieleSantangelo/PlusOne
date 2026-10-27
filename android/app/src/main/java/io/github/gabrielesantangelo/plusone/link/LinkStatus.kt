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
    /** A PC with a session: its id (fingerprint), name, and path ("usb",
     * "lan"...; null while the session waits for one). */
    data class Pc(val id: String, val name: String, val path: String?)

    @Volatile var pcs: Map<String, Pc> = emptyMap()
        private set
    private val listeners = CopyOnWriteArrayList<(Pc?, Pc?) -> Unit>()
    private val main = Handler(Looper.getMainLooper())

    /** listener(before, now) on the main thread, for each change of a PC's path. */
    fun listen(listener: (Pc?, Pc?) -> Unit) = listeners.add(listener)
    fun unlisten(listener: (Pc?, Pc?) -> Unit) = listeners.remove(listener)

    /** The one the user cares about: the PC whose desktop this is, or any. */
    fun current(device: String? = null): Pc? = device?.let { pcs[it] } ?: pcs.values.firstOrNull()

    /** From the core's events thread. */
    fun event(event: JSONObject) {
        val id = event.optString("device").ifEmpty { return }
        val before = pcs[id]
        val now = when (event.optString("event")) {
            "up" -> Pc(id, event.optString("name").ifEmpty { before?.name.orEmpty() }, event.optString("path").ifEmpty { null })
            "moved" -> (before ?: Pc(id, "", null)).copy(path = event.optString("path").ifEmpty { null })
            "detached" -> before?.copy(path = null) ?: return
            "gone" -> null
            else -> return
        }
        pcs = pcs.toMutableMap().apply { if (now == null) remove(id) else put(id, now) }
        if (before != now) main.post { for (listener in listeners) listener(before, now) }
    }

    fun icon(path: String?): Int = when (path) {
        "usb" -> R.drawable.ic_cable
        null -> R.drawable.ic_sync
        else -> R.drawable.ic_wifi
    }

    /** Its name, as a label ("Cable") or in a sentence ("via cable"). */
    fun name(context: Context, path: String?, inSentence: Boolean = false): String = context.getString(when (path) {
        "usb" -> if (inSentence) R.string.path_cable_lc else R.string.path_cable
        "lan" -> R.string.path_wifi
        "wifi-direct", "ap" -> R.string.path_wifi_direct
        "bluetooth" -> R.string.path_bluetooth
        "relay" -> if (inSentence) R.string.path_internet_lc else R.string.path_internet
        null -> R.string.path_reconnecting
        else -> R.string.path_wifi
    })
}
