package io.github.gabrielesantangelo.plusone

import android.Manifest
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothServerSocket
import android.bluetooth.BluetoothSocket
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.os.Handler
import android.os.Looper
import android.os.ParcelFileDescriptor
import android.util.Log
import io.github.gabrielesantangelo.plusone.link.LinkCore
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.InputStream
import java.io.OutputStream
import java.util.UUID

/**
 * Bluetooth as a path of the link's own ("bt"): slow, but there with no
 * network at all (plusone/link/bt.py), over the bond the PC and the tablet
 * made in their Bluetooth settings. While the app runs and Bluetooth is on,
 * it listens on RFCOMM under Plus One's UUID (a listening socket: nothing
 * awake, the controller's page scan is there anyway), again each time
 * Bluetooth comes back on (the socket and its SDP record go with it); a PC
 * connects when it has no better path. What comes and goes on the Bluetooth
 * socket is copied to a socket pair whose other end the link's core takes
 * as a stream (Android gives an app a Bluetooth socket's streams, not its
 * descriptor); the PC makes the session (or a path for it) there.
 */
class BtListener(private val service: CompanionService, private val core: LinkCore) {
    @Volatile private var server: BluetoothServerSocket? = null
    private var started = false        // under this: from start() to stop()
    private val retries = BtRetries()  // under this
    private val main = Handler(Looper.getMainLooper())
    private val again = Runnable { open() }

    /** Bluetooth on: listening; going off: not (the socket dies with it). */
    private val stateChanged = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            val listen = synchronized(this@BtListener) {
                retries.state(intent.getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR))
            }
            when (listen) {
                true -> open()
                false -> if (close()) Log.i(CompanionService.TAG, "bt: not listening: Bluetooth off")
                null -> {}
            }
        }
    }

    /** Listening whenever Bluetooth is on, until stop(); again from the app
     * (MainActivity), which may have been allowed Bluetooth since. */
    fun start() {
        synchronized(this) {
            if (!started) service.registerReceiver(stateChanged, IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED))
            started = true
            retries.reset()
        }
        open()
    }

    fun stop() {
        synchronized(this) {
            if (started) service.unregisterReceiver(stateChanged)
            started = false
        }
        close()
    }

    /** The server, one at a time: unless Bluetooth is off or not allowed. */
    private fun open() {
        synchronized(this) {
            if (!started || server != null) return
            if (service.checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT) != PackageManager.PERMISSION_GRANTED) return
            val adapter = service.getSystemService(BluetoothManager::class.java)?.adapter ?: return
            if (!adapter.isEnabled) return
            val listening = try {
                adapter.listenUsingInsecureRfcommWithServiceRecord("Plus One", UUID.fromString(UUID_TEXT))
            } catch (e: java.io.IOException) {
                return failed(e, adapter)
            }
            server = listening
            Log.i(CompanionService.TAG, "bt: listening")
            Thread({
                while (server === listening) {
                    val socket = try { listening.accept() } catch (e: java.io.IOException) {
                        // Closed here (close()), or under it: Bluetooth's stack restarted, say.
                        synchronized(this) {
                            if (server === listening) {
                                server = null
                                runCatching { listening.close() }
                                failed(e, adapter)
                            }
                        }
                        break
                    }
                    synchronized(this) { retries.reset() }
                    bridge(socket)
                }
            }, "link-bt-listen").start()
        }
    }

    /** Not listening with Bluetooth on: again in a while (BtRetries). Under this. */
    private fun failed(e: java.io.IOException, adapter: BluetoothAdapter) {
        val wait = retries.failed(started, adapter.isEnabled)
        Log.i(CompanionService.TAG, "bt: not listening: $e" + (wait?.let { ", again in $it ms" } ?: ""))
        main.removeCallbacks(again)
        if (wait != null) main.postDelayed(again, wait)
    }

    /** The server closed, and no try to open it pending: whether there was one. */
    private fun close(): Boolean {
        val listening = synchronized(this) {
            main.removeCallbacks(again)
            server.also { server = null }
        } ?: return false
        runCatching { listening.close() }
        return true
    }

    private fun bridge(socket: BluetoothSocket) {
        try {
            val (ours, theirs) = ParcelFileDescriptor.createSocketPair()
            copy("link-bt-in", socket.inputStream, FileOutputStream(ours.fileDescriptor), socket, ours)
            copy("link-bt-out", FileInputStream(ours.fileDescriptor), socket.outputStream, socket, ours)
            core.acceptStream(theirs.detachFd(), "bt")
            Log.i(CompanionService.TAG, "bt: a PC connected")
        } catch (e: Exception) {
            Log.i(CompanionService.TAG, "bt: $e")
            runCatching { socket.close() }
        }
    }

    /** One way, on a thread of its own, until either side ends. */
    private fun copy(name: String, from: InputStream, to: OutputStream, socket: BluetoothSocket,
                     pair: ParcelFileDescriptor) {
        Thread({
            val buffer = ByteArray(64 shl 10)
            try {
                while (true) {
                    val length = from.read(buffer)
                    if (length < 0) break
                    to.write(buffer, 0, length)
                    to.flush()
                }
            } catch (e: java.io.IOException) {
                // either side gone
            } finally {
                runCatching { socket.close() }
                runCatching { pair.close() }
            }
        }, name).start()
    }

    companion object {
        const val UUID_TEXT = "5f1a3c9e-8b2d-4e6f-9a7c-0d1b2e3f4a5b"  // plusone/link/bt.py's
    }
}

/**
 * When BtListener listens again, Android aside: as Bluetooth comes on, and
 * after its socket failed with Bluetooth still on, in a second, then in 5
 * and in 30, no more until Bluetooth comes on again, a PC connects or the
 * app opens. No timer otherwise: nothing runs while idle.
 */
class BtRetries {
    private var tries = 0

    /** Bluetooth's new state (BluetoothAdapter.STATE_*): listen (true), stop
     * listening (false), or as it is (null). */
    fun state(state: Int): Boolean? = when (state) {
        BluetoothAdapter.STATE_ON -> {
            tries = 0
            true
        }
        BluetoothAdapter.STATE_TURNING_OFF, BluetoothAdapter.STATE_OFF -> false
        else -> null
    }

    /** Not listening, the listener started: in how many ms to try again, or null. */
    fun failed(started: Boolean, enabled: Boolean): Long? =
        if (!started || !enabled) null else WAITS.getOrNull(tries)?.also { tries++ }

    /** Listening works (a PC connected), or the app asks again. */
    fun reset() {
        tries = 0
    }

    companion object {
        val WAITS = listOf(1_000L, 5_000L, 30_000L)  // ms
    }
}
