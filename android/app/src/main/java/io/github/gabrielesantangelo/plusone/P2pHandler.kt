package io.github.gabrielesantangelo.plusone

import android.Manifest
import android.content.pm.PackageManager
import android.net.MacAddress
import android.net.wifi.WpsInfo
import android.net.wifi.p2p.WifiP2pConfig
import android.net.wifi.p2p.WifiP2pDevice
import android.net.wifi.p2p.WifiP2pManager
import android.os.Looper
import android.util.Log
import io.github.gabrielesantangelo.plusone.link.Handler
import io.github.gabrielesantangelo.plusone.link.Link
import org.json.JSONObject
import java.util.concurrent.CompletableFuture
import java.util.concurrent.TimeUnit

/**
 * A Wi-Fi Direct group with the PC: a path of the link's own, "p2p" (each
 * packet once through the air, not twice through an access point or a
 * phone's hotspot), served over the link (plusone/link/p2p.py). The PC, looking
 * for peers already, asks with its P2P address (and its name); the app finds it, connects to it as the group owner (it hands the
 * PC an address, 192.168.49.x, as Android's groups do) and answers with its
 * name on Wi-Fi Direct (its address Android keeps from apps: 02:00:00:00:00:00);
 * the PC starts connecting, then the app: both sides asked, neither shows a
 * question. The group lasts as long as this link.
 */
class P2pHandler(private val service: CompanionService) : Handler {
    override val types = setOf("p2p")

    override fun handle(link: Link, request: JSONObject, size: Long): Boolean {
        val name = request.optString("name")
        val pcAddress = request.optString("address").lowercase()
        require(name.isNotEmpty() || pcAddress.isNotEmpty()) { "bad p2p request" }
        if (service.checkSelfPermission(Manifest.permission.NEARBY_WIFI_DEVICES) != PackageManager.PERMISSION_GRANTED) {
            link.refuse("permission")
            return false
        }
        val manager = service.getSystemService(WifiP2pManager::class.java)
        if (manager == null) {
            link.refuse("unavailable")
            return false
        }
        val channel = manager.initialize(service, Looper.getMainLooper(), null)
        try {
            val own = CompletableFuture<WifiP2pDevice?>()
            manager.requestDeviceInfo(channel) { own.complete(it) }
            val me = own.get(WAIT, TimeUnit.MILLISECONDS)
            // A group or a connection of before (another session's): gone, so
            // this one starts clean (else Android answers "busy").
            done { manager.cancelConnect(channel, it) }
            done { manager.removeGroup(channel, it) }
            done { manager.discoverPeers(channel, it) }
            val seen = mutableListOf<WifiP2pDevice>()
            val pc = find(manager, channel, pcAddress, name, seen)
            if (pc == null) {
                // What was seen instead, for the PC to say why.
                val peers = seen.joinToString(", ") { "${it.deviceName} (${it.deviceAddress})" }
                Log.i(CompanionService.TAG, "p2p: $name ($pcAddress) not seen; seen: $peers")
                link.send(JSONObject().put("type", "refused").put("reason", "unavailable").put("seen", peers))
                return false
            }
            // Not persistent: Android would otherwise invite the PC back into a
            // group it remembers (Second Screen's, say), which the PC does not
            // know and ignores; and it would remember this one for next time.
            // Found: the PC is told who to connect to, and starts first (its
            // NetworkManager then takes the app's negotiation). Android's own
            // way of letting a peer in (authorizeConnectionFromPeer) left this
            // Samsung turning the PC's negotiation away.
            link.send(JSONObject().put("type", "answer").put("p2p", me?.deviceAddress.orEmpty())
                .put("name", me?.deviceName.orEmpty()))
            if (link.receive().optString("type") != "p2p-go") return false  // the PC's attempt under way
            // Not remembered: a group Android remembers it offers back to the
            // PC (an invitation) instead of negotiating, and the PC knows none.
            // The group on 5 GHz: on the PC's own channel (Wi-Fi and Wi-Fi
            // Direct at once) Intel's AX211 firmware (89, the latest) crashes.
            val config = WifiP2pConfig.Builder().setDeviceAddress(MacAddress.fromString(pc.deviceAddress))
                .setGroupOperatingBand(WifiP2pConfig.GROUP_OWNER_BAND_5GHZ)
                .enablePersistentMode(false).build().apply {
                    wps.setup = WpsInfo.PBC
                    groupOwnerIntent = WifiP2pConfig.GROUP_OWNER_INTENT_MAX  // the tablet the owner: its DHCP for the PC
                }
            var reason = done { manager.connect(channel, config, it) }
            if (reason == WifiP2pManager.BUSY) {
                Thread.sleep(BUSY_AGAIN)  // the last session still letting go
                reason = done { manager.connect(channel, config, it) }
            }
            if (reason != 0) {
                Log.i(CompanionService.TAG, "p2p: connect failed ($reason)")
                link.refuse("unavailable")
                return false
            }
            Log.i(CompanionService.TAG, "p2p: connecting to ${pc.deviceName} (${pc.deviceAddress}), as ${me?.deviceName}")
            // Once the group is up: where the PC reaches the app in it.
            val group = joined(manager, channel)
            Log.i(CompanionService.TAG, "p2p: group ${group ?: "not up"}")
            link.send(JSONObject().put("type", "p2p-group").put("address", group?.second.orEmpty())
                .put("owner", group?.first ?: false))
            link.hold()
        } finally {
            manager.removeGroup(channel, null)
            manager.cancelConnect(channel, null)
            manager.stopPeerDiscovery(channel, null)
            channel.close()
        }
        return false
    }

