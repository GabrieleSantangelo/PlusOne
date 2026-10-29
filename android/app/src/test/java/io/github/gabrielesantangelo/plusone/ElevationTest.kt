package io.github.gabrielesantangelo.plusone

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

/** The app brought to the front for the PC's camera or microphone (CompanionService.foreground). */
class ElevationTest {
    @Test
    fun aLaunchThatNeverCameToTheFrontIsTriedAgain() {
        val launches = AtomicInteger()
        val elevation = Elevation { launches.incrementAndGet() }  // CaptureActivity never resumes
        assertFalse(elevation.await(10))
        assertFalse(elevation.await(10))  // the PC asks again: launched again, not waiting on the first
        assertEquals(2, launches.get())
    }

    @Test(timeout = 10_000)
    fun theCameraAndTheMicrophoneAskedAtOnceShareOneLaunch() {
        val launched = CountDownLatch(1)
        val launches = AtomicInteger()
        val elevation = Elevation { launches.incrementAndGet(); launched.countDown() }
        val came = AtomicInteger()
        val asking = List(2) { Thread { if (elevation.await(5_000)) came.incrementAndGet() }.apply { start() } }
        assertTrue(launched.await(5, TimeUnit.SECONDS))
        while (asking.any { it.state != Thread.State.TIMED_WAITING }) Thread.sleep(5)  // both waiting
        elevation.elevated()
        asking.forEach { it.join() }
        assertEquals(1, launches.get())
        assertEquals(2, came.get())
    }

    @Test
    fun onceInFrontTheNextAskLaunchesAnew() {
        val launches = AtomicInteger()
        lateinit var elevation: Elevation
        elevation = Elevation { launches.incrementAndGet(); elevation.elevated() }  // in front at once
        assertTrue(elevation.await(1_000))
        assertTrue(elevation.await(1_000))
        assertEquals(2, launches.get())
    }
}
