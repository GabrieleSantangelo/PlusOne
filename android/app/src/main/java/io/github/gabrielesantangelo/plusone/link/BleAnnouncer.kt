package io.github.gabrielesantangelo.plusone.link

import android.Manifest
import android.bluetooth.BluetoothManager
import android.bluetooth.le.AdvertiseCallback
import android.bluetooth.le.AdvertiseData
import android.bluetooth.le.AdvertiseSettings
import android.content.Context
import android.content.pm.PackageManager
import android.os.Handler
import android.os.Looper
import android.os.ParcelUuid
import android.util.Log
import java.security.SecureRandom

/**
 * The tablet's address on its Wi-Fi, advertised over Bluetooth LE for the PCs
 * paired with it (BleBlocks): where multicast does not go (eduroam, a guest
 * network) and a PC no longer has an address that holds, it finds the tablet
 * by this. Only that PC can read it.
 *
 * While its user holds pairing open (two minutes) it advertises who and
 * where it is in the clear instead, with its name in the scan response, for
 * a new PC to list it.
 *
 * Power: advertised only while a paired PC has no session with the tablet
 * (one that has none needs nothing found), the tablet is on a Wi-Fi with an
 * IPv4 address, and the link listens; not connectable, at Android's lowest
 * rate (an advertisement a second). Nothing otherwise.
 */
/** Tells where the app's own listener takes pairing (LinkServer announces it to its announcers). */
class PairingPortAnnouncer(private val port: (Int) -> Unit) : Announcer {
    override fun announce(kind: String, where: Int, id: String, name: String, version: Long) {
        if (kind == "tcp") port(where)
    }

    override fun stop() {}
}

