package io.github.gabrielesantangelo.plusone.link

import org.junit.Assert.assertEquals
import org.junit.Test
import java.net.InetAddress

class NetworkWatchTest {
    private fun at(address: String) = InetAddress.getByName(address)

    @Test fun whatAPcCannotDialIsLeftOut() {
        val told = NetworkWatch.usable(listOf(
            at("192.168.1.112") to 0, at("127.0.0.1") to 0, at("169.254.3.4") to 0, at("fe80::1") to 0,
            at("2001:db8::7") to 0, at("10.0.0.7") to 0))
        // IPv4 only (the link listens on 0.0.0.0), not loopback or link-local.
        assertEquals(listOf("192.168.1.112", "10.0.0.7"), told)
    }

    @Test fun ipv4ComesFirstAndNoneTwice() {
        assertEquals(listOf("10.0.0.2", "192.168.1.2", "2001:db8::1"),
            NetworkWatch.order(listOf("2001:db8::1", "10.0.0.2", "192.168.1.2", "10.0.0.2")))
    }
}
