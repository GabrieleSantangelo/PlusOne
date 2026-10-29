package io.github.gabrielesantangelo.plusone

import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Test
import java.io.File
import java.nio.file.Files

/** The app's update from a PC, as it comes: where it is written. */
class UpdatesTest {
    private val cache = Files.createTempDirectory("cache").toFile()

    @After
    fun delete() {
        cache.deleteRecursively()
    }

    @Test
    fun twoUpdatesAtOnceEachWriteAFileOfTheirOwn() {
        val first = Updates.incoming(cache)
        val second = Updates.incoming(cache)
        assertNotEquals(first, second)
        first.writeText("the first PC's")
        second.writeText("the second PC's")
        assertEquals("the first PC's", first.readText())
        second.delete()  // the second one's done (refused, say) as the first is checked
        assertEquals("the first PC's", first.readText())
    }

    @Test
    fun whatUpdatesLeftIsCleanedAsTheServiceStarts() {
        val left = Updates.incoming(cache).apply { writeText("an install that ended the process") }
        val before = File(cache, "update.apk").apply { writeText("the file of before") }
        val other = File(cache, "other").apply { writeText("not an update's") }
        Updates.clean(cache)
        assertFalse(left.exists())
        assertFalse(before.exists())
        assertEquals("not an update's", other.readText())
    }
}
