package io.github.gabrielesantangelo.plusone.link

import io.github.gabrielesantangelo.plusone.R

/**
 * The words for what the desktop comes by (PathOverlay's pill): the
 * transport where there is one to say, and the path in the user's words.
 * "QUIC · Wi-Fi", "TCP · Wi-Fi Direct", "Cable", "Bluetooth"; on the pill,
 * after "PC screen". No Android here: the strings come from string(id),
 * Context.getString in the app.
 */
object PathLabel {
    /** The path's name as a label; "Reconnecting…" while there is none. */
    fun place(path: String?): Int = when (path) {
        "usb" -> R.string.path_cable
        "lan" -> R.string.path_wifi
        "p2p", "wifi-direct", "ap" -> R.string.path_wifi_direct
        "bt", "bluetooth" -> R.string.path_bluetooth
        "relay" -> R.string.path_internet
        null -> R.string.path_reconnecting
        else -> R.string.path_wifi
    }

    /** The transport's word: QUIC or TCP on an IP path (the core's
     * "transport"), UDP for the link of before; none on a stream of its
     * own (the cable, Bluetooth), nor while there is no path. */
    fun transport(path: String?, transport: String?): Int? = if (path == null) null else when (transport) {
        "quic" -> R.string.transport_quic
        "tcp" -> R.string.transport_tcp
        "udp" -> R.string.transport_udp
        else -> null
    }

    fun text(path: String?, transport: String?, string: (Int) -> String): String {
        val place = string(place(path))
        val word = transport(path, transport) ?: return place
        return string(R.string.path_label).format(string(word), place)
    }

    /** What a change of the desktop's PC (LinkStatus) is to its pill and
     * its change screen (PathOverlay), which show shownPath, shownTransport. */
    sealed interface Change {
        /** The session over: the activity goes. */
        data object Gone : Change
        /** Nothing the pill says changed (its name, say). */
        data object Same : Change
        /** The path lost: the change screen, waiting for another. */
        data object Waiting : Change
        /** On another path, or on one again after a wait: the change screen and the pill. */
        data class Moved(val path: String, val transport: String?) : Change
        /** The same path carried otherwise (TCP moved to QUIC): the pill alone. */
        data class Transport(val path: String, val transport: String?) : Change
    }

    fun change(shownPath: String?, shownTransport: String?, now: LinkStatus.Pc?): Change {
        if (now == null) return Change.Gone
        val path = now.path ?: return Change.Waiting
        return when {
            path != shownPath -> Change.Moved(path, now.transport)
            now.transport != shownTransport -> Change.Transport(path, now.transport)
            else -> Change.Same
        }
    }

    /** The pill over the desktop: whose screen it is (SEC-30), and text(). */
    fun marker(path: String?, transport: String?, string: (Int) -> String): String =
        string(R.string.display_marker).format(text(path, transport, string))
}
