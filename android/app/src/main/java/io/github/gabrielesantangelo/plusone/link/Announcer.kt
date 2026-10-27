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
 * TCP listener's port, and in its TXT record the app's id (its
 * certificate's fingerprint, the start), name and version (an older one
 * than the PC's gets its update). */
class MdnsAnnouncer(private val context: Context, private val serviceType: String = SERVICE_TYPE) : Announcer {
    @Volatile private var registration: NsdManager.RegistrationListener? = null
    @Volatile private var stopped = false

    private val nsd get() = context.getSystemService(Context.NSD_SERVICE) as NsdManager

    override fun announce(kind: String, where: Int, id: String, name: String, version: Long) {
        if (kind != "tcp") return
        val info = NsdServiceInfo().apply {
            serviceName = name.take(63)  // a DNS label's most
            serviceType = this@MdnsAnnouncer.serviceType
            port = where
            setAttribute("id", id.take(16))
            // A TXT entry, key and value, is 255 bytes at most.
            setAttribute("name", name.take(80))
            setAttribute("v", version.toString())
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
    }
}
