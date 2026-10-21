package com.redharegames.nanocast.receiver

import android.animation.ValueAnimator
import android.content.Context
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.animation.DecelerateInterpolator
import android.widget.FrameLayout
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView

/**
 * Full-screen waiting screen, shown whenever no picture plays: app icon with a slow pulse,
 * what the receiver is waiting for, the TV name and address to look for on the PC, the
 * 4-digit code when it is on, and the remote keys. Covers the Surface (and the last decoded
 * picture) completely; [MainActivity] fades it out once the first frame of a cast is up.
 */
class IdleView(context: Context) : FrameLayout(context) {
    enum class Phase { WAITING, CONNECTED, RECEIVING }

    /** Laid out for a 1080p TV (960×540 dp); smaller screens (phones, 720p boxes at high
     *  density) get everything scaled down so it still fits. */
    private val scale = resources.displayMetrics.let { m ->
        val long = maxOf(m.widthPixels, m.heightPixels) / m.density
        val short = minOf(m.widthPixels, m.heightPixels) / m.density
        minOf(1f, long / 960f, short / 540f)
    }
    private val status = text(22f, Color.WHITE, MEDIUM)
    private val hint = text(15f, MUTED, REGULAR)
    private val nameValue = text(19f, Color.WHITE, MEDIUM)
    private val addressValue = text(19f, Color.WHITE, MEDIUM)
    private val codeValue = text(19f, ACCENT_LIGHT, MEDIUM).apply { typeface = Typeface.MONOSPACE }
    private val codeColumn: LinearLayout
    private val codeKey = text(13f, MUTED, REGULAR)
    private val rings = List(2) { View(context) }
    private val pulses = mutableListOf<ValueAnimator>()
    /** Whether it is (or is fading to) visible; [show] does nothing when unchanged. */
    private var shownNow = true

