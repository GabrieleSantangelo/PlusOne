package io.github.gabrielesantangelo.plusone

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.graphics.Color
import android.os.Bundle
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.view.WindowManager
import android.widget.FrameLayout

/** The PC's desktop, on the whole screen: the session in progress
 * (DisplaySession.current) decodes into this activity's surface while it
 * is shown. One activity: a new session takes it over (onNewIntent), as its
 * surface stays the same. */
class DisplayActivity : Activity(), SurfaceHolder.Callback {
    private var surface: Surface? = null
    private var session: DisplaySession? = null  // the one decoding into the surface
    // The one this activity shows, kept when the surface goes (before
    // onStop()): what onStop() and onDestroy() may pause or end. Never a
    // newer session's, which a closing activity must leave alone.
    private var mine: DisplaySession? = null
    private lateinit var pathOverlay: PathOverlay
    /** The path of the desktop's PC changed: shown over it. */
    private val pathChanged: (io.github.gabrielesantangelo.plusone.link.LinkStatus.Pc?,
                              io.github.gabrielesantangelo.plusone.link.LinkStatus.Pc?) -> Unit = { before, now ->
        val device = mine?.device
        val pc = now ?: before
        if (device != null && pc?.id == device) pathOverlay.changed(before?.path, now?.path, gone = now == null)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        instance = this
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        protect(this)
        window.attributes = window.attributes.apply {
            layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
        }
        val view = SurfaceView(this)
        view.setSecure(true)
        view.holder.addCallback(this)
        // The PC's monitor has the screen's proportions, held in landscape;
        // Android 16 lets a large screen's app turn to portrait all the same
        // (and split the screen): then it is letterboxed, not squeezed.
        val mode = display.mode
        val aspect = maxOf(mode.physicalWidth, mode.physicalHeight).toFloat() /
            minOf(mode.physicalWidth, mode.physicalHeight)
        pathOverlay = PathOverlay(this) { mine }
        setContentView(FrameLayout(this).apply {
            addView(Letterbox(this@DisplayActivity, view, aspect))
            addView(pathOverlay, FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT))
        })
        DisplayInput(view) { session }
        window.insetsController?.apply {
            hide(WindowInsets.Type.systemBars())
            systemBarsBehavior = WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
        if (DisplaySession.current == null) finish()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        bind()
    }

    override fun onStart() {
        super.onStart()
        Front.started()  // the app is in front: the PC's camera or a new desktop may come up
        mine = DisplaySession.current
        mine?.show(true)
        // What it comes by: the link's core says (its sessions' paths); the link of before is the network's.
        val device = mine?.device
        pathOverlay.show(if (device != null) io.github.gabrielesantangelo.plusone.link.LinkStatus.current(device)?.path else "lan")
        io.github.gabrielesantangelo.plusone.link.LinkStatus.listen(pathChanged)
    }

    override fun onStop() {
        super.onStop()
        io.github.gabrielesantangelo.plusone.link.LinkStatus.unlisten(pathChanged)
        Front.stopped()
        // Another app, the screen off: the PC rests.
        mine?.takeIf { it === DisplaySession.current }?.show(false)
    }

    override fun onDestroy() {
        // Back, or swiped away from Recents: the desktop is no longer wanted
        // here. (Not when it went because the session ended: then mine is
        // over, and DisplaySession.current is another's or nobody's.)
        if (isFinishing) mine?.takeIf { it === DisplaySession.current }?.end()
        if (instance === this) instance = null
        super.onDestroy()
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        surface = holder.surface
        bind()
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        session?.detach()
        session = null
        surface = null
    }

    /** The session in progress into the surface, if both are there. */
    private fun bind() {
        val current = DisplaySession.current ?: return finish()
        mine = current
        if (current === session) return
        session?.detach()
        session = null
        val surface = surface ?: return  // surfaceCreated() binds it
        current.attach(surface)
        session = current
        current.show(true)
    }

    /** child at aspect (width / height), as large as fits, centred on black. */
    private class Letterbox(context: Context, private val child: View, private val aspect: Float) :
        FrameLayout(context) {
        init {
            setBackgroundColor(Color.BLACK)
            addView(child)
        }

        override fun onMeasure(widthSpec: Int, heightSpec: Int) {
            val width = MeasureSpec.getSize(widthSpec)
            val height = MeasureSpec.getSize(heightSpec)
            val wide = width >= height * aspect
            val w = if (wide) Math.round(height * aspect) else width
            val h = if (wide) height else Math.round(width / aspect)
            child.measure(MeasureSpec.makeMeasureSpec(w, MeasureSpec.EXACTLY),
                MeasureSpec.makeMeasureSpec(h, MeasureSpec.EXACTLY))
            setMeasuredDimension(width, height)
        }

        override fun onLayout(changed: Boolean, left: Int, top: Int, right: Int, bottom: Int) {
            val x = (width - child.measuredWidth) / 2
            val y = (height - child.measuredHeight) / 2
            child.layout(x, y, x + child.measuredWidth, y + child.measuredHeight)
        }
    }

    companion object {
        // On the main thread only.
        private var instance: DisplayActivity? = null

        /** On the main thread: a session ended. The activity goes, unless a
         * newer session is there to take it over (its startActivity() comes
         * as onNewIntent()). */
        fun ended(session: DisplaySession) {
            val activity = instance ?: return
            if (DisplaySession.current?.takeIf { it !== session } == null) activity.finish()
        }
    }
}
