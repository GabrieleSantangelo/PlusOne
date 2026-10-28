package io.github.gabrielesantangelo.plusone

import android.content.Context
import android.content.res.ColorStateList
import android.graphics.Color
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView
import androidx.appcompat.content.res.AppCompatResources
import com.google.android.material.button.MaterialButton
import com.google.android.material.color.MaterialColors
import com.google.android.material.listitem.ListItemCardView
import com.google.android.material.listitem.ListItemLayout
import com.google.android.material.shape.MaterialShapeDrawable
import com.google.android.material.shape.ShapeAppearanceModel
import kotlin.math.roundToInt
import com.google.android.material.R as M

/*
 * Material 3 pieces made in code, as all of the app's views are: text in
 * the type scale, buttons, rows grouped as Android 16's settings group
 * them, surfaces; each in the theme's colours (the wallpaper's).
 */

/** The PC's screen, a signature, a pairing code: kept out of screenshots,
 * screen recordings, casting and the Recents thumbnail (SEC-30). */
fun protect(activity: android.app.Activity) {
    activity.window.addFlags(android.view.WindowManager.LayoutParams.FLAG_SECURE)
    activity.setRecentsScreenshotEnabled(false)
}

fun Context.dp(v: Int): Int = (v * resources.displayMetrics.density).roundToInt()
fun Context.dp(v: Float): Float = v * resources.displayMetrics.density

/** A colour of the theme: M.attr.colorSurfaceContainer... (colorPrimary is AppCompat's attribute). */
fun Context.themeColor(attr: Int): Int = MaterialColors.getColor(this, attr, Color.MAGENTA)

private fun Context.resourceOf(attr: Int): Int =
    TypedValue().also { theme.resolveAttribute(attr, it, true) }.resourceId

/** Text in a size of the type scale (M.attr.textAppearanceBodyLarge...), in a colour of the theme. */
fun Context.label(text: CharSequence?, appearance: Int, color: Int = M.attr.colorOnSurface) = TextView(this).apply {
    setTextAppearance(resourceOf(appearance))
    setTextColor(themeColor(color))
    this.text = text
}

enum class Look(val style: Int) {
    FILLED(M.attr.materialButtonStyle),
    TONAL(M.attr.materialButtonTonalStyle),
    TEXT(R.attr.plusTextButtonStyle),
}

fun Context.button(label: CharSequence, look: Look = Look.FILLED, icon: Int = 0, onClick: (View) -> Unit) =
    MaterialButton(this, null, look.style).apply {
        text = label
        isSingleLine = true
        if (icon != 0) {
            setIconResource(icon)
            iconGravity = MaterialButton.ICON_GRAVITY_TEXT_START  // beside the label, however wide
        }
        setOnClickListener(onClick)
    }

/** An icon button; its description is also a tooltip (the S Pen hovers over it). */
fun Context.iconButton(icon: Int, description: Int, style: Int = M.attr.materialIconButtonStyle,
                       onClick: (View) -> Unit) =
    MaterialButton(this, null, style).apply {
        setIconResource(icon)
        contentDescription = getString(description)
        tooltipText = contentDescription
        setOnClickListener(onClick)
    }

/** A rounded surface in a colour of the theme, for bars, popups and cards drawn by hand. */
fun Context.surface(color: Int = M.attr.colorSurfaceContainer, radius: Float = dp(28f)) =
    MaterialShapeDrawable(ShapeAppearanceModel.builder().setAllCornerSizes(radius).build()).apply {
        fillColor = ColorStateList.valueOf(themeColor(color))
    }

/** An icon from res/drawable in a colour of the theme. */
fun Context.icon(drawable: Int, color: Int = M.attr.colorOnSurfaceVariant, size: Int = 24) = ImageView(this).apply {
    setImageResource(drawable)
    imageTintList = ColorStateList.valueOf(themeColor(color))
    layoutParams = LinearLayout.LayoutParams(dp(size), dp(size))
}

/** A section's title over its group. */
fun Context.sectionTitle(text: CharSequence) = label(text, M.attr.textAppearanceTitleSmall, androidx.appcompat.R.attr.colorPrimary).apply {
    setPadding(dp(16), dp(28), dp(16), dp(10))
}

/** The theme's touch feedback, for views that draw themselves. */
fun Context.ripple(borderless: Boolean = false) = AppCompatResources.getDrawable(this, resourceOf(
    if (borderless) android.R.attr.selectableItemBackgroundBorderless else android.R.attr.selectableItemBackground))

/** A list row: a leading icon, a title and a supporting line, something at the end.
 * oneLine: a long title is cut in the middle (a file name's end tells it apart). */
fun Context.row(icon: Int, title: CharSequence, supporting: CharSequence? = null, trailing: View? = null,
                oneLine: Boolean = false) =
    LinearLayout(this).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        minimumHeight = dp(48)
        if (icon != 0) addView(icon(icon).apply {
            (layoutParams as LinearLayout.LayoutParams).marginEnd = dp(16)
        })
        addView(LinearLayout(context).apply {
            orientation = LinearLayout.VERTICAL
            addView(label(title, M.attr.textAppearanceBodyLarge).apply {
                if (oneLine) {
                    isSingleLine = true
                    ellipsize = android.text.TextUtils.TruncateAt.MIDDLE
                }
            })
            if (supporting != null) addView(label(supporting, M.attr.textAppearanceBodyMedium, M.attr.colorOnSurfaceVariant))
        }, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        if (trailing != null) addView(trailing, LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT,
            ViewGroup.LayoutParams.WRAP_CONTENT).apply { marginStart = dp(12) })
    }

/** Rows as one group, Android 16's way: a card each, the group's ends
 * well rounded, slightly where rows meet; a row with onClick ripples. */
fun Context.group(rows: List<Row>) = LinearLayout(this).apply {
    orientation = LinearLayout.VERTICAL
    rows.forEachIndexed { i, (content, onClick) ->
        val card = ListItemCardView(context, null, M.attr.listItemCardViewSegmentedStyle).apply {
            addView(content)
            if (onClick != null) setOnClickListener(onClick) else isClickable = false
        }
        val item = ListItemLayout(context).apply {
            addView(card)
            updateAppearance(i, rows.size)
        }
        addView(item, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT).apply { if (i > 0) topMargin = dp(2) })
    }
}

data class Row(val view: View, val onClick: ((View) -> Unit)? = null)


/** Whether one of the app's screens is in front: then it may open another
 * (the pairing question, CaptureActivity) without "display over other apps". */
object Front {
    private val count = java.util.concurrent.atomic.AtomicInteger()
    val shown get() = count.get() > 0
    fun started() = count.incrementAndGet()
    fun stopped() = count.decrementAndGet()
}
