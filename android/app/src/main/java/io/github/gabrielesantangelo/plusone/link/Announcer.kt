package io.github.gabrielesantangelo.plusone.link

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.util.Log

/** A way the PCs learn the app is there, and where (plusone/link/discovery.py
 * on the PC): mDNS today. */
interface Announcer {
    /** The app (its id, name and version) is reached by a listener of kind at where. */
    fun announce(kind: String, where: Int, id: String, name: String, version: Long)
    fun stop()
}

/** mDNS (DNS-SD, _plusone._tcp; the link's core, _plusone-link._tcp): the
 * TCP listener's port under a name of no one ("Plus One" and a few letters
 * new each start), and in its TXT record a tag for each paired PC ("t",
 * Tags), which only that PC recognises, a quarter of an hour at a time.
 * While the user holds pairing open (pairing()), the app's id (its
 * certificate's fingerprint, the start) and name too, for a new PC to list
 * it; never its version (the PC learns that over the link, authenticated). */
class MdnsAnnouncer(private val context: Context, private val serviceType: String = SERVICE_TYPE,
                    private val pairing: () -> Boolean = { false }) : Announcer {
    @Volatile private var registration: NsdManager.RegistrationListener? = null
    @Volatile private var stopped = false
    @Volatile private var last: Pair<Int, Pair<String, String>>? = null  // where, (id, name): to announce again

    private val nsd get() = context.getSystemService(Context.NSD_SERVICE) as NsdManager

    override fun announce(kind: String, where: Int, id: String, name: String, version: Long) {
        if (kind != "tcp") return
        last = where to (id to name)
        register()
    }

    /** Again, as things are now: the quarter of an hour turned, a PC was
     * paired or forgotten, pairing opened or shut. */
    fun refresh() {
        if (last != null) register()
    }

    private fun register() {
        val (where, who) = last ?: return
        val (id, name) = who
        val open = pairing()
        val info = NsdServiceInfo().apply {
            serviceName = "Plus One $SUFFIX"
            serviceType = this@MdnsAnnouncer.serviceType
            port = where
            setAttribute("t", Tags.announced(id, Paired.all(context).keys))
            if (open) {
                // A TXT entry, key and value, is 255 bytes at most.
                setAttribute("pair", "1")
                setAttribute("id", id.take(16))
                setAttribute("name", name.take(80))
            }
        }
        val listener = object : NsdManager.RegistrationListener {
            override fun onServiceRegistered(info: NsdServiceInfo) {
                Log.i(TAG, "on the network as ${info.serviceName}, port $where")
            }
            override fun onRegistrationFailed(info: NsdServiceInfo, error: Int) {
                Log.e(TAG, "mDNS registration failed: $error")
                synchronized(this@MdnsAnnouncer) { if (registration === this) registration = null }
            }
            override fun onServiceUnregistered(info: NsdServiceInfo) {}
            override fun onUnregistrationFailed(info: NsdServiceInfo, error: Int) {}
        }
        synchronized(this) {
            if (stopped) return
            registration?.let { runCatching { nsd.unregisterService(it) } }
            registration = listener
            nsd.registerService(info, NsdManager.PROTOCOL_DNS_SD, listener)
        }
    }

    override fun stop() = synchronized(this) {
        stopped = true
        registration?.let { runCatching { nsd.unregisterService(it) } }
        registration = null
    }

    companion object {
        const val SERVICE_TYPE = "_plusone._tcp"
        /** The name's few letters: new each time the app starts, so it follows no one. */
        private val SUFFIX = "%04x".format(java.security.SecureRandom().nextInt(0x10000))
    }
}
