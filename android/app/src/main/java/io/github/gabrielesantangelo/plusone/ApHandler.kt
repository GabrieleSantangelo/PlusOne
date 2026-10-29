package io.github.gabrielesantangelo.plusone

import android.Manifest
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.wifi.WifiManager
import android.net.wifi.WifiNetworkSpecifier
import android.util.Log
import io.github.gabrielesantangelo.plusone.link.Handler
import io.github.gabrielesantangelo.plusone.link.Link
import org.json.JSONObject
import java.net.Inet4Address
import java.util.concurrent.CompletableFuture
import java.util.concurrent.TimeUnit

/**
 * The PC's own access point (its second adapter's): a path of the link's
 * own, "ap", once through the air and no one else's queue, where Wi-Fi
 * Direct is not to be had (plusone/link/ap.py). Joined as a local-only
 * network alongside the Wi-Fi the tablet is on (STA concurrency, Android
 * 12 on, where the hardware has it): its internet and its calls stay where
 * they are. Without that, only when the PC says "exclusive" (the user chose
 * a direct link over the tablet's own network): the tablet's Wi-Fi moves to
 * the PC's network for as long as it lasts, and Android takes it back to its
 * own after. Android asks the user the first time; the network lasts while
 * the feature's channel does.
 */
class ApHandler(private val service: CompanionService) : Handler {
    override val types = setOf("ap")

    override fun handle(link: Link, request: JSONObject, size: Long): Boolean {
        val ssid = request.optString("ssid")
        val passphrase = request.optString("passphrase")
        require(ssid.isNotEmpty() && passphrase.length >= 8) { "bad ap request" }
        val wifi = service.getSystemService(WifiManager::class.java)
        val alongside = wifi?.isStaConcurrencyForLocalOnlyConnectionsSupported == true
        if (wifi == null || (!alongside && !request.optBoolean("exclusive"))) {
            link.refuse("unsupported")
            return false
        }
        if (service.checkSelfPermission(Manifest.permission.NEARBY_WIFI_DEVICES) != PackageManager.PERMISSION_GRANTED) {
            link.refuse("permission")
            return false
        }
        val connectivity = service.getSystemService(ConnectivityManager::class.java)
        val specifier = WifiNetworkSpecifier.Builder().setSsid(ssid).setWpa2Passphrase(passphrase).build()
        val wanted = NetworkRequest.Builder()
            .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
            .removeCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)  // local only: not instead of the tablet's Wi-Fi
            .setNetworkSpecifier(specifier)
            .build()
        val joined = CompletableFuture<Network?>()
        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) { joined.complete(network) }
            override fun onUnavailable() { joined.complete(null) }
        }
        try {
            connectivity.requestNetwork(wanted, callback, JOIN.toInt())
        } catch (e: RuntimeException) {  // SecurityException, a request refused: said why, not a channel just closed
            Log.w(CompanionService.TAG, "ap: $e")
            link.refuse("not allowed")
            return false
        }
        val session = Any()
        var bound = false
        try {
            val network = try { joined.get(JOIN + 2_000, TimeUnit.MILLISECONDS) } catch (e: Exception) { null }
            if (network == null) {
                link.refuse("not joined")
                return false
            }
            // Its own Wi-Fi left, the PC's network is the only one, but not
            // the default: what the app opens goes there (the link's UDP
            // socket among it, else realtime fell back on its TCP).
            // The binding this session's from now on: the one before, ending,
            // leaves it be.
            if (!alongside) bound = process.take(session) { connectivity.bindProcessToNetwork(network) }
            val address = connectivity.getLinkProperties(network)?.linkAddresses
                ?.map { it.address }?.firstOrNull { it is Inet4Address }?.hostAddress
            Log.i(CompanionService.TAG, "ap: on $ssid, at $address${if (alongside) "" else ", its own Wi-Fi left"}")
            // (Exclusive: the link this goes on was the Wi-Fi left, so it may
            // only come once the PC found the tablet on its network itself.)
            link.send(JSONObject().put("type", "ap-up").put("address", address.orEmpty()))
            link.hold()
        } finally {
            if (bound) process.letGo(session) { connectivity.bindProcessToNetwork(null) }
            connectivity.unregisterNetworkCallback(callback)
        }
        return false
    }

    companion object {
        /** The network the process is bound to: one at a time, whichever
         * session bound it; unbound as it ends. */
        private val process = Newest()
        const val JOIN = 85_000L  // ms: Android's question to the user, the first time, included
    }
}
