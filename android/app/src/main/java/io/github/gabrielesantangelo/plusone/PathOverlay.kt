package io.github.gabrielesantangelo.plusone

import android.animation.ValueAnimator
import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.drawable.GradientDrawable
import android.os.SystemClock
import android.util.TypedValue
import android.view.Gravity
import android.view.RoundedCorner
import android.view.View
import android.view.WindowInsets
import android.view.animation.AccelerateInterpolator
import android.view.animation.LinearInterpolator
import android.view.animation.OvershootInterpolator
import android.widget.FrameLayout
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView
import io.github.gabrielesantangelo.plusone.link.LinkStatus
import io.github.gabrielesantangelo.plusone.link.PathLabel
import kotlin.math.ceil
import kotlin.math.sqrt

/**
 * Over the desktop (DisplayActivity), whose screen it is and what it comes
 * by: a small pill in the bottom left corner ("PC screen · QUIC · Wi-Fi",
 * "PC screen · Cable"), bright for SHOWN_MS when the desktop comes up and
 * at each change of its path or transport, then faint, always there
 * (SEC-30); and while the link moves from one
 * path to another, a screen that says so, the old path's icon giving way
 * to the arrows going round, then the new one's coming in, until the
 * desktop's frames come again on it. A TCP path moved to QUIC is the same
 * path: the label alone says it. The label changes at the link's events
 * only (LinkStatus), its one timer its own fade. It never takes a touch:
 * the desktop under it gets them all.
 */
class PathOverlay(context: Context, private val session: () -> DisplaySession?) : FrameLayout(context) {
    private val pillIcon = ImageView(context)
    private val pillText = TextView(context)
    private val pill = LinearLayout(context)
    private val scrim = FrameLayout(context)
    private val rings = Rings(context)
    private val fromIcon = ImageView(context)
    private val toIcon = ImageView(context)
    private val title = TextView(context)
    private val detail = TextView(context)
    private var spin: ValueAnimator? = null
    private var shown: String? = null  // the path the pill says
    private var shownTransport: String? = null  // and its transport
    private val margin = context.dp(16)
    private val fade = Runnable {
        pill.animate().alpha(FAINT).setInterpolator(AccelerateInterpolator()).setStartDelay(0).setDuration(FADE_MS)
    }
    private var movedAt = 0L           // uptime ms of the last move, waiting for a frame after it (0: none)
    // Steady after a move: a frame decoded after it (or, at worst, STEADY_MS); then the screen goes.
    private val look = object : Runnable {
        override fun run() {
            val since = movedAt
            if (since == 0L) return
            val frame = session()?.lastFrameAt ?: 0L
            if (frame > since || SystemClock.uptimeMillis() - since > STEADY_MS) postDelayed(settle, SETTLE_MS)
            else postDelayed(this, 100)
        }
    }
    private val settle = Runnable {
        movedAt = 0
        hideScrim()
    }

    init {
        // The label, a pill: bottom left, clear of GNOME's bar and of most windows' buttons.
        pill.apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            background = GradientDrawable().apply { cornerRadius = context.dp(16f); setColor(0xB3000000.toInt()) }
            setPadding(context.dp(10), context.dp(6), context.dp(12), context.dp(6))
            addView(pillIcon, LinearLayout.LayoutParams(context.dp(16), context.dp(16)).apply { marginEnd = context.dp(6) })
            addView(pillText)
        }
        pillIcon.setColorFilter(Color.WHITE)
        pillText.setTextColor(Color.WHITE)
        pillText.setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
        addView(pill, LayoutParams(LayoutParams.WRAP_CONTENT, LayoutParams.WRAP_CONTENT, Gravity.BOTTOM or Gravity.START).apply {
            setMargins(margin, 0, 0, margin)
        })
        // Clear of a cutout on that side and of the screen's rounded corner
        // (as far into it as its arc reaches: r (1 - 1/√2)). The system bars
        // are hidden: a swipe shows them over everything for a moment.
        setOnApplyWindowInsetsListener { _, insets ->
            val cutout = insets.getInsets(WindowInsets.Type.displayCutout())
            val radius = insets.getRoundedCorner(RoundedCorner.POSITION_BOTTOM_LEFT)?.radius ?: 0
            val clear = maxOf(margin, ceil(radius * (1 - 1 / sqrt(2.0))).toInt())
            (pill.layoutParams as LayoutParams).setMargins(cutout.left + clear, 0, 0, cutout.bottom + clear)
            pill.requestLayout()
            insets
        }

