package io.github.gabrielesantangelo.plusone

import android.Manifest
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.provider.Settings
import android.util.Log
import io.github.gabrielesantangelo.plusone.link.Link
import io.github.gabrielesantangelo.plusone.link.LinkServer
import io.github.gabrielesantangelo.plusone.link.MdnsAnnouncer
import io.github.gabrielesantangelo.plusone.link.TcpListener
import java.io.File
import java.util.concurrent.CompletableFuture
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

/**
 * Always ready for the PC: the app's services on the paired PCs' links
 * (link.LinkServer: listeners, announcers, pairing): a PDF to sign (SignHandler),
 * the desktop (DisplayHandler), the speakers (SpeakersHandler), the camera
 * and the microphone (CaptureHandler), the app's update (UpdateHandler). A
 * foreground service (Android shows it as a quiet notification, which says
 * what the PC uses); it does nothing until a PC connects.
 */
class CompanionService : Service(), LinkServer.Host {
    private var server: LinkServer? = null
    /** The link's core (docs/LINK.md), beside the link of before while the
     * features move over: the PCs' plus-one-link connect to it. */
    var link: io.github.gabrielesantangelo.plusone.link.LinkCore? = null
        private set
    private var linkAnnouncer: MdnsAnnouncer? = null
    private val pairedChanged = android.content.SharedPreferences.OnSharedPreferenceChangeListener { _, _ ->
        link?.setPaired(io.github.gabrielesantangelo.plusone.link.Paired.all(this).keys)
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        instance = this
        Notifications.channels(this)
        try {
            startForeground(Notifications.SERVICE_ID, Notifications.ready(this),
                ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
        } catch (e: Exception) {
            // Restarted from the background (START_STICKY) where Android does
            // not allow it: the next time the app opens starts it again.
            Log.w(TAG, "foreground service not allowed now: $e")
            stopSelf()
            return
        }
        // PDFs of jobs this process no longer has (it was ended meanwhile).
        File(cacheDir, "jobs").listFiles()?.forEach { if (it.nameWithoutExtension !in Jobs.ids) it.delete() }
        Jobs.listen(queueChanged)
        queueChanged()  // nor a notification for them
        // One for both links: the room it keeps for PDFs counts uploads on
        // either, and an update comes once whichever it comes on.
        val sign = SignHandler(this)
        val update = UpdateHandler(this)
        server = LinkServer(this,
            handlers = listOf(sign, DisplayHandler(this), SpeakersHandler(this),
                CaptureHandler(this), update),
            host = this,
            // How the PCs reach the app, and learn where: another way is one more here.
            listeners = listOf(TcpListener()),
            announcers = listOf(MdnsAnnouncer(this))).also { it.start() }
        startLink(sign, update)
        handler.postDelayed(::idleCheck, IDLE_CHECK)
    }

    /** The link's core: listening, announced, the paired PCs trusted; "echo"
     * served (every message back: the link measured from the PC). */
    private fun startLink(sign: SignHandler, update: UpdateHandler) {
        val core = try {
            io.github.gabrielesantangelo.plusone.link.LinkCore(deviceName(this),
                io.github.gabrielesantangelo.plusone.link.Identity.certificate,
                io.github.gabrielesantangelo.plusone.link.Identity.privateKey)
        } catch (e: Throwable) {  // UnsatisfiedLinkError among them: the old link goes on alone
            Log.w(TAG, "link: not started: $e")
            return
        }
        link = core
        core.onEvent = { io.github.gabrielesantangelo.plusone.link.LinkStatus.event(it) }
        core.setPaired(io.github.gabrielesantangelo.plusone.link.Paired.all(this).keys)
        // A cable plugged in before the service was up, or while it was not.
        (pendingCable ?: getSystemService(android.hardware.usb.UsbManager::class.java).accessoryList
            ?.firstOrNull { it.manufacturer == "Plus One" })?.let { pendingCable = null; openCable(it) }
        getSharedPreferences("paired", MODE_PRIVATE).registerOnSharedPreferenceChangeListener(pairedChanged)
        core.serve("echo") { channel ->
            channel.use {
                val buffer = ByteArray(256 shl 10)
                while (true) {
                    val length = channel.receive(buffer)
                    if (length <= 0) break
                    channel.send(buffer, 0, length)
                }
            }
        }
        // The features moved over so far: their handlers of the link of before.
        LinkFeatures(this, core, listOf(DisplayHandler(this), SpeakersHandler(this), CaptureHandler(this), sign, update))
        Thread({
            try {
                // The same port each time, if free: where the PC reached it
                // last still holds (no mDNS needed, as on eduroam).
                val port = try { core.listen(LINK_PORT) } catch (e: java.io.IOException) { core.listen() }
                val id = io.github.gabrielesantangelo.plusone.link.Identity.fingerprint(
                    io.github.gabrielesantangelo.plusone.link.Identity.certificate.encoded)
                linkAnnouncer = MdnsAnnouncer(this, io.github.gabrielesantangelo.plusone.link.LinkCore.SERVICE_TYPE)
                    .also { it.announce("tcp", port, id, deviceName(this), appVersion()) }
                Log.i(TAG, "link: listening on $port")
            } catch (e: Exception) {
                Log.w(TAG, "link: cannot listen: $e")
            }
        }, "link-start").start()
    }

    /** The accessory, opened once, to the link's core: it owns it until the
     * cable is out (sessions on it one after the other; Android's accessory
     * opens only once). */
    fun openCable(accessory: android.hardware.usb.UsbAccessory) {
        val core = link ?: return
        val usb = getSystemService(android.hardware.usb.UsbManager::class.java)
        if (!usb.hasPermission(accessory)) return  // the user said no to "open Plus One"
        Thread({
            val fd = try { usb.openAccessory(accessory)?.detachFd() } catch (e: Exception) { null }
            if (fd == null) {
                Log.i(TAG, "link: the cable's accessory did not open (it has its owner already)")
                return@Thread
            }
            try {
                core.accept(fd, "usb")
                Log.i(TAG, "link: the cable is the link's")
            } catch (e: Exception) {
                Log.w(TAG, "link: by cable: $e")
            }
        }, "link-cable").start()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        lastUse = System.currentTimeMillis()  // the app was opened, or a job came
        return if (Options.alwaysReady(this)) START_STICKY else START_NOT_STICKY
    }

    override fun onDestroy() {
        instance = null
        Jobs.unlisten(queueChanged)
        handler.removeCallbacksAndMessages(null)
        server?.stop()
        getSharedPreferences("paired", MODE_PRIVATE).unregisterOnSharedPreferenceChangeListener(pairedChanged)
        linkAnnouncer?.stop()
        link?.close()
        super.onDestroy()
    }

    // -- LinkServer.Host ----------------------------------------------------------
    override fun deviceName(): String = deviceName(this)

    override fun appVersion(): Long = Updates.versionCode(this)

    override fun askPairing(pc: String, code: String, from: String): Boolean? = Pairing.ask(this, pc, code, from)

    override fun pairingWithdrawn(code: String) = Pairing.answer(code, false)

    override fun connected(delta: Int) {
        busy.addAndGet(delta)
        lastUse = System.currentTimeMillis()
    }

    /** The user forgot this PC: what it has open ends now. */
    fun disconnect(fingerprint: String) {
        server?.disconnect(fingerprint)
    }

    // Not always ready: stop once nothing has happened for a while.
    private val handler = android.os.Handler(android.os.Looper.getMainLooper())
    @Volatile private var lastUse = System.currentTimeMillis()
    private val busy = AtomicInteger()  // PCs connected

    private fun idleCheck() {
        // PDFs whose PC stopped looking for the tablet.
        for (job in Jobs.expire()) if (job.ownsFile) job.file.delete()
        // A PC using the link's core (a channel open) is as busy as a link of before.
        val linkInUse = try { link?.devices()?.any { it.optInt("channels") > 0 } == true } catch (e: Exception) { false }
        if (!Options.alwaysReady(this) && busy.get() == 0 && !linkInUse && Jobs.ids.isEmpty() &&
            System.currentTimeMillis() - lastUse > IDLE_STOP) {
            stopSelf()
            return
        }
        handler.postDelayed(::idleCheck, IDLE_CHECK)
    }

    /** The notification for what waits to be signed: it alerts when the
     * screen cannot open by itself, and only for the first PDF of a batch. */
    private val queueChanged: () -> Unit = {
        Notifications.waiting(this, Jobs.waiting(), alert = !Settings.canDrawOverlays(this))
    }

    // -- what the PCs use now ------------------------------------------------------
    /** A session a PC holds: what (camera, microphone, speakers), on which link. */
    class Session(val kind: String, val link: Link)

    private val sessions = mutableMapOf<Any, Session>()  // by its owner (a capture, a playback)

    /** A session begins (the PC is told it may use it after: showSessions,
     * lowLatency); ended() whatever becomes of it. */
    fun began(owner: Any, session: Session) {
        synchronized(sessions) { sessions[owner] = session }
    }

    fun ended(owner: Any) {
        synchronized(sessions) { sessions.remove(owner) }
        showSessions()
        lowLatency(false)
    }

    /** The PC whose desktop is shown here (DisplayHandler), or null. */
    @Volatile var displaying: String? = null

    /** Whether the PC uses the tablet now: an update would end it. */
    fun busy(): Boolean = synchronized(sessions) { sessions.isNotEmpty() } || displaying != null ||
        Jobs.waiting().isNotEmpty()

    /** The user took the device back from the PC (MainActivity): its
     * sessions end, and the service lets go of it. */
    fun stopCaptures(kind: String) {
        val links = synchronized(sessions) { sessions.values.filter { it.kind == kind }.map { it.link } }
        links.forEach { it.close() }
        if (kind != "speakers" && allowedTypes(kind)) {
            types = types and typeOf(kind).inv()
            showForeground()
        }
    }

    // -- the foreground service ------------------------------------------------------
    // Its types now: camera and microphone stay once they were taken in
    // front, so the PC's next use needs no CaptureActivity.
    @Volatile private var types = ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE
    private var elevating: CompletableFuture<Boolean>? = null  // under sessions

    fun allowedTypes(kind: String) = types and typeOf(kind) != 0

    private fun typeOf(kind: String) = if (kind == "camera") ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA
        else ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE

    /** Camera and microphone are only for a foreground service started, or
     * started again, while the app is in front: for a moment a see-through
     * activity is (CaptureActivity, which may open over other apps). Once:
     * the types stay while the service runs and the user lets the PC have them. */
    fun foreground(kind: String): Boolean {
        // CaptureActivity can open only over other apps (or the app's own
        // screen), and not over the lock screen: else no use trying.
        val locked = getSystemService(android.app.KeyguardManager::class.java).isKeyguardLocked
        if (locked || !(Settings.canDrawOverlays(this) || Front.shown)) return false
        // Camera and microphone asked at once: one activity, one wait.
        val done = synchronized(sessions) {
            elevating?.takeIf { !it.isDone } ?: CompletableFuture<Boolean>().also {
                elevating = it
                startActivity(Intent(this, CaptureActivity::class.java)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_NO_ANIMATION))
            }
        }
        try {
            done.get(ELEVATE_WAIT, TimeUnit.MILLISECONDS)
        } catch (e: Exception) {
            Log.w(TAG, "could not come to the front for the camera or microphone: $e")
        }
        return allowedTypes(kind)
    }

