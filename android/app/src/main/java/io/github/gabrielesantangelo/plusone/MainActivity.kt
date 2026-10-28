package io.github.gabrielesantangelo.plusone

import android.Manifest
import android.content.Intent
import android.content.pm.ApplicationInfo
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Bundle
import android.provider.Settings
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.LinearLayout
import android.widget.ScrollView
import androidx.appcompat.app.AppCompatActivity
import com.google.android.material.card.MaterialCardView
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import com.google.android.material.materialswitch.MaterialSwitch
import java.io.File
import com.google.android.material.R as M
import io.github.gabrielesantangelo.plusone.link.Paired

/**
 * What the app is doing and what it may do: the PDFs waiting to be signed,
 * the service that waits for the PC, the saved signatures, the permission
 * that lets a PDF open by itself, the paired PCs.
 */
class MainActivity : AppCompatActivity() {
    private lateinit var content: LinearLayout

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (savedInstanceState == null) tryOut(intent)  // not again when recreated
        content = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
        }
        setContentView(ScrollView(this).apply {
            setBackgroundColor(themeColor(M.attr.colorSurfaceContainer))
            clipToPadding = false
            addView(content)
            // Clear of the status and navigation bars (the app is drawn behind
            // them), and no wider than reads well on a big screen.
            var bars = android.graphics.Insets.NONE
            fun pad(v: View) {
                val side = maxOf(dp(16), (v.width - bars.left - bars.right - dp(MAX_WIDTH)) / 2)
                v.setPadding(bars.left + side, bars.top + dp(8), bars.right + side, bars.bottom + dp(24))
            }
            setOnApplyWindowInsetsListener { v, insets ->
                bars = systemBars(insets)
                pad(v)
                insets
            }
            addOnLayoutChangeListener { v, l, _, r, _, ol, _, or_, _ -> if (r - l != or_ - ol) v.post { pad(v) } }
        })
    }

    /** adb shell am start -S -n …/.MainActivity --es path /data/user/0/…/files/x.pdf
     * opens the signing screen on a PDF (for trying it out; debug builds only). */
    private fun tryOut(intent: Intent) {
        if (applicationInfo.flags and ApplicationInfo.FLAG_DEBUGGABLE == 0) return
        val path = intent.getStringExtra(EXTRA_PATH) ?: return
        val job = Jobs.local(File(path), File(getExternalFilesDir(null), "last-signature.json"))
        startActivity(SignActivity.open(this, job.id, chosen = true))
    }

    override fun onStart() {
        super.onStart()
        Front.started()
    }

    override fun onStop() {
        Front.stopped()
        super.onStop()
    }

    override fun onResume() {
        super.onResume()
        CompanionService.start(this)  // ready now (for a while, if not always)
        CompanionService.instance?.elevate()  // in front: camera and microphone for the PC, if let
        Jobs.listen(queueChanged)
        io.github.gabrielesantangelo.plusone.link.LinkStatus.listen(pathChanged)
        refresh()
    }

    override fun onPause() {
        Jobs.unlisten(queueChanged)
        io.github.gabrielesantangelo.plusone.link.LinkStatus.unlisten(pathChanged)
        super.onPause()
    }

    private val queueChanged: () -> Unit = { refresh() }
    private val pathChanged: (io.github.gabrielesantangelo.plusone.link.LinkStatus.Pc?,
                              io.github.gabrielesantangelo.plusone.link.LinkStatus.Pc?) -> Unit = { _, _ -> refresh() }

    private fun refresh() {
        // Made again from scratch: where the user had scrolled to stays.
        val scroller = content.parent as? ScrollView
        val scrolled = scroller?.scrollY ?: 0
        scroller?.post { scroller.scrollTo(0, scrolled) }
        content.removeAllViews()
        content.addView(label(getString(R.string.app_name), M.attr.textAppearanceDisplaySmall).apply {
            setPadding(dp(8), dp(32), dp(8), dp(20))
        })
        content.addView(hero())
        onboarding()?.let { content.addView(it) }

        val waiting = Jobs.waiting()
        if (waiting.isNotEmpty()) {
            section(R.string.heading_waiting, waiting.map { job ->
                val line = listOfNotNull(job.pc.takeIf { it.isNotEmpty() },
                    getString(R.string.started).takeIf { !job.draft.isNullOrEmpty() })
                Row(row(R.drawable.ic_document, job.title, line.joinToString(" · ").ifEmpty { null },
                    icon(R.drawable.ic_chevron_right), oneLine = true)) {
                    startActivity(SignActivity.open(this, job.id, chosen = true))
                }
            })
        }

        val always = MaterialSwitch(this).apply {
            isChecked = Options.alwaysReady(this@MainActivity)
            setOnCheckedChangeListener { _, on ->
                Options.setAlwaysReady(this@MainActivity, on)
                refresh()
            }
        }
        section(R.string.heading_ready, listOf(
            Row(row(R.drawable.ic_ready, getString(R.string.always_ready),
                getString(if (always.isChecked) R.string.always_on else R.string.always_off), always)) { always.toggle() },
        ))

        val scroll = MaterialSwitch(this).apply {
            isChecked = Options.scrollPages(this@MainActivity)
            setOnCheckedChangeListener { _, on ->
                Options.setScrollPages(this@MainActivity, on)
                refresh()
            }
        }
        section(R.string.heading_pages, listOf(
            Row(row(R.drawable.ic_scroll, getString(R.string.scroll_pages),
                getString(if (scroll.isChecked) R.string.scroll_on else R.string.scroll_off), scroll)) { scroll.toggle() },
        ))

        val saved = Signatures.all(this)
        section(R.string.heading_signatures, saved.map { signature ->
            Row(LinearLayout(this).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                addView(SignatureThumb(this@MainActivity, signature, Ink.COLOR), LinearLayout.LayoutParams(dp(200), dp(64)))
                addView(View(this@MainActivity), LinearLayout.LayoutParams(0, 1, 1f))
                addView(iconButton(R.drawable.ic_clear, R.string.remove) { askRemove(signature) })
            })
        } + Row(row(R.drawable.ic_add, getString(R.string.new_signature),
            if (saved.isEmpty()) getString(R.string.signatures_none) else null)) {
            val (color, width) = Pen.load(this)
            SignaturePad(this, color, width) { refresh() }.show()
        })

        val overlay = Settings.canDrawOverlays(this)
        section(R.string.heading_open, listOf(
            Row(row(R.drawable.ic_open, getString(R.string.open_by_itself),
                getString(if (overlay) R.string.overlay_on else R.string.overlay_off),
                if (overlay) icon(R.drawable.ic_check, androidx.appcompat.R.attr.colorPrimary)
                else button(getString(R.string.overlay_allow), Look.TONAL) { allowOverlay() })) { allowOverlay() },
        ))

        section(R.string.heading_pc_devices, listOf(
            pcDevice(CAMERA, R.drawable.ic_camera, R.string.camera_for_pc, R.string.camera_on, R.string.camera_off),
            pcDevice(MICROPHONE, R.drawable.ic_microphone, R.string.microphone_for_pc, R.string.microphone_on,
                R.string.microphone_off),
            speakers(),
        ))

        val paired = Paired.all(this)
        section(R.string.heading_paired, (if (paired.isEmpty()) {
            listOf(Row(row(R.drawable.ic_computer, getString(R.string.paired_none_title), getString(R.string.paired_none))))
        } else paired.map { (fingerprint, name) ->
            Row(row(R.drawable.ic_computer, name, pcAllows(fingerprint),
                iconButton(R.drawable.ic_forget, R.string.forget) { askForget(fingerprint, name) })) {
                pcOptions(fingerprint, name)
            }
        }) + pairNew())

        val version = packageManager.getPackageInfo(packageName, 0).versionName
        content.addView(label(getString(R.string.version, version), M.attr.textAppearanceBodySmall,
            M.attr.colorOnSurfaceVariant).apply {
            gravity = Gravity.CENTER
            setPadding(0, dp(32), 0, 0)
        }, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))
    }

    /** A new PC pairs only while the user holds this open, two minutes at
     * most (Pairing.windowUntil): no one else on the network can put the
     * question on the screen. */
    private fun pairNew(): Row {
        val open = Pairing.windowOpen()
        return Row(row(R.drawable.ic_computer, getString(R.string.pair_new),
            if (open) getString(R.string.pair_new_open, CompanionService.deviceName(this)) else getString(R.string.pair_new_hint),
            button(getString(if (open) R.string.pair_new_stop else R.string.pair_new_start), Look.TONAL) {
                if (Pairing.windowOpen()) Pairing.closeWindow() else {
                    Pairing.openWindow()
                    content.postDelayed({ if (!isFinishing) refresh() }, Pairing.windowUntil - android.os.SystemClock.elapsedRealtime() + 100)
                }
                refresh()
            }))
    }

    /** Ready, and how the PC finds this tablet: the app's whole job in a card. */
    private fun hero() = MaterialCardView(this, null, M.attr.materialCardViewFilledStyle).apply {
        setCardBackgroundColor(themeColor(M.attr.colorPrimaryContainer))
        radius = dp(28f)
        // Connected to a PC over the link's core: to which, and by what (the cable, the Wi-Fi...).
        val pc = io.github.gabrielesantangelo.plusone.link.LinkStatus.current()
        addView(LinearLayout(context).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(20), dp(20), dp(24), dp(20))
            val heroIcon = if (pc != null) io.github.gabrielesantangelo.plusone.link.LinkStatus.icon(pc.path) else R.drawable.ic_ready
            addView(icon(heroIcon, M.attr.colorOnPrimary, size = 28).apply {
                background = surface(androidx.appcompat.R.attr.colorPrimary, dp(28f))
                setPadding(dp(14), dp(14), dp(14), dp(14))
                layoutParams = LinearLayout.LayoutParams(dp(56), dp(56)).apply { marginEnd = dp(16) }
            })
            addView(LinearLayout(context).apply {
                orientation = LinearLayout.VERTICAL
                // Not paired: what to do on the PC. Paired: what this tablet is for, or whom it is connected to.
                val paired = Paired.all(this@MainActivity).isNotEmpty()
                if (pc != null) {
                    addView(label(getString(R.string.connected_to, pc.name.ifEmpty { "PC" }),
                        M.attr.textAppearanceTitleLarge, M.attr.colorOnPrimaryContainer))
                    val by = io.github.gabrielesantangelo.plusone.link.LinkStatus.name(this@MainActivity, pc.path, inSentence = true)
                    addView(label(if (pc.path == null) by else getString(R.string.via, by),
                        M.attr.textAppearanceBodyMedium, M.attr.colorOnPrimaryContainer))
                } else {
                    addView(label(getString(if (paired) R.string.ready else R.string.not_paired_title),
                        M.attr.textAppearanceTitleLarge, M.attr.colorOnPrimaryContainer))
                    addView(label(if (paired) getString(R.string.status_ready)
                        else getString(R.string.status_unpaired, CompanionService.deviceName(this@MainActivity)),
                        M.attr.textAppearanceBodyMedium, M.attr.colorOnPrimaryContainer))
                }
            }, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        })
    }

    /** What lets the app reach the user by itself (the pairing code, a PDF to
     * sign, a PC asking for the camera), asked together and said why, while
     * anything of it is missing; null once it is all there. */
    private fun onboarding(): View? {
        val notifications = checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) ==
            PackageManager.PERMISSION_GRANTED
        val overlay = Settings.canDrawOverlays(this)
        // Wi-Fi Direct with the PC (P2pHandler): asked with the notifications.
        val nearby = checkSelfPermission(Manifest.permission.NEARBY_WIFI_DEVICES) == PackageManager.PERMISSION_GRANTED
        // Bluetooth with the PC (BtHandler): asked with them too.
        val bluetooth = checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT) == PackageManager.PERMISSION_GRANTED
        if (notifications && nearby && bluetooth && overlay) return null
        return MaterialCardView(this, null, M.attr.materialCardViewFilledStyle).apply {
            radius = dp(28f)
            layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT).apply { topMargin = dp(12) }
            addView(LinearLayout(context).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(dp(20), dp(20), dp(20), dp(16))
                addView(label(getString(R.string.onboarding_title), M.attr.textAppearanceTitleMedium))
                addView(label(getString(R.string.onboarding_text), M.attr.textAppearanceBodyMedium,
                    M.attr.colorOnSurfaceVariant).apply { setPadding(0, dp(8), 0, dp(12)) })
                addView(button(getString(R.string.overlay_allow), Look.FILLED) {
                    // Notifications first (a question here), then "display over other apps" (a settings page).
                    val missing = listOfNotNull(Manifest.permission.POST_NOTIFICATIONS.takeIf { !notifications },
                                                Manifest.permission.NEARBY_WIFI_DEVICES.takeIf { !nearby },
                                                Manifest.permission.BLUETOOTH_CONNECT.takeIf { !bluetooth })
                    if (missing.isNotEmpty()) requestPermissions(missing.toTypedArray(), 1)
                    else allowOverlay()
                }, LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT)
                    .apply { gravity = Gravity.END })
            })
        }
    }

    /** A switch that lets the PC have the camera or the microphone: Android's
     * permission is asked for when it is turned on. */
    private fun pcDevice(kind: String, iconRes: Int, title: Int, on: Int, off: Int): Row {
        val permission = PERMISSIONS.getValue(kind)
        val allowed = checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED
        val chosen = if (kind == CAMERA) Options.camera(this) else Options.microphone(this)
        val switch = MaterialSwitch(this).apply {
            isChecked = chosen && allowed
            setOnCheckedChangeListener { _, checked ->
                when {
                    !checked -> {
                        setPcDevice(kind, false)
                        CompanionService.instance?.stopCaptures(kind)
                        refresh()
                    }
                    allowed -> {
                        setPcDevice(kind, true)
                        refresh()
                    }
                    shouldShowRequestPermissionRationale(permission) || !asked(kind) ->
                        requestPermissions(arrayOf(permission), REQUESTS.getValue(kind))
                    else -> {  // denied for good: only the app's settings can change it
                        startActivity(Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.parse("package:$packageName")))
                        isChecked = false
                    }
                }
            }
        }
        val subtitle = when {
            chosen && !allowed -> R.string.pc_device_denied
            switch.isChecked -> on
            else -> off
        }
        return Row(row(iconRes, getString(title), getString(subtitle), switch)) { switch.toggle() }
    }

    /** The PC's sound on the speakers: nothing for Android to allow. */
    private fun speakers(): Row {
        val switch = MaterialSwitch(this).apply {
            isChecked = Options.speakers(this@MainActivity)
            setOnCheckedChangeListener { _, checked ->
                Options.setSpeakers(this@MainActivity, checked)
                if (!checked) CompanionService.instance?.stopCaptures("speakers")
                refresh()
            }
        }
        return Row(row(R.drawable.ic_speakers, getString(R.string.speakers_for_pc),
            getString(if (switch.isChecked) R.string.speakers_on else R.string.speakers_off), switch)) { switch.toggle() }
    }

    private fun setPcDevice(kind: String, on: Boolean) {
        if (kind == CAMERA) Options.setCamera(this, on) else Options.setMicrophone(this, on)
        // In front now: the service takes it at once, so the PC's first use is quick too.
        if (on) CompanionService.instance?.elevate()
    }

    private fun asked(kind: String) = Options.asked(this, kind)

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, results)
        if (requestCode == 1) {  // notifications (the onboarding card): then the overlay, if missing
            if (!Settings.canDrawOverlays(this)) allowOverlay() else refresh()
            return
        }
        val kind = REQUESTS.entries.firstOrNull { it.value == requestCode }?.key ?: return
        Options.setAsked(this, kind)
        setPcDevice(kind, results.firstOrNull() == PackageManager.PERMISSION_GRANTED)
        refresh()
    }

    private fun section(title: Int, rows: List<Row>) {
        content.addView(sectionTitle(getString(title)))
        content.addView(group(rows))
    }

    private fun allowOverlay() {
        startActivity(Intent(Settings.ACTION_MANAGE_OVERLAY_PERMISSION, Uri.parse("package:$packageName")))
    }

    private fun askRemove(signature: Signature) {
        MaterialAlertDialogBuilder(this)
            .setTitle(R.string.remove_signature)
            .setMessage(R.string.remove_signature_detail)
            .setPositiveButton(R.string.remove) { _, _ ->
                Signatures.remove(this, signature.id)
                refresh()
            }
            .setNegativeButton(R.string.cancel, null)
            .show()
    }

    /** What this PC may do, in a line: what is on, or a hint that there is a choice. */
    private fun pcAllows(fingerprint: String): String =
        listOfNotNull(getString(R.string.pc_updates_app).takeIf { PcOptions.updates(this, fingerprint) },
            getString(R.string.pc_shares_signatures).takeIf { PcOptions.shareSignatures(this, fingerprint) },
            getString(R.string.pc_uses_camera).takeIf { PcOptions.device(this, fingerprint, CAMERA) },
            getString(R.string.pc_uses_microphone).takeIf { PcOptions.device(this, fingerprint, MICROPHONE) })
            .joinToString(" · ").ifEmpty { getString(R.string.pc_options_hint) }

    /** The choices for one PC alone. */
    private fun pcOptions(fingerprint: String, name: String) {
        val items = arrayOf(getString(R.string.pc_update_app), getString(R.string.pc_share_signatures),
            getString(R.string.pc_use_camera), getString(R.string.pc_use_microphone))
        val checked = booleanArrayOf(PcOptions.updates(this, fingerprint), PcOptions.shareSignatures(this, fingerprint),
            PcOptions.device(this, fingerprint, CAMERA), PcOptions.device(this, fingerprint, MICROPHONE))
        MaterialAlertDialogBuilder(this)
            .setTitle(name)
            .setMultiChoiceItems(items, checked) { _, which, on ->
                when (which) {
                    0 -> PcOptions.setUpdates(this, fingerprint, on)
                    1 -> PcOptions.setShareSignatures(this, fingerprint, on)
                    2 -> PcOptions.setDevice(this, fingerprint, CAMERA, on)
                    3 -> PcOptions.setDevice(this, fingerprint, MICROPHONE, on)
                }
            }
            .setPositiveButton(R.string.done) { _, _ -> }
            .setOnDismissListener { refresh() }
            .show()
    }

    private fun askForget(fingerprint: String, name: String) {
        MaterialAlertDialogBuilder(this)
            .setTitle(getString(R.string.forget_title, name))
            .setMessage(R.string.forget_detail)
            .setPositiveButton(R.string.forget) { _, _ ->
                Paired.forget(this, fingerprint)
                PcOptions.forget(this, fingerprint)
                CompanionService.instance?.disconnect(fingerprint)  // and what it has open, now
                refresh()
            }
            .setNegativeButton(R.string.cancel, null)
            .show()
    }

    companion object {
        const val MAX_WIDTH = 760  // dp of content
        const val EXTRA_PATH = "path"
        const val CAMERA = "camera"
        const val MICROPHONE = "microphone"
        private val PERMISSIONS = mapOf(CAMERA to Manifest.permission.CAMERA,
            MICROPHONE to Manifest.permission.RECORD_AUDIO)
        private val REQUESTS = mapOf(CAMERA to 2, MICROPHONE to 3)
    }
}