        // The screen of a change.
        scrim.setBackgroundColor(0xC0000000.toInt())
        scrim.visibility = GONE
        val card = LinearLayout(context).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER_HORIZONTAL
        }
        val stage = FrameLayout(context)
        stage.addView(rings, LayoutParams(context.dp(180), context.dp(180), Gravity.CENTER))
        for (icon in listOf(fromIcon, toIcon)) {
            icon.setColorFilter(Color.WHITE)
            stage.addView(icon, LayoutParams(context.dp(56), context.dp(56), Gravity.CENTER))
        }
        card.addView(stage, LinearLayout.LayoutParams(context.dp(180), context.dp(180)))
        title.setTextColor(Color.WHITE)
        title.setTextSize(TypedValue.COMPLEX_UNIT_SP, 28f)
        title.gravity = Gravity.CENTER
        card.addView(title, LinearLayout.LayoutParams(LayoutParams.WRAP_CONTENT, LayoutParams.WRAP_CONTENT).apply {
            topMargin = context.dp(12)
        })
        detail.setTextColor(0xCCFFFFFF.toInt())
        detail.setTextSize(TypedValue.COMPLEX_UNIT_SP, 16f)
        detail.gravity = Gravity.CENTER
        detail.maxWidth = context.dp(480)
        card.addView(detail, LinearLayout.LayoutParams(LayoutParams.WRAP_CONTENT, LayoutParams.WRAP_CONTENT).apply {
            topMargin = context.dp(8)
        })
        scrim.addView(card, LayoutParams(LayoutParams.WRAP_CONTENT, LayoutParams.WRAP_CONTENT, Gravity.CENTER))
        addView(scrim, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.MATCH_PARENT))
        isClickable = false
        isFocusable = false
    }

    /** The path now and its transport, without a change's screen (the
     * desktop just opened, or is in front again). On a path, the desktop is
     * live: a change screen left from before (events missed while it was
     * not shown) goes at once. */
    fun show(path: String?, transport: String?) {
        if (path != null) clearScrim()
        setPill(path, transport)
    }

    /** A change of the PC's path or transport (LinkStatus): before and now. */
    fun changed(before: LinkStatus.Pc?, now: LinkStatus.Pc?) {
        when (val change = PathLabel.change(shown, shownTransport, now)) {
            PathLabel.Change.Gone, PathLabel.Change.Same -> {}  // gone: the activity goes
            PathLabel.Change.Waiting -> waiting(before?.path ?: shown)
            is PathLabel.Change.Moved -> moved(shown, change.path, change.transport)
            is PathLabel.Change.Transport -> setPill(change.path, change.transport)
        }
    }

    /** Nothing going on while the desktop is not shown (the activity
     * stopped, or this view gone): no timer, no animation, the change
     * screen down; show() says what is now when it is back. */
    fun stop() {
        clearScrim()
        removeCallbacks(fade)
        pill.animate().cancel()
    }

    override fun onDetachedFromWindow() {
        stop()
        super.onDetachedFromWindow()
    }

    private fun clearScrim() {
        removeCallbacks(look)
        removeCallbacks(settle)
        movedAt = 0
        spin?.cancel()
        spin = null
        for (view in listOf(scrim, fromIcon, toIcon)) view.animate().cancel()
        rings.stop()
        scrim.visibility = GONE
    }

    /** The pill, bright from now for SHOWN_MS, then faint (the system's
     * animator duration scale makes the fade shorter or none; the time it
     * stays bright is the user's to read, not an animation). */
    private fun setPill(path: String?, transport: String?) {
        shown = path
        shownTransport = transport
        pillIcon.setImageResource(LinkStatus.icon(path))
        // Whose screen this is, always (SEC-30): a picture from the PC could
        // look like Android's own, its lock screen say; this pill is the
        // app's, above it, and the stream cannot cover it.
        pillText.text = PathLabel.marker(path, transport, context::getString)
        removeCallbacks(fade)
        pill.animate().cancel()
        pill.alpha = 1f
        if (ValueAnimator.areAnimatorsEnabled()) {
            pill.scaleX = 0.8f
            pill.scaleY = 0.8f
            pill.animate().scaleX(1f).scaleY(1f).setInterpolator(OvershootInterpolator(3f)).setStartDelay(0).setDuration(350)
        } else {
            pill.scaleX = 1f
            pill.scaleY = 1f
        }
        postDelayed(fade, SHOWN_MS)
    }

    private fun showScrim() {
        // Going already: up again (its fade's end would take it down).
        scrim.animate().cancel()
        if (scrim.visibility != VISIBLE) {
            scrim.alpha = 0f
            scrim.visibility = VISIBLE
        }
        scrim.animate().alpha(1f).setDuration(200)
        rings.start()
    }

    private fun hideScrim() {
        scrim.animate().alpha(0f).setDuration(350).withEndAction {
            scrim.visibility = GONE
            rings.stop()
        }
    }

    /** The path lost: the screen up, the lost path's icon giving way to the arrows. */
    private fun waiting(lost: String?) {
        movedAt = 0
        removeCallbacks(look)
        removeCallbacks(settle)
        showScrim()
        title.text = context.getString(R.string.path_reconnecting)
        detail.text = if (lost != null) context.getString(R.string.switch_lost, LinkStatus.name(context, lost, inSentence = true))
            else context.getString(R.string.switch_waiting)
        fromIcon.setImageResource(LinkStatus.icon(lost))
        fromIcon.apply { animate().cancel(); alpha = 1f; translationX = 0f; scaleX = 1f; scaleY = 1f }
        fromIcon.animate().alpha(0f).scaleX(0.6f).scaleY(0.6f).setDuration(500).setStartDelay(300)
        toIcon.setImageResource(R.drawable.ic_sync)
        toIcon.apply { animate().cancel(); alpha = 0f; translationX = 0f; rotation = 0f }
        toIcon.animate().alpha(1f).setStartDelay(500).setDuration(400)
        spin?.cancel()
        spin = ValueAnimator.ofFloat(0f, 360f).apply {
            duration = 1200
            repeatCount = ValueAnimator.INFINITE
            interpolator = LinearInterpolator()
            addUpdateListener { toIcon.rotation = it.animatedValue as Float }
            start()
        }
        setPill(null, null)
    }

    /** On a path again (another, or a better one): the new icon comes in;
     * the screen goes once a frame came on it. */
    private fun moved(from: String?, to: String, transport: String?) {
        val wasWaiting = spin != null
        showScrim()
        spin?.cancel()
        spin = null
        title.text = context.getString(R.string.switch_now, LinkStatus.name(context, to, inSentence = true))
        detail.text = context.getString(R.string.switch_steadying)
        // Out: the arrows (after a wait), or the path before (a better one came).
        val leaving = toIcon
        val coming = fromIcon
        leaving.animate().cancel()
        if (!wasWaiting) {
            leaving.setImageResource(LinkStatus.icon(from))
            leaving.alpha = 1f
            leaving.translationX = 0f
        }
        leaving.animate().translationX(-context.dp(72f)).alpha(0f).rotation(0f).setDuration(350).setStartDelay(0)
        coming.apply {
            animate().cancel()
            setImageResource(LinkStatus.icon(to))
            rotation = 0f
            scaleX = 1f
            scaleY = 1f
            translationX = context.dp(72f)
            alpha = 0f
        }
        coming.animate().translationX(0f).alpha(1f).setInterpolator(OvershootInterpolator(2f)).setDuration(500).setStartDelay(150)
        setPill(to, transport)
        movedAt = SystemClock.uptimeMillis()
        removeCallbacks(look)
        removeCallbacks(settle)
        postDelayed(look, 300)
    }

    /** Circles going out from the middle, one after the other. */
    private class Rings(context: Context) : View(context) {
        private val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeWidth = context.dp(2f)
            color = Color.WHITE
        }
        private var phase = 0f
        private val animator = ValueAnimator.ofFloat(0f, 1f).apply {
            duration = 1800
            repeatCount = ValueAnimator.INFINITE
            interpolator = LinearInterpolator()
            addUpdateListener { phase = it.animatedValue as Float; invalidate() }
        }

        fun start() { if (!animator.isStarted) animator.start() }
        fun stop() = animator.cancel()

        override fun onDraw(canvas: Canvas) {
            val most = minOf(width, height) / 2f
            for (i in 0 until 3) {
                val t = (phase + i / 3f) % 1f
                paint.alpha = ((1f - t) * 200).toInt()
                canvas.drawCircle(width / 2f, height / 2f, most * (0.35f + 0.65f * t), paint)
            }
        }
    }

    companion object {
        const val FAINT = 0.4f       // the pill's alpha once SHOWN_MS is over
        const val SHOWN_MS = 4000L   // the pill bright this long after the desktop came up or a change
        const val FADE_MS = 800L     // then it fades to FAINT this long
        const val STEADY_MS = 4000L  // the change's screen at most this long after the move
        const val SETTLE_MS = 500L   // and this long after the first frame on the new path
    }
}