    /** While the app is in front (CaptureActivity, MainActivity): the service
     * takes the camera and the microphone the user lets the PC have. */
    fun elevate() {
        var wanted = ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE
        for ((kind, on) in listOf("camera" to Options.camera(this), "microphone" to Options.microphone(this))) {
            val permission = if (kind == "camera") Manifest.permission.CAMERA else Manifest.permission.RECORD_AUDIO
            if (on && checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED) wanted = wanted or typeOf(kind)
        }
        if (wanted != types) {
            val before = types
            types = wanted
            if (!showForeground()) types = before  // what the service really has
        }
        synchronized(sessions) { elevating?.complete(true) }
    }

    /** The foreground service with its types, and its notification as the
     * sessions are now: whether Android took it. */
    private fun showForeground(): Boolean = try {
        startForeground(Notifications.SERVICE_ID, notification(), types)
        true
    } catch (e: Exception) {
        Log.w(TAG, "foreground service: $e")
        false
    }

    private fun notification(): android.app.Notification {
        val now = synchronized(sessions) { sessions.values.toList() }
        val kinds = now.map { it.kind }.toSet()
        val pcs = now.map { it.link.pc }.distinct().joinToString(", ")
        displaying?.let { return Notifications.displaying(this, it) }
        return if (kinds.isEmpty()) Notifications.ready(this) else Notifications.capturing(this, kinds, pcs)
    }

