package io.github.gabrielesantangelo.plusone

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent

object Notifications {
    private const val SERVICE = "service"
    private const val ASK = "ask"
    private const val QUEUE = "queue"
    const val SERVICE_ID = 1

    fun channels(context: Context) {
        val manager = context.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(NotificationChannel(SERVICE,
            context.getString(R.string.channel_service), NotificationManager.IMPORTANCE_MIN))
        manager.createNotificationChannel(NotificationChannel(ASK,
            context.getString(R.string.channel_ask), NotificationManager.IMPORTANCE_HIGH))
        manager.createNotificationChannel(NotificationChannel(QUEUE,
            context.getString(R.string.channel_queue), NotificationManager.IMPORTANCE_LOW))
    }

    fun ready(context: Context): Notification =
        Notification.Builder(context, SERVICE)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(R.string.ready_title))
            .setContentText(context.getString(R.string.ready_text))
            .setContentIntent(PendingIntent.getActivity(context, 0, Intent(context, MainActivity::class.java),
                PendingIntent.FLAG_IMMUTABLE))
            .setOngoing(true)
            .build()

    /** The service's notification while the PC uses the camera, the microphone or the speakers. */
    fun capturing(context: Context, kinds: Set<String>, pc: String): Notification =
        Notification.Builder(context, SERVICE)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString((kinds - "speakers").let { devices ->
                when {
                    devices.size > 1 -> R.string.capturing_both
                    "camera" in devices -> R.string.capturing_camera
                    "microphone" in devices -> R.string.capturing_microphone
                    else -> R.string.capturing_speakers
                }
            }))
            .setContentText(context.getString(R.string.capturing_for, pc))
            .setContentIntent(PendingIntent.getActivity(context, 0, Intent(context, MainActivity::class.java),
                PendingIntent.FLAG_IMMUTABLE))
            .setOngoing(true)
            .build()

    /** The service's notification while a PC's desktop is shown here: back
     * to it with a tap, or Stop. */
    fun displaying(context: Context, pc: String): Notification {
        val stop = PendingIntent.getBroadcast(context, 0,
            Intent(context, DisplayStopReceiver::class.java), PendingIntent.FLAG_IMMUTABLE)
        return Notification.Builder(context, SERVICE)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(R.string.display_title, pc))
            .setContentText(context.getString(R.string.display_text))
            .setContentIntent(PendingIntent.getActivity(context, 0, Intent(context, DisplayActivity::class.java),
                PendingIntent.FLAG_IMMUTABLE))
            .addAction(Notification.Action.Builder(null, context.getString(R.string.display_stop), stop).build())
            .setOngoing(true)
            .build()
    }

    /** One per code (several PCs may pair at once); 1000 and up, clear of SERVICE_ID. */
    private fun pairingId(code: String) = 1000 + code.toInt()

    fun pairing(context: Context, pc: String, code: String, open: Intent) {
        fun action(action: String) = PendingIntent.getBroadcast(context, pairingId(code),
            Intent(context, PairingReceiver::class.java).setAction(action).putExtra(PairActivity.EXTRA_CODE, code),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        val notification = Notification.Builder(context, ASK)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(R.string.pair_title, pc))
            .setContentText(context.getString(R.string.pair_message, code.chunked(3).joinToString(" ")))
            .setContentIntent(PendingIntent.getActivity(context, pairingId(code), open,
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT))
            // Not from the lock screen: whoever holds the tablet, locked, does not pair it.
            .addAction(Notification.Action.Builder(null, context.getString(R.string.pair),
                action(PairingReceiver.ACTION_ACCEPT)).setAuthenticationRequired(true).build())
            .setVisibility(Notification.VISIBILITY_PRIVATE)
            .addAction(Notification.Action.Builder(null, context.getString(R.string.cancel),
                action(PairingReceiver.ACTION_REJECT)).build())
            .setAutoCancel(true)
            .build()
        context.getSystemService(NotificationManager::class.java).notify(pairingId(code), notification)
    }

    /** A PC wants the camera or the microphone, which the user did not let
     * PCs have (or Android did not): Allow, from where the user is. */
    fun askDevice(context: Context, kind: String, pc: String, fingerprint: String) {
        val allow = PendingIntent.getActivity(context, deviceId(kind),
            Intent(context, AllowActivity::class.java).putExtra(AllowActivity.EXTRA_KIND, kind)
                .putExtra(AllowActivity.EXTRA_PC, fingerprint)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        val camera = kind == MainActivity.CAMERA
        val notification = Notification.Builder(context, ASK)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(if (camera) R.string.ask_camera else R.string.ask_microphone, pc))
            .setContentText(context.getString(R.string.ask_device_text))
            .setContentIntent(allow)
            // Not from the lock screen: whoever holds the tablet, locked, does not turn it on.
            .addAction(Notification.Action.Builder(null, context.getString(R.string.overlay_allow), allow)
                .setAuthenticationRequired(true).build())
            .setVisibility(Notification.VISIBILITY_PRIVATE)
            // A group of its own: grouped with the service's, a tap opened the
            // group instead of allowing.
            .setGroup("ask-$kind")
            .setAutoCancel(true)
            .build()
        context.getSystemService(NotificationManager::class.java).notify(deviceId(kind), notification)
    }

    fun cancelDevice(context: Context, kind: String) =
        context.getSystemService(NotificationManager::class.java).cancel(deviceId(kind))

    /** Each time a PC turns the camera or the microphone on: said at the
     * top of the screen, with Stop (CC-04; the service's own notification
     * stays quiet). */
    fun inUse(context: Context, kind: String, pc: String) {
        val stop = PendingIntent.getBroadcast(context, inUseId(kind),
            Intent(context, CaptureStopReceiver::class.java).putExtra(CaptureStopReceiver.EXTRA_KIND, kind),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        val camera = kind == MainActivity.CAMERA
        val notification = Notification.Builder(context, ASK)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(if (camera) R.string.in_use_camera else R.string.in_use_microphone, pc))
            .addAction(Notification.Action.Builder(null, context.getString(R.string.display_stop), stop).build())
            .setGroup("in-use-$kind")
            .setOngoing(true)
            .build()
        context.getSystemService(NotificationManager::class.java).notify(inUseId(kind), notification)
    }

    fun cancelInUse(context: Context, kind: String) =
        context.getSystemService(NotificationManager::class.java).cancel(inUseId(kind))

    private fun inUseId(kind: String) = if (kind == MainActivity.CAMERA) 2011 else 2012

    private fun deviceId(kind: String) = if (kind == MainActivity.CAMERA) 2001 else 2002

    fun cancelPairing(context: Context, code: String) =
        context.getSystemService(NotificationManager::class.java).cancel(pairingId(code))

    /** What waits to be signed: one notification for the whole queue, gone
     * when it is empty. alert: it pops up, once per batch (not when updated
     * while shown); otherwise it only sits there (the screen opened by itself). */
    fun waiting(context: Context, jobs: List<Jobs.Job>, alert: Boolean) {
        val manager = context.getSystemService(NotificationManager::class.java)
        if (jobs.isEmpty()) return manager.cancel(WAITING_ID)
        val titles = Notification.InboxStyle()
        jobs.take(6).forEach { titles.addLine(it.title) }
        if (jobs.size > 6) titles.setSummaryText(context.resources.getQuantityString(R.plurals.and_more, jobs.size - 6, jobs.size - 6))
        val notification = Notification.Builder(context, if (alert) ASK else QUEUE)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.resources.getQuantityString(R.plurals.to_sign, jobs.size, jobs.size))
            .setContentText(jobs.joinToString(", ") { it.title })
            .setStyle(titles)
            .setNumber(jobs.size)
            .setContentIntent(PendingIntent.getActivity(context, WAITING_ID, SignActivity.open(context, null),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT))
            .setOnlyAlertOnce(true)
            .build()
        manager.notify(WAITING_ID, notification)
    }

    private const val WAITING_ID = 2
    private const val UPDATE_ID = 3

    /** The PC's update waits for the user's tap (Android asks the first time). */
    fun updateWaiting(context: Context, confirm: Intent) {
        val notification = Notification.Builder(context, ASK)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(R.string.update_waiting_title))
            .setContentText(context.getString(R.string.update_waiting_text))
            .setContentIntent(PendingIntent.getActivity(context, UPDATE_ID, confirm,
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT))
            .setAutoCancel(true)
            .build()
        context.getSystemService(NotificationManager::class.java).notify(UPDATE_ID, notification)
    }

    /** Updated: said once, quietly. */
    fun updated(context: Context) {
        val version = context.packageManager.getPackageInfo(context.packageName, 0).versionName
        val notification = Notification.Builder(context, QUEUE)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(R.string.updated_title, version))
            .setContentText(context.getString(R.string.updated_text))
            .setContentIntent(PendingIntent.getActivity(context, UPDATE_ID, Intent(context, MainActivity::class.java),
                PendingIntent.FLAG_IMMUTABLE))
            .setAutoCancel(true)
            .build()
        context.getSystemService(NotificationManager::class.java).notify(UPDATE_ID, notification)
    }
}
