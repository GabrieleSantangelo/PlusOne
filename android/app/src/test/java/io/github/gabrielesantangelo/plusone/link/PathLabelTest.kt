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

    @Test
    fun aMoveToQuicOnTheSamePathIsAChange() {
        val pc = "ab".repeat(32)
        LinkStatus.event(JSONObject("""{"event": "up", "device": "$pc", "name": "PC", "path": "lan", "transport": "tcp"}"""))
        assertEquals(LinkStatus.Pc(pc, "PC", "lan", "tcp"), LinkStatus.pcs[pc])
        LinkStatus.event(JSONObject("""{"event": "moved", "device": "$pc", "path": "lan", "transport": "quic"}"""))
        assertEquals(LinkStatus.Pc(pc, "PC", "lan", "quic"), LinkStatus.pcs[pc])
        LinkStatus.event(JSONObject("""{"event": "detached", "device": "$pc"}"""))
        assertEquals(LinkStatus.Pc(pc, "PC", null, null), LinkStatus.pcs[pc])
        // An older core's events, with no transport (or a null one): none.
        LinkStatus.event(JSONObject("""{"event": "moved", "device": "$pc", "path": "usb", "transport": null}"""))
        assertEquals(LinkStatus.Pc(pc, "PC", "usb", null), LinkStatus.pcs[pc])
        LinkStatus.event(JSONObject("""{"event": "gone", "device": "$pc"}"""))
        assertNull(LinkStatus.pcs[pc])
    }
}
