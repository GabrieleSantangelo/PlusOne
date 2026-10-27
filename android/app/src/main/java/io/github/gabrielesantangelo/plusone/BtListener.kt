package io.github.gabrielesantangelo.plusone

import android.Manifest
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothServerSocket
import android.bluetooth.BluetoothSocket
import android.content.pm.PackageManager
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
 * awake, the controller's page scan is there anyway); a PC connects when it
 * has no better path. What comes and goes on the Bluetooth socket is copied
 * to a socket pair whose other end the link's core takes as a stream
 * (Android gives an app a Bluetooth socket's streams, not its descriptor);
 * the PC makes the session (or a path for it) there.
 */
class BtListener(private val service: CompanionService, private val core: LinkCore) {
    @Volatile private var server: BluetoothServerSocket? = null

    fun start() {
        if (service.checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT) != PackageManager.PERMISSION_GRANTED) return
        val adapter = service.getSystemService(BluetoothManager::class.java)?.adapter ?: return
        if (!adapter.isEnabled) return
        val listening = try {
            adapter.listenUsingInsecureRfcommWithServiceRecord("Plus One", UUID.fromString(UUID_TEXT))
        } catch (e: java.io.IOException) {
            Log.i(CompanionService.TAG, "bt: not listening: $e")
            return
        }
        server = listening
        Thread({
            while (server === listening) {
                val socket = try { listening.accept() } catch (e: java.io.IOException) { break }  // closed: stop()
                bridge(socket)
            }
        }, "link-bt-listen").start()
    }

    fun stop() {
        val listening = server
        server = null
        runCatching { listening?.close() }
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
