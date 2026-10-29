package io.github.gabrielesantangelo.plusone.link

import io.github.gabrielesantangelo.plusone.R
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.w3c.dom.Element
import java.io.File
import javax.xml.parsers.DocumentBuilderFactory

class PathLabelTest {
    /** The app's own strings (res/values, then a language's over them) by
     * their R ids, as Context.getString gives them: no Android here. */
    private fun strings(vararg folders: String): (Int) -> String {
        val byName = HashMap<String, String>()
        for (folder in folders) {
            val xml = DocumentBuilderFactory.newInstance().newDocumentBuilder()
                .parse(File("src/main/res/$folder/strings.xml"))
            val nodes = xml.getElementsByTagName("string")
            for (i in 0 until nodes.length) {
                val node = nodes.item(i) as Element
                byName[node.getAttribute("name")] = node.textContent.replace("\\'", "'")
            }
        }
        val names = R.string::class.java.fields.associate { it.getInt(null) to it.name }
        return { id -> byName.getValue(names.getValue(id)) }
    }

    private val english = strings("values")
    private val italian = strings("values", "values-it")

    @Test
    fun theTransportAndThePath() {
        assertEquals("QUIC · Wi-Fi", PathLabel.text("lan", "quic", english))
        assertEquals("TCP · Wi-Fi", PathLabel.text("lan", "tcp", english))
        assertEquals("QUIC · Wi-Fi Direct", PathLabel.text("p2p", "quic", english))
        assertEquals("TCP · Wi-Fi Direct", PathLabel.text("ap", "tcp", english))
        assertEquals("TCP · Internet", PathLabel.text("relay", "tcp", english))
        // The link of before: the desktop as plain UDP.
        assertEquals("UDP · Wi-Fi", PathLabel.text("lan", "udp", english))
    }

    @Test
    fun aStreamOfItsOwnHasNoTransportWord() {
        assertEquals("Cable", PathLabel.text("usb", "stream", english))
        assertEquals("Bluetooth", PathLabel.text("bt", "stream", english))
        assertEquals("Bluetooth", PathLabel.text("bluetooth", null, english))
    }

    @Test
    fun noPathNoTransport() {
        assertEquals("Reconnecting…", PathLabel.text(null, null, english))
        assertEquals("Reconnecting…", PathLabel.text(null, "quic", english))
        // A core that does not say its transport: the path alone.
        assertEquals("Wi-Fi", PathLabel.text("lan", null, english))
    }

    @Test
    fun inItalian() {
        assertEquals("QUIC · Wi-Fi", PathLabel.text("lan", "quic", italian))
        assertEquals("TCP · Wi-Fi Direct", PathLabel.text("p2p", "tcp", italian))
        assertEquals("Cavo", PathLabel.text("usb", "stream", italian))
        assertEquals("Bluetooth", PathLabel.text("bt", "stream", italian))
        assertEquals("TCP · Internet", PathLabel.text("relay", "tcp", italian))
        assertEquals("Riconnessione…", PathLabel.text(null, null, italian))
    }

    @Test
    fun thePillSaysWhoseScreenItIs() {
        assertEquals("PC screen · QUIC · Wi-Fi", PathLabel.marker("lan", "quic", english))
        assertEquals("PC screen · TCP · Wi-Fi Direct", PathLabel.marker("p2p", "tcp", english))
        assertEquals("PC screen · Cable", PathLabel.marker("usb", "stream", english))
        assertEquals("PC screen · UDP · Wi-Fi", PathLabel.marker("lan", "udp", english))
        assertEquals("PC screen · Reconnecting…", PathLabel.marker(null, null, english))
        assertEquals("Schermo del PC · QUIC · Wi-Fi", PathLabel.marker("lan", "quic", italian))
        assertEquals("Schermo del PC · Cavo", PathLabel.marker("usb", "stream", italian))
        assertEquals("Schermo del PC · Riconnessione…", PathLabel.marker(null, null, italian))
    }

    /** An event as Android's org.json reads it: optString gives a JSON null
     * as "null" (this one, the JVM's, gives the fallback, ""). */
    private fun onAndroid(text: String) = object : JSONObject(text) {
        override fun optString(key: String): String = opt(key)?.toString() ?: ""
    }

    @Test
    fun aMoveToQuicOnTheSamePathIsAChange() {
        val pc = "ab".repeat(32)
        LinkStatus.event(onAndroid("""{"event": "up", "device": "$pc", "name": "PC", "path": "lan", "transport": "tcp"}"""))
        assertEquals(LinkStatus.Pc(pc, "PC", "lan", "tcp"), LinkStatus.pcs[pc])
        LinkStatus.event(onAndroid("""{"event": "moved", "device": "$pc", "path": "lan", "transport": "quic"}"""))
        assertEquals(LinkStatus.Pc(pc, "PC", "lan", "quic"), LinkStatus.pcs[pc])
        LinkStatus.event(onAndroid("""{"event": "detached", "device": "$pc"}"""))
        assertEquals(LinkStatus.Pc(pc, "PC", null, null), LinkStatus.pcs[pc])
        LinkStatus.event(onAndroid("""{"event": "gone", "device": "$pc"}"""))
        assertNull(LinkStatus.pcs[pc])
    }

    @Test
    fun aNullOrMissingTransportIsNone() {
        val pc = "cd".repeat(32)
        assertEquals("null", onAndroid("""{"transport": null}""").optString("transport"))  // what the guard is for
        LinkStatus.event(onAndroid("""{"event": "up", "device": "$pc", "name": "PC", "path": null, "transport": null}"""))
        assertEquals(LinkStatus.Pc(pc, "PC", null, null), LinkStatus.pcs[pc])
        // An older core's events: no transport at all.
        LinkStatus.event(onAndroid("""{"event": "moved", "device": "$pc", "path": "usb"}"""))
        assertEquals(LinkStatus.Pc(pc, "PC", "usb", null), LinkStatus.pcs[pc])
        LinkStatus.event(onAndroid("""{"event": "gone", "device": "$pc"}"""))
    }

    @Test
    fun whatAChangeIsToTheDesktop() {
        val pc = LinkStatus.Pc("ef".repeat(32), "PC", "lan", "tcp")
        assertEquals(PathLabel.Change.Gone, PathLabel.change("lan", "tcp", null))
        assertEquals(PathLabel.Change.Same, PathLabel.change("lan", "tcp", pc))
        assertEquals(PathLabel.Change.Same, PathLabel.change("lan", "tcp", pc.copy(name = "Renamed")))
        assertEquals(PathLabel.Change.Waiting, PathLabel.change("lan", "tcp", pc.copy(path = null, transport = null)))
        // TCP moved to QUIC: the same path, the pill alone.
        assertEquals(PathLabel.Change.Transport("lan", "quic"), PathLabel.change("lan", "tcp", pc.copy(transport = "quic")))
        // Another path, or one again after a wait: the change screen.
        assertEquals(PathLabel.Change.Moved("usb", "stream"), PathLabel.change("lan", "quic", pc.copy(path = "usb", transport = "stream")))
        assertEquals(PathLabel.Change.Moved("lan", "quic"), PathLabel.change(null, null, pc.copy(transport = "quic")))
        // The desktop opened before its PC's session was known.
        assertEquals(PathLabel.Change.Moved("lan", "tcp"), PathLabel.change(null, null, pc))
    }
}
