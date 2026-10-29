package io.github.gabrielesantangelo.plusone.link

import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
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

    @Test
    fun wifiWithoutPowerSavingOnlyForAPathOverWifi() {
        // CompanionService.lowLatency: the lock while a session's path is one of these.
        for (path in listOf("lan", "p2p", "ap", "relay")) assertTrue(path, LinkStatus.overWifi(path))
        for (path in listOf("usb", "bt", null)) assertFalse("$path", LinkStatus.overWifi(path))
    }

    @Test
    fun aSessionsPathFollowsItsPc() {
        assertEquals("lan", LinkStatus.path(null))  // the link of before: TCP on the network
        assertNull(LinkStatus.path("a"))  // no session (yet) over the core
        LinkStatus.event(up("a", "lan"))
        assertTrue(LinkStatus.overWifi(LinkStatus.path("a")))
        LinkStatus.event(JSONObject().put("event", "moved").put("device", "a").put("path", "usb"))
        assertFalse(LinkStatus.overWifi(LinkStatus.path("a")))  // the cable plugged in: no lock
        LinkStatus.event(JSONObject().put("event", "detached").put("device", "a"))
        assertFalse(LinkStatus.overWifi(LinkStatus.path("a")))  // waiting for a path
        LinkStatus.event(JSONObject().put("event", "moved").put("device", "a").put("path", "p2p").put("transport", "quic"))
        assertTrue(LinkStatus.overWifi(LinkStatus.path("a")))  // back on Wi-Fi: the lock again
    }
}
