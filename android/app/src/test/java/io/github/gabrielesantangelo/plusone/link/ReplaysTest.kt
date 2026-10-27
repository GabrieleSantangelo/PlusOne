package io.github.gabrielesantangelo.plusone.link

import org.junit.Assert.assertEquals
import org.junit.Test

class ReplaysTest {
    private fun heard(window: Int, vararg counts: Long): List<Boolean> {
        val replays = Replays(window)
        return counts.map { replays.fresh(it) }
    }

    @Test
    fun inOrderIsFresh() =
        assertEquals(listOf(true, true, true), heard(8, 0, 1, 2))

    @Test
    fun aRepeatIsAReplay() =
        assertEquals(listOf(true, true, false, false), heard(8, 0, 1, 1, 0))

    @Test
    fun outOfOrderWithinTheWindowIsKept() =
        assertEquals(listOf(true, true, true, true, false), heard(8, 0, 3, 1, 2, 3))

    @Test
    fun tooOldIsAReplay() =
        // 2 is 8 before 10: past the window, whether it was heard or not.
        assertEquals(listOf(true, true, false, true), heard(8, 2, 10, 2, 3))

    @Test
    fun aJumpForgetsWhatWasHeard() =
        // After 100, the bits of 0..7 must not say 96..99 were heard.
        assertEquals(listOf(true, true, true, true, false), heard(8, 0, 1, 100, 97, 100))

    @Test
    fun negativeIsNever() = assertEquals(listOf(false), heard(8, -1))
}