    /** The PC among the peers, by its name, as it shows up (NEARBY_WIFI_DEVICES
     * checked by handle(); taken back meanwhile, the link just ends). */
    @android.annotation.SuppressLint("MissingPermission")
    private fun find(manager: WifiP2pManager, channel: WifiP2pManager.Channel, address: String, name: String,
                     seen: MutableList<WifiP2pDevice>): WifiP2pDevice? {
        val until = System.currentTimeMillis() + FIND
        while (System.currentTimeMillis() < until) {
            val peers = CompletableFuture<List<WifiP2pDevice>>()
            manager.requestPeers(channel) { peers.complete(it?.deviceList?.toList().orEmpty()) }
            val list = peers.get(WAIT, TimeUnit.MILLISECONDS)
            seen.clear()
            seen.addAll(list)
            list.firstOrNull { address.isNotEmpty() && it.deviceAddress.equals(address, ignoreCase = true) }
                ?.let { return it }
            list.firstOrNull { name.isNotEmpty() && it.deviceName == name }?.let { return it }
            Thread.sleep(500)
        }
        return null
    }

    /** (whether the tablet owns it, its address in it) once the group is up, or null. */
    private fun joined(manager: WifiP2pManager, channel: WifiP2pManager.Channel): Pair<Boolean, String>? {
        val until = System.currentTimeMillis() + GROUP
        while (System.currentTimeMillis() < until) {
            val info = CompletableFuture<android.net.wifi.p2p.WifiP2pInfo?>()
            manager.requestConnectionInfo(channel) { info.complete(it) }
            val got = info.get(WAIT, TimeUnit.MILLISECONDS)
            if (got != null && got.groupFormed) {
                if (got.isGroupOwner) return true to (got.groupOwnerAddress?.hostAddress ?: "192.168.49.1")
                // A client: its own address on the group's interface (p2p-*), once DHCP gave it one.
                val own = java.net.NetworkInterface.getNetworkInterfaces()?.toList().orEmpty()
                    .filter { it.name.startsWith("p2p") }
                    .flatMap { it.inetAddresses.toList() }
                    .firstOrNull { it is java.net.Inet4Address }
                if (own != null) return false to own.hostAddress.orEmpty()
            }
            Thread.sleep(250)
        }
        return null
    }

    /** An action of the manager's, its outcome: 0, or its failure's reason. */
    private fun done(action: (WifiP2pManager.ActionListener) -> Unit): Int {
        val result = CompletableFuture<Int>()
        action(object : WifiP2pManager.ActionListener {
            override fun onSuccess() { result.complete(0) }
            override fun onFailure(reason: Int) { result.complete(reason) }
        })
        return result.get(WAIT, TimeUnit.MILLISECONDS)
    }

    companion object {
        const val WAIT = 5_000L   // ms for the system to answer
        const val FIND = 20_000L  // ms to see the PC
        const val GROUP = 60_000L // ms for the group to be up (the PC's NetworkManager tries for 55 s)
        const val BUSY_AGAIN = 1_000L  // ms before asking again, Android busy with the last session
    }
}
