package io.github.gabrielesantangelo.plusone

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Test

/** Wi-Fi Direct's group, the process's network (P2pHandler, ApHandler): a
 * session ending lets go of what it made, not of what a newer one did. */
class NewestTest {
    private val newest = Newest()
    private val first = Any()
    private val second = Any()
    private val lets = mutableListOf<String>()

    @Test
    fun aSessionAloneLetsItGoOnce() {
        newest.take(first)
        newest.letGo(first) { lets += "first" }
        newest.letGo(first) { lets += "first again" }
        assertEquals(listOf("first"), lets)
    }

    @Test
    fun aSessionBeforeANewerOneLeavesItsGroupBe() {
        newest.take(first)
        newest.take(second)  // the PC asked again: the group is made anew
        newest.letGo(first) { lets += "first" }  // the first one's link ends after
        newest.letGo(second) { lets += "second" }
        assertEquals(listOf("second"), lets)
    }

    @Test
    fun aNewerSessionThatDidNotGetItLeavesItToTheOneBefore() {
        newest.take(first)
        assertFalse(newest.take(second) { false })  // its network not bound
        newest.letGo(second) { lets += "second" }
        newest.letGo(first) { lets += "first" }
        assertEquals(listOf("first"), lets)
    }

    @Test(timeout = 10_000)
    fun aNewerSessionTakesItOnlyOnceALetGoUnderWayIsDone() {
        newest.take(first)
        lateinit var taking: Thread
        newest.letGo(first) {
            taking = Thread { newest.take(second); synchronized(lets) { lets += "second took it" } }.apply { start() }
            Thread.sleep(200)
            synchronized(lets) { lets += "first let it go" }
        }
        taking.join()
        assertEquals(listOf("first let it go", "second took it"), lets)
        newest.letGo(second) { lets += "second" }
        assertEquals("second", lets.last())
    }
}