class BleAnnouncer(private val context: Context, private val appFingerprint: String,
                   private val paired: () -> Set<String>, private val pairing: () -> Boolean = { false },
                   private val name: () -> String = { "" }, private val pairingPort: () -> Int = { 0 }) {
    private val handler = Handler(Looper.getMainLooper())
    private val random = SecureRandom()
    private var port = 0
    private var addresses: List<String> = emptyList()
    private var connected: Set<String> = emptySet()
    private var rotation = 0
    /** What is advertised now: for PCs (their blocks, in a slot) or for a new one (pairing, with the name). */
    private data class Shown(val pairing: Boolean, val slot: Long, val pcs: List<String>, val address: List<Int>,
                             val port: Int, val name: String)
    private var now: Shown? = null
    private var callback: AdvertiseCallback? = null
    private val rotate = object : Runnable {
        override fun run() {
            synchronized(this@BleAnnouncer) { rotation++ }
            refresh()
        }
    }

    /** Where the link listens, once it does. */
    fun listening(port: Int) {
        synchronized(this) { this.port = port }
        refresh()
    }

    /** The addresses of the networks the tablet is on (NetworkWatch). */
    fun addresses(addresses: List<String>) {
        synchronized(this) { this.addresses = addresses }
        refresh()
    }

    /** The PCs with a session now (their fingerprints): they are not advertised for. */
    fun connected(devices: Set<String>) {
        synchronized(this) { connected = devices }
        refresh()
    }

    /** A session's event (LinkCore.onEvent): up or moved, or detached or gone. */
    fun event(event: org.json.JSONObject) {
        val device = event.optString("device").takeIf { it.isNotEmpty() } ?: return
        synchronized(this) {
            connected = when (event.optString("event")) {
                "up", "moved" -> connected + device
                "detached", "gone" -> connected - device
                else -> return
            }
        }
        refresh()
    }

    /** What is advertised, as things are now: again only if it changed (the
     * quarter of an hour, a PC with a session or without, another network,
     * pairing opened or shut). A tablet whose user holds pairing open says
     * so, in the clear, for the new PC to list it as earphones to be paired
     * are; else only what the PCs it is paired with can read. */
    @Synchronized
    fun refresh() {
        handler.removeCallbacks(rotate)
        val ip = BleBlocks.ipv4(addresses)
        val advertiser = advertiser()
        val opened = pairing()
        // Pairing is done at the app's own listener (LinkServer's: the PC's link.pair()), not at the
        // link's core, which takes only the PCs already paired.
        val at = if (opened) pairingPort() else port
        if (ip == null || at == 0 || advertiser == null) return stop()
        val octets = ip.map { it.toInt() and 0xff }
        val told: Shown
        val data: ByteArray
        var scanResponse: AdvertiseData? = null
        if (opened) {
            val label = name()
            told = Shown(true, 0, emptyList(), octets, at, label)
            data = BleBlocks.pairingData(appFingerprint, ip, at)
            scanResponse = AdvertiseData.Builder()
                .addServiceData(ParcelUuid.fromString(BleBlocks.NAME_UUID), BleBlocks.nameData(label))
                .setIncludeDeviceName(false).setIncludeTxPowerLevel(false).build()
        } else {
            val wanted = (paired() - connected).sorted()
            if (wanted.isEmpty()) return stop()
            // More PCs than one advertisement holds: each in its turn.
            val chosen = if (wanted.size <= BleBlocks.MOST) wanted
                else List(BleBlocks.MOST) { wanted[(rotation * BleBlocks.MOST + it) % wanted.size] }
            if (wanted.size > BleBlocks.MOST) handler.postDelayed(rotate, ROTATE)
            val slot = BleBlocks.slot()
            told = Shown(false, slot, chosen, octets, at, "")
            data = BleBlocks.data(appFingerprint, chosen, ip, at, slot) { random.nextInt(256) }
        }
        if (told == now) return
        stop()
        val settings = AdvertiseSettings.Builder()
            // Pairing is waited for by a user, for two minutes: quicker; the rest, Android's lowest rate.
            .setAdvertiseMode(if (opened) AdvertiseSettings.ADVERTISE_MODE_BALANCED else AdvertiseSettings.ADVERTISE_MODE_LOW_POWER)
            .setTxPowerLevel(AdvertiseSettings.ADVERTISE_TX_POWER_MEDIUM)
            .setConnectable(false)
            .setTimeout(0)
            .build()
        val advertised = AdvertiseData.Builder()
            .addManufacturerData(BleBlocks.COMPANY, data)
            .setIncludeDeviceName(false)
            .setIncludeTxPowerLevel(false)
            .build()
        val started = object : AdvertiseCallback() {
            override fun onStartFailure(errorCode: Int) {
                Log.w(TAG, "ble: not advertising ($errorCode)")
                synchronized(this@BleAnnouncer) { if (callback === this) { callback = null; now = null } }
            }
        }
        try {
            if (scanResponse != null) advertiser.startAdvertising(settings, advertised, scanResponse, started)
            else advertiser.startAdvertising(settings, advertised, started)
            callback = started
            now = told
            Log.i(TAG, if (opened) "ble: advertising, pairing open" else "ble: advertising for ${told.pcs.size} PC(s)")
        } catch (e: Exception) {  // permission taken back, Bluetooth gone off: tried again at the next refresh
            Log.w(TAG, "ble: $e")
        }
    }

    @Synchronized
    fun stop() {
        handler.removeCallbacks(rotate)
        val running = callback ?: return run { now = null }
        callback = null
        now = null
        try { advertiser()?.stopAdvertising(running) } catch (e: Exception) { /* off already */ }
        Log.i(TAG, "ble: not advertising")
    }

    private fun advertiser() =
        if (context.checkSelfPermission(Manifest.permission.BLUETOOTH_ADVERTISE) != PackageManager.PERMISSION_GRANTED) null
        else context.getSystemService(BluetoothManager::class.java)?.adapter?.takeIf { it.isEnabled }?.bluetoothLeAdvertiser

    companion object {
        private const val TAG = LinkCore.TAG
        private const val ROTATE = 8_000L  // ms: each PC's turn, when more are paired than an advertisement holds
    }
}
