package io.github.gabrielesantangelo.plusone.link

import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** What the tablet is connected to its PCs by, from the core's events. */
class LinkStatusTest {
    private fun up(device: String, path: String) =
        JSONObject().put("event", "up").put("device", device).put("name", "PC $device").put("path", path)

    @After
    fun clear() = LinkStatus.clear()

    @Test
    fun aPcIsConnectedFromUpUntilGone() {
        LinkStatus.event(up("a", "lan"))
        assertEquals("lan", LinkStatus.current("a")?.path)
        LinkStatus.event(JSONObject().put("event", "gone").put("device", "a"))
        assertNull(LinkStatus.current("a"))
    }

    @Test
    fun noPcIsConnectedOnceTheCoreStopped() {
        // The core says nothing more once closed: no "gone" for them.
        LinkStatus.event(up("a", "lan"))
        LinkStatus.event(up("b", "usb"))
        LinkStatus.clear()
        assertEquals(emptyMap<String, LinkStatus.Pc>(), LinkStatus.pcs)
        assertNull(LinkStatus.current())
    }
}
