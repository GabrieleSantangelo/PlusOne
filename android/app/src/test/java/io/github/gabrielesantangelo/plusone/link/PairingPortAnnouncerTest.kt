package io.github.gabrielesantangelo.plusone.link

import org.junit.Assert.assertEquals
import org.junit.Test

class PairingPortAnnouncerTest {
    @Test fun onlyTheTcpListenersPortIsWhereThePcPairs() {
        val told = mutableListOf<Int>()
        val announcer = PairingPortAnnouncer { told += it }
        announcer.announce("tcp", 47236, "id", "name", 1)
        announcer.announce("usb", 1, "id", "name", 1)
        announcer.announce("tcp", 51000, "id", "name", 1)  // another, if the first was taken
        assertEquals(listOf(47236, 51000), told)
    }
}
