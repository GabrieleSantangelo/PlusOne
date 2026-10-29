package io.github.gabrielesantangelo.plusone

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.content.pm.PackageManager
import android.provider.Settings
import android.util.Log
import java.io.File
import java.security.MessageDigest

/**
 * Updates from the paired PC (plusone/updates.py): it hands over the app of
 * its own version when this one is older and nothing is in use. Checked
 * here (this app's package, newer, signed with this app's key: Android
 * checks the key again) and installed with PackageInstaller: without a
 * question once Plus One is the app that installed itself (Android 12 and
 * later let the installer of record update its own app), else Android asks
 * once, with a tap.
 */
object Updates {
    const val MAX_APK = 64L * 1024 * 1024
    private const val UPDATES = "updates"

    fun versionCode(context: Context): Long =
        context.packageManager.getPackageInfo(context.packageName, 0).longVersionCode

    /** A file of its own for an update on its way (two at once, from two PCs
     * or both links, would write one over the other), in cache's "updates". */
    fun incoming(cache: File): File = File.createTempFile("update", ".apk", File(cache, UPDATES).apply { mkdirs() })

    /** What updates left in cache: one being installed ends this process,
     * maybe before its file is deleted (so did the one file of before). As
     * the service starts, before any update can come. */
    fun clean(cache: File) {
        File(cache, UPDATES).listFiles()?.forEach { it.delete() }
        File(cache, "update.apk").delete()
    }

    /** Why file is not this app's update, or null if it is. */
    fun check(context: Context, file: File, sha256: String): String? {
        val digest = MessageDigest.getInstance("SHA-256")
        file.inputStream().use { input ->
            val buffer = ByteArray(1 shl 16)
            while (true) {
                val n = input.read(buffer)
                if (n < 0) break
                digest.update(buffer, 0, n)
            }
        }
        if (digest.digest().joinToString("") { "%02x".format(it) } != sha256) return "invalid"
        val pm = context.packageManager
        val archive = pm.getPackageArchiveInfo(file.path,
            PackageManager.PackageInfoFlags.of(PackageManager.GET_SIGNING_CERTIFICATES.toLong())) ?: return "invalid"
        if (archive.packageName != context.packageName) return "invalid"
        if (archive.longVersionCode <= versionCode(context)) return "older"
        val signers = archive.signingInfo?.apkContentsSigners ?: return "invalid"
        if (signers.none { pm.hasSigningCertificate(context.packageName, it.toByteArray(), PackageManager.CERT_INPUT_RAW_X509) }) {
            return "invalid"
        }
        return null
    }

    /** Installs file over this app: Android ends this process to do it, and
     * starts the service again after (BootReceiver). */
    fun install(context: Context, file: File) {
        val installer = context.packageManager.packageInstaller
        val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL).apply {
            setAppPackageName(context.packageName)
            setSize(file.length())
            setRequireUserAction(PackageInstaller.SessionParams.USER_ACTION_NOT_REQUIRED)
            setInstallReason(PackageManager.INSTALL_REASON_USER)
        }
        val id = installer.createSession(params)
        installer.openSession(id).use { session ->
            session.openWrite("base.apk", 0, file.length()).use { out ->
                file.inputStream().use { it.copyTo(out) }
                session.fsync(out)
            }
            // Mutable: Android puts the outcome in it. Explicit, so nobody else gets it.
            val status = PendingIntent.getBroadcast(context, id, Intent(context, UpdateReceiver::class.java),
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE)
            session.commit(status.intentSender)
        }
    }
}

/** How an update went: Android wants the user's tap (the first time, or
 * while Plus One is not the app that installed itself), or it failed. */
class UpdateReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val status = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)
        when (status) {
            PackageInstaller.STATUS_PENDING_USER_ACTION -> {
                val confirm = intent.getParcelableExtra(Intent.EXTRA_INTENT, Intent::class.java) ?: return
                confirm.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                // At once over whatever is on screen if allowed; the
                // notification stays for when it was dismissed, or not shown.
                if (Settings.canDrawOverlays(context) || Front.shown) runCatching { context.startActivity(confirm) }
                Notifications.updateWaiting(context, confirm)
            }
            PackageInstaller.STATUS_SUCCESS -> {}
            else -> Log.w(CompanionService.TAG,
                "update not installed: $status ${intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE)}")
        }
    }
}