    /** What the notification says, when only that changed. */
    fun showSessions() =
        getSystemService(android.app.NotificationManager::class.java).notify(Notifications.SERVICE_ID, notification())

    // Wi-Fi without power saving while the PC uses the camera, the microphone or
    // the speakers, or its desktop is on screen: saving, it holds packets back
    // and sends them in bunches every 100-300 ms.
    private val wifiLock by lazy {
        getSystemService(android.net.wifi.WifiManager::class.java)
            .createWifiLock(android.net.wifi.WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "plusone:capture")
            .apply { setReferenceCounted(false) }
    }

    @Synchronized  // from the main thread (onVisible) and the links': in order
    fun lowLatency(using: Boolean) {
        val any = synchronized(sessions) { sessions.isNotEmpty() }
        try {
            val showing = displaying != null && DisplaySession.current?.visible == true
            if (using || any || showing) wifiLock.acquire() else wifiLock.release()
        } catch (e: Exception) {
            Log.w(TAG, "Wi-Fi lock: $e")
        }
    }

    companion object {
        /** The link's core listens here, if it can. */
        const val LINK_PORT = 47101
        const val TAG = io.github.gabrielesantangelo.plusone.link.TAG
        const val ELEVATE_WAIT = 5_000L        // ms for CaptureActivity to bring the app in front
        const val IDLE_CHECK = 60_000L         // ms
        const val IDLE_STOP = 10 * 60_000L     // ms without a PC when not always ready

        fun deviceName(context: Context): String =
            Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME) ?: Build.MODEL

        /** The running service, for the activities that talk to it. */
        @Volatile var instance: CompanionService? = null

        fun start(context: Context) {
            context.startForegroundService(Intent(context, CompanionService::class.java))
        }

        /** A PC's accessory, by cable (UsbActivity): to the link's core once
         * the service is up. */
        @Volatile private var pendingCable: android.hardware.usb.UsbAccessory? = null

        fun cable(context: Context, accessory: android.hardware.usb.UsbAccessory) {
            val service = instance
            if (service?.link != null) service.openCable(accessory) else {
                pendingCable = accessory
                start(context)
            }
        }
    }
}
