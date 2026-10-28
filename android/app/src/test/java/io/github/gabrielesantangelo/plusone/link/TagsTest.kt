package io.github.gabrielesantangelo.plusone.link

import org.junit.Assert.assertEquals
import org.junit.Test

class TagsTest {
    private val app = "aa".repeat(32)
    private val pc = "bb".repeat(32)

    @Test
    fun theSharedVector() {  // as plusone/link/tags.py's and mdns.rs's tests
        assertEquals("19ab5569a757519b", Tags.tag(app, pc, 2_000_000))
        assertEquals("c2cc4faea63bb065", Tags.tag(app, pc, 2_000_001))
    }

    @Test
    fun oneTagAPcAtMostEight() {
        val pcs = (0 until 10).map { "%064x".format(it) }
        assertEquals(Tags.MOST, Tags.announced(app, pcs, 2_000_000).split(",").size)
        assertEquals("", Tags.announced(app, emptyList(), 2_000_000))
    }
}
