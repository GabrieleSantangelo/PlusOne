package io.github.gabrielesantangelo.plusone

import android.content.Context

/** The app's own settings. */
object Options {
    private const val PREFS = "options"

    /** Ready for the PC all the time (and after a restart), or only for a
     * while after the app was opened. */
    fun alwaysReady(context: Context) =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getBoolean("always", true)

    fun setAlwaysReady(context: Context, on: Boolean) =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().putBoolean("always", on).apply()

    /** A PDF's pages one under the other, scrolled through (as a note app
     * shows a notebook), or one at a time, turned with the arrows. */
    fun scrollPages(context: Context) =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getBoolean("scroll", true)

    fun setScrollPages(context: Context, on: Boolean) =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().putBoolean("scroll", on).apply()

    /** The PC may use the camera, the microphone (while a program there does). Off at first. */
    fun camera(context: Context) =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getBoolean("camera", false)

    fun setCamera(context: Context, on: Boolean) =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().putBoolean("camera", on).apply()

    /** Whether Android's question for kind was asked already (after a "no", it is not asked again). */
    fun asked(context: Context, kind: String) =
        context.getSharedPreferences("asked", Context.MODE_PRIVATE).getBoolean(kind, false)

    fun setAsked(context: Context, kind: String) =
        context.getSharedPreferences("asked", Context.MODE_PRIVATE).edit().putBoolean(kind, true).apply()

    /** The PC may play on the speakers (while a program there does). On at first: only sound. */
    fun speakers(context: Context) =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getBoolean("speakers", true)

    fun setSpeakers(context: Context, on: Boolean) =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().putBoolean("speakers", on).apply()

    fun microphone(context: Context) =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getBoolean("microphone", false)

    fun setMicrophone(context: Context, on: Boolean) =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().putBoolean("microphone", on).apply()
}

/** What the user allows each paired PC (by its certificate's fingerprint),
 * apart from the others; forgotten with the PC. */
object PcOptions {
    private const val PREFS = "pc-options"
    const val SIGNATURES = "signatures"
    const val UPDATES = "updates"

    private fun prefs(context: Context) = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    /** The saved signatures shared with that PC, both ways (if it shares them too). Off at first. */
    fun shareSignatures(context: Context, pc: String) = prefs(context).getBoolean("$pc/$SIGNATURES", false)

    fun setShareSignatures(context: Context, pc: String, on: Boolean) =
        prefs(context).edit().putBoolean("$pc/$SIGNATURES", on).apply()

    /** The app's update from that PC is taken (plusone/updates.py). Asked
     * when it is paired (yes at first); the PCs paired before it was asked
     * keep updating it (adoptPaired). */
    fun updates(context: Context, pc: String) = prefs(context).getBoolean("$pc/$UPDATES", false)

    fun setUpdates(context: Context, pc: String, on: Boolean) =
        prefs(context).edit().putBoolean("$pc/$UPDATES", on).apply()

    /** Once: the PCs paired before updates were asked about update as they did. */
    fun adoptPaired(context: Context, paired: Set<String>) {
        val prefs = prefs(context)
        if (prefs.getBoolean(ADOPTED, false)) return
        prefs.edit().apply {
            for (pc in paired) if (!prefs.contains("$pc/$UPDATES")) putBoolean("$pc/$UPDATES", true)
            putBoolean(ADOPTED, true)
        }.apply()
    }

    private const val ADOPTED = "updates-adopted"

    fun forget(context: Context, pc: String) {
        val prefs = prefs(context)
        prefs.edit().apply { prefs.all.keys.filter { it.startsWith("$pc/") }.forEach(::remove) }.apply()
    }
}
