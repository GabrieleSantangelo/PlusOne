package io.github.gabrielesantangelo.plusone.link

import android.content.Context
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.util.Log
import java.net.Inet4Address
import java.net.InetAddress

/** The addresses of the networks the tablet is on (Wi-Fi, Ethernet; never the
 * mobile network nor a VPN), told to the link's core as they change. The core
 * tells them to every PC with a session, over its cable as well: a PC that
 * has the Wi-Fi's address while the cable is in dials it when the cable is
 * out, where multicast (mDNS) does not go. */
class NetworkWatch(context: Context, private val core: LinkCore, private val also: (List<String>) -> Unit = {}) {
    private val connectivity = context.getSystemService(ConnectivityManager::class.java)
    private val known = LinkedHashMap<Network, List<String>>()
    private val callback = object : ConnectivityManager.NetworkCallback() {
        override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) =
            changed(network, properties.linkAddresses.map { it.address to it.flags })

        override fun onLost(network: Network) = changed(network, null)
    }

    fun start() {
        val request = NetworkRequest.Builder()
            .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
            .addTransportType(NetworkCapabilities.TRANSPORT_ETHERNET)
            .build()
        try {
            connectivity.registerNetworkCallback(request, callback)
        } catch (e: Exception) {  // no permission, say: the PC finds the tablet as it did
            Log.w(LinkCore.TAG, "link: the networks not watched: $e")
        }
    }

    fun stop() {
        try { connectivity.unregisterNetworkCallback(callback) } catch (e: Exception) { /* not registered */ }
    }

    private fun changed(network: Network, addresses: List<Pair<InetAddress, Int>>?) {
        val told = synchronized(known) {
            if (addresses == null) known.remove(network) else known[network] = usable(addresses)
            order(known.values.flatten())
        }
        try { core.setAddresses(told) } catch (e: Exception) { Log.w(LinkCore.TAG, "link: addresses: $e") }
        also(told)
    }

    companion object {
        /** What a PC may dial: IPv4 only (the link listens on 0.0.0.0: an IPv6 address
         * was told and refused on the first live try), not loopback or link-local. */
        fun usable(addresses: List<Pair<InetAddress, Int>>): List<String> = addresses.filter { (address, _) ->
            address is Inet4Address &&
                !(address.isLoopbackAddress || address.isLinkLocalAddress || address.isAnyLocalAddress || address.isMulticastAddress)
        }.mapNotNull { (address, _) -> address.hostAddress }

        /** Each once, IPv4 first. */
        fun order(addresses: List<String>): List<String> {
            val unique = addresses.distinct()
            return unique.filter { !it.contains(':') } + unique.filter { it.contains(':') }
        }
    }
}
