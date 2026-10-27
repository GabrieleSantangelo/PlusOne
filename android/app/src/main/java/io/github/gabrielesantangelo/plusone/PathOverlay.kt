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
import android.view.View
import android.view.animation.LinearInterpolator
import android.view.animation.OvershootInterpolator
import android.widget.FrameLayout
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView
import io.github.gabrielesantangelo.plusone.link.LinkStatus

/**
 * Over the desktop (DisplayActivity), what it comes by: a small pill in a
 * corner, always there (faint after a moment, bright again at a change);
 * and while the link moves from one path to another, a screen that says
 * so, the old path's icon giving way to the arrows going round, then the
 * new one's coming in, until the desktop's frames come again on it. It
 * never takes a touch: the desktop under it gets them all.
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
    private var movedAt = 0L           // uptime ms of the last move, waiting for a frame after it

    init {
        // The pill: bottom left, clear of GNOME's bar and of most windows' buttons.
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
            setMargins(context.dp(16), 0, 0, context.dp(16))
        })

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

    /** The path now, without a change's screen (the desktop just opened). */
    fun show(path: String?) {
        setPill(path, bright = true)
    }

    /** A change of the path (LinkStatus): before's and now's. */
    fun changed(before: String?, now: String?, gone: Boolean) {
        if (gone) return  // the session over: the activity goes
        if (now == null) waiting(before ?: shown) else if (now != shown) moved(shown, now)
    }

    private fun setPill(path: String?, bright: Boolean) {
        shown = path
        pillIcon.setImageResource(LinkStatus.icon(path))
        pillText.text = LinkStatus.name(context, path)
        pill.animate().cancel()
        if (bright) {
            pill.alpha = 1f
            pill.scaleX = 0.8f
            pill.scaleY = 0.8f
            pill.animate().scaleX(1f).scaleY(1f).setInterpolator(OvershootInterpolator(3f)).setDuration(350)
                .withEndAction { pill.animate().alpha(FAINT).setStartDelay(BRIGHT_MS).setDuration(800) }
        }
    }

    private fun showScrim() {
        if (scrim.visibility == VISIBLE) return
        scrim.alpha = 0f
        scrim.visibility = VISIBLE
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
        setPill(null, bright = true)
    }

    /** On a path again (another, or a better one): the new icon comes in;
     * the screen goes once a frame came on it. */
    private fun moved(from: String?, to: String) {
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
        setPill(to, bright = true)
        movedAt = SystemClock.uptimeMillis()
        val since = movedAt
        // Steady: a frame decoded after the move (or, at worst, STEADY_MS).
        fun look() {
            if (movedAt != since) return  // another change since
            val frame = session()?.lastFrameAt ?: 0L
            val waited = SystemClock.uptimeMillis() - since
            if (frame > since || waited > STEADY_MS) {
                postDelayed({ if (movedAt == since) hideScrim() }, SETTLE_MS)
            } else {
                postDelayed(::look, 100)
            }
        }
        postDelayed(::look, 300)
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
        const val FAINT = 0.4f
        const val BRIGHT_MS = 4000L  // the pill bright this long after a change
        const val STEADY_MS = 4000L  // the change's screen at most this long after the move
        const val SETTLE_MS = 500L   // and this long after the first frame on the new path
    }
}