    init {
        background = GradientDrawable().apply {
            gradientType = GradientDrawable.RADIAL_GRADIENT
            colors = intArrayOf(0xFF17264A.toInt(), 0xFF0A1020.toInt(), 0xFF05070D.toInt())
            gradientRadius = context.resources.displayMetrics.widthPixels * 0.75f
            setGradientCenter(0.5f, 0.42f)
        }
        // Never takes focus: OK / ▼ on the remote must reach MainActivity.onKeyDown.
        isFocusable = false

        val pulseBox = FrameLayout(context)
        for (ring in rings) {
            ring.background = GradientDrawable().apply {
                shape = GradientDrawable.OVAL
                setColor(0x333B82F6)
                setStroke(dp(2f).toInt(), 0x663B82F6)
            }
            ring.alpha = 0f
            pulseBox.addView(ring, LayoutParams(dp(96f).toInt(), dp(96f).toInt(), Gravity.CENTER))
        }
        val icon = ImageView(context).apply { setImageResource(R.mipmap.ic_launcher) }
        pulseBox.addView(icon, LayoutParams(dp(88f).toInt(), dp(88f).toInt(), Gravity.CENTER))

        val title = text(36f, Color.WHITE, MEDIUM).apply {
            text = context.getString(R.string.app_name)
            letterSpacing = 0.02f
        }

        nameValue.maxLines = 1
        nameValue.ellipsize = android.text.TextUtils.TruncateAt.END
        nameValue.maxWidth = dp(320f).toInt()
        val card = LinearLayout(context).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            background = GradientDrawable().apply {
                cornerRadius = dp(18f)
                setColor(0x14FFFFFF)
                setStroke(dp(1f).toInt(), 0x22FFFFFF)
            }
            val px = dp(28f).toInt()
            val py = dp(16f).toInt()
            setPadding(px, py, px, py)
        }
        card.addView(column(R.string.label_name, nameValue))
        card.addView(divider())
        card.addView(column(R.string.label_address, addressValue))
        codeColumn = LinearLayout(context).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            addView(divider())
            addView(column(R.string.label_code, codeValue))
        }
        card.addView(codeColumn)

        val center = LinearLayout(context).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER_HORIZONTAL
            addView(pulseBox, LinearLayout.LayoutParams(dp(160f).toInt(), dp(160f).toInt()))
            addView(title, spaced(4f))
            addView(status, spaced(18f))
            addView(hint, spaced(8f))
            addView(card, spaced(32f))
        }
        status.gravity = Gravity.CENTER
        hint.gravity = Gravity.CENTER
        hint.maxWidth = dp(560f).toInt()
        addView(center, LayoutParams(-2, -2, Gravity.CENTER))

        val stats = text(13f, MUTED, REGULAR).apply { setText(R.string.key_stats) }
        val footer = LinearLayout(context).apply {
            orientation = LinearLayout.HORIZONTAL
            addView(codeKey)
            addView(text(13f, 0x55FFFFFF, REGULAR).apply { text = "   ·   " })
            addView(stats)
        }
        addView(footer, LayoutParams(-2, -2, Gravity.BOTTOM or Gravity.CENTER_HORIZONTAL).apply {
            bottomMargin = dp(28f).toInt()
        })

        val version = text(12f, 0x66FFFFFF, REGULAR).apply { text = "v${BuildConfig.VERSION_NAME}" }
        addView(version, LayoutParams(-2, -2, Gravity.TOP or Gravity.END).apply {
            topMargin = dp(24f).toInt()
            rightMargin = dp(32f).toInt()
        })
    }

    /** Refreshes the texts; cheap to call on every overlay tick (unchanged texts are skipped). */
    fun bind(phase: Phase, sender: String?, name: String, address: String, pin: String?) {
        val who = sender ?: "?"
        val (s, h) = when (phase) {
            Phase.WAITING -> context.getString(R.string.waiting) to context.getString(R.string.waiting_hint)
            Phase.CONNECTED -> context.getString(R.string.connected, who) to context.getString(R.string.connected_hint)
            Phase.RECEIVING -> context.getString(R.string.receiving, who) to context.getString(R.string.receiving_hint)
        }
        set(status, s)
        set(hint, h)
        set(nameValue, name)
        set(addressValue, address)
        set(codeValue, pin ?: "")
        codeColumn.visibility = if (pin != null) View.VISIBLE else View.GONE
        set(codeKey, context.getString(if (pin != null) R.string.key_code_off else R.string.key_code_on))
    }

    /** Fades in / out; the pulse only runs while it is visible. */
    fun show(on: Boolean) {
        if (on == shownNow) return
        shownNow = on
        animate().cancel()
        if (on) {
            visibility = View.VISIBLE
            animate().alpha(1f).setDuration(FADE_MS).start()
            startPulse()
        } else {
            animate().alpha(0f).setDuration(FADE_MS).withEndAction {
                visibility = View.GONE
                stopPulse()
            }.start()
        }
    }

    override fun onAttachedToWindow() {
        super.onAttachedToWindow()
        if (shownNow) startPulse()
    }

    override fun onDetachedFromWindow() {
        stopPulse()
        super.onDetachedFromWindow()
    }

    private fun startPulse() {
        if (pulses.isNotEmpty()) return
        rings.forEachIndexed { i, ring ->
            pulses += ValueAnimator.ofFloat(0f, 1f).apply {
                duration = PULSE_MS
                startDelay = PULSE_MS / rings.size * i
                repeatCount = ValueAnimator.INFINITE
                interpolator = DecelerateInterpolator()
                addUpdateListener {
                    val f = it.animatedValue as Float
                    ring.scaleX = 0.9f + f
                    ring.scaleY = 0.9f + f
                    ring.alpha = 1f - f
                }
                start()
            }
        }
    }

    private fun stopPulse() {
        pulses.forEach { it.cancel() }
        pulses.clear()
        rings.forEach { it.alpha = 0f }
    }

    private fun column(label: Int, value: TextView) = LinearLayout(context).apply {
        orientation = LinearLayout.VERTICAL
        addView(text(12f, MUTED, REGULAR).apply {
            setText(label)
            isAllCaps = true
            letterSpacing = 0.08f
        })
        addView(value)
    }

    private fun divider() = View(context).apply {
        setBackgroundColor(0x22FFFFFF)
        layoutParams = LinearLayout.LayoutParams(dp(1f).toInt(), dp(40f).toInt()).apply {
            leftMargin = dp(28f).toInt()
            rightMargin = dp(28f).toInt()
        }
    }

    private fun spaced(topDp: Float) = LinearLayout.LayoutParams(-2, -2).apply { topMargin = dp(topDp).toInt() }

    private fun text(sp: Float, color: Int, face: Typeface) = TextView(context).apply {
        setTextSize(TypedValue.COMPLEX_UNIT_SP, sp * scale)
        setTextColor(color)
        typeface = face
    }

    private fun set(view: TextView, value: String) {
        if (view.text.toString() != value) view.text = value
    }

    private fun dp(v: Float) = TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, v * scale, resources.displayMetrics)

    private companion object {
        val MEDIUM: Typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
        val REGULAR: Typeface = Typeface.create("sans-serif", Typeface.NORMAL)
        const val MUTED = 0xFF9AA7BD.toInt()
        const val ACCENT_LIGHT = 0xFF93C5FD.toInt()
        const val FADE_MS = 250L
        const val PULSE_MS = 2400L
    }
}
