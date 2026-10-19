package com.redharegames.nanocast.receiver

import android.app.Activity
import android.graphics.Color
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.util.TypedValue
import android.view.Gravity
import android.view.KeyEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.TextView
import android.widget.Toast
import java.net.Inet4Address
import java.net.NetworkInterface
import java.security.SecureRandom
import java.util.Locale

/**
 * Spike receiver: full-screen SurfaceView fed by UDP → FrameAssembler → MediaCodec,
 * system sound (UDP Opus → [AudioPlayer]), a TCP control channel (keyframe requests, stats,
 * audio on/off, pause / black screen, codec) and a small stats overlay in the top-left corner.
 *
 * Pause keeps the last decoded picture on the Surface and shows a small pause sign; black
 * screen covers the Surface with a black view. The optional 4-digit code is switched with
 * OK / Enter on the remote, kept in SharedPreferences and shown in the bottom-right corner.
 */
class MainActivity : Activity(), SurfaceHolder.Callback {
    private val stats = Stats()
    private lateinit var discovery: Discovery
    private lateinit var control: ControlServer
    private val audio = AudioPlayer()
    private lateinit var overlay: TextView
    @Volatile private var decoder: VideoDecoder? = null
    private var receiver: UdpReceiver? = null
    /** Last `start.codec`, applied to a decoder created later (surface recreated). */
    @Volatile private var codec = Protocol.CODEC_H264
    @Volatile private var paused = false
    /** The 4-digit code senders must give, null when the option is off. */
    @Volatile private var pin: String? = null
    private lateinit var pauseBadge: TextView
    private lateinit var blankView: View
    private lateinit var pinView: TextView

    private val ui = Handler(Looper.getMainLooper())
    private var ticks = 0
    private val tick = object : Runnable {
        override fun run() {
            updateOverlay()
            ui.postDelayed(this, OVERLAY_MS)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        discovery = Discovery(this)
        val prefs = getSharedPreferences(PREFS, MODE_PRIVATE)
        pin = if (prefs.getBoolean(PREF_PIN_ON, false)) prefs.getString(PREF_PIN, null) ?: newPin() else null
        discovery.setPin(pin != null)
        control = ControlServer(
            stats,
            { discovery.registeredName ?: discovery.deviceName() },
            audio::setEnabled,
            pin = { pin },
            onPause = { on ->
                paused = on
                decoder?.hold = on
                ui.post { pauseBadge.visibility = if (on) View.VISIBLE else View.GONE }
            },
            onBlank = { on -> ui.post { blankView.visibility = if (on) View.VISIBLE else View.GONE } },
            onCodec = { c ->
                codec = c
                decoder?.setCodec(c)
            },
        )
        audio.allow = { control.allows(it) }

        val surfaceView = SurfaceView(this).apply { holder.addCallback(this@MainActivity) }
        overlay = TextView(this).apply {
            setTextColor(Color.WHITE)
            setBackgroundColor(0x99000000.toInt())
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 9f)
            typeface = android.graphics.Typeface.MONOSPACE
            setPadding(16, 8, 16, 8)
        }
        val overlayParams = FrameLayout.LayoutParams(-2, -2, Gravity.TOP or Gravity.START)
        overlayParams.setMargins(24, 24, 0, 0)
        blankView = View(this).apply {
            setBackgroundColor(Color.BLACK)
            visibility = View.GONE
        }
        pauseBadge = TextView(this).apply {
            text = "⏸"
            setTextColor(Color.WHITE)
            setBackgroundColor(0x80000000.toInt())
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 28f)
            setPadding(24, 8, 24, 8)
            visibility = View.GONE
        }
        val badgeParams = FrameLayout.LayoutParams(-2, -2, Gravity.TOP or Gravity.END)
        badgeParams.setMargins(0, 32, 32, 0)
        pinView = TextView(this).apply {
            setTextColor(Color.WHITE)
            setBackgroundColor(0x99000000.toInt())
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 22f)
            typeface = android.graphics.Typeface.MONOSPACE
            setPadding(24, 8, 24, 8)
        }
        val pinParams = FrameLayout.LayoutParams(-2, -2, Gravity.BOTTOM or Gravity.END)
        pinParams.setMargins(0, 0, 32, 32)
        val root = FrameLayout(this)
        root.setBackgroundColor(Color.BLACK)
        root.addView(surfaceView, FrameLayout.LayoutParams(-1, -1))
        root.addView(overlay, overlayParams)
        // Above the stats overlay: a black screen is fully black (the code stays visible).
        root.addView(blankView, FrameLayout.LayoutParams(-1, -1))
        root.addView(pauseBadge, badgeParams)
        root.addView(pinView, pinParams)
        setContentView(root)
        showPin()
        @Suppress("DEPRECATION")
        root.systemUiVisibility = View.SYSTEM_UI_FLAG_FULLSCREEN or View.SYSTEM_UI_FLAG_HIDE_NAVIGATION or
            View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY or View.SYSTEM_UI_FLAG_LAYOUT_STABLE
    }

    override fun onStart() {
        super.onStart()
        discovery.register()
        try {
            control.start()
        } catch (e: Exception) {
            Log.e(TAG, "cannot bind TCP ${Protocol.CONTROL_PORT}", e)
        }
        try {
            audio.start()
        } catch (e: Exception) {
            Log.e(TAG, "cannot bind UDP ${Protocol.AUDIO_PORT}", e)
        }
        ui.post(tick)
    }

    override fun onStop() {
        ui.removeCallbacks(tick)
        audio.stop()
        control.stop()
        discovery.unregister()
        super.onStop()
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        val dec = VideoDecoder(holder.surface, stats) { reason -> control.requestKeyframe(reason) }
        dec.setCodec(codec)
        dec.hold = paused
        val assembler = FrameAssembler(stats, onLoss = { control.requestKeyframe("packet_loss") }) { id, data, completeNs ->
            dec.submit(id, data, completeNs)
        }
        val rx = UdpReceiver(Protocol.VIDEO_PORT, assembler) { control.allows(it) }
        try {
            rx.start()
        } catch (e: Exception) {
            Log.e(TAG, "cannot bind UDP ${Protocol.VIDEO_PORT}", e)
        }
        decoder = dec
        receiver = rx
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        receiver?.stop()
        receiver = null
        decoder?.release()
        decoder = null
    }

    /** OK / Enter on the remote: 4-digit code on (new code) / off. */
    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        when (keyCode) {
            KeyEvent.KEYCODE_DPAD_CENTER, KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER, KeyEvent.KEYCODE_BUTTON_A -> {
                if (event.repeatCount == 0) togglePin()
                return true
            }
        }
        return super.onKeyDown(keyCode, event)
    }

    private fun togglePin() {
        val code = if (pin == null) newPin() else null
        pin = code
        getSharedPreferences(PREFS, MODE_PRIVATE).edit()
            .putBoolean(PREF_PIN_ON, code != null)
            .apply { if (code != null) putString(PREF_PIN, code) }
            .apply()
        // The current sender has to say hello again (with the code, if one is needed now).
        control.disconnect()
        discovery.setPin(code != null)
        showPin()
        val msg = if (code != null) "Mã 4 số: $code / 4-digit code on" else "Đã tắt mã 4 số / 4-digit code off"
        Toast.makeText(this, msg, Toast.LENGTH_SHORT).show()
        Log.i(TAG, "4-digit code ${if (code != null) "on" else "off"}")
    }

    private fun showPin() {
        val code = pin
        pinView.text = if (code != null) "Mã / Code  $code" else ""
        pinView.visibility = if (code != null) View.VISIBLE else View.GONE
    }

    private fun newPin(): String = String.format(Locale.US, "%04d", SecureRandom().nextInt(10_000))

    private fun updateOverlay() {
        val s = stats.snapshot()
        val audioKbps = audio.kbps()
        val text = buildString {
            append("NANOCast  ").append(localIps()).append(" udp:").append(Protocol.VIDEO_PORT)
                .append(" tcp:").append(Protocol.CONTROL_PORT).append(" audio:").append(Protocol.AUDIO_PORT).append('\n')
            append(discovery.status).append('\n')
            append(control.status).append("  kf req ").append(control.keyframeRequests.get())
            if (pin != null) append("  code refused ").append(control.rejected.get())
            append('\n')
            append("decoder ").append(stats.codecName)
            if (stats.codecInfo.isNotEmpty()) append(" (").append(stats.codecInfo).append(')')
            append("  ").append(stats.videoSize).append('\n')
            append(fmt("fps %.1f (in %.1f)  %.1f Mbps", s.fpsOut, s.fpsIn, s.mbps)).append('\n')
            append(fmt("lost pkts %d  dropped frames %d  dec err %d",
                stats.lostPackets.get(), stats.droppedFrames.get(), stats.decoderErrors.get())).append('\n')
            append(fmt("decode ms  avg %.2f  p95 %.2f   (session p50 %.1f p95 %.1f p99 %.1f, n=%d)",
                s.windowAvgMs, s.windowP95Ms, s.sessionP50Ms, s.sessionP95Ms, s.sessionP99Ms, s.sessionN)).append('\n')
            append(fmt("%.0f kbps  ", audioKbps)).append(audio.overlayLine()).append('\n')
            append("OK: mã 4 số bật/tắt (4-digit code on/off)  codecs ").append(VideoDecoder.supportedCodecs.joinToString(","))
        }
        overlay.text = text

        // Periodic logcat line so numbers can be collected with `adb logcat -s NANOCast.Stats`.
        if (++ticks % (LOG_MS / OVERLAY_MS).toInt() == 0 && s.framesIn > 0) {
            Log.i("NANOCast.Stats", fmt(
                "codec=%s [%s] size=%s fps_out=%.1f fps_in=%.1f mbps=%.1f lost_pkts=%d dropped=%d " +
                    "dec_err=%d frames_in=%d frames_out=%d assembly_avg=%.2fms " +
                    "decode_win avg=%.2f p95=%.2f | session n=%d avg=%.2f p50=%.1f p95=%.1f p99=%.1f",
                stats.codecName, stats.codecInfo, stats.videoSize, s.fpsOut, s.fpsIn, s.mbps,
                stats.lostPackets.get(), stats.droppedFrames.get(), stats.decoderErrors.get(),
                s.framesIn, s.framesOut, s.avgAssemblyMs, s.windowAvgMs, s.windowP95Ms,
                s.sessionN, s.sessionAvgMs, s.sessionP50Ms, s.sessionP95Ms, s.sessionP99Ms,
            ))
        }
        if (ticks % (LOG_MS / OVERLAY_MS).toInt() == 0 && audio.packets.get() > 0) {
            Log.i("NANOCast.Stats", fmt("audio kbps=%.1f pkts=%d ", audioKbps, audio.packets.get()) + audio.overlayLine())
        }
    }

    private fun localIps(): String = try {
        NetworkInterface.getNetworkInterfaces().toList()
            .filter { it.isUp && !it.isLoopback }
            .flatMap { it.inetAddresses.toList() }
            .filterIsInstance<Inet4Address>()
            .joinToString(",") { it.hostAddress ?: "" }
            .ifEmpty { "no network" }
    } catch (e: Exception) {
        "?"
    }

    private fun fmt(f: String, vararg args: Any?) = String.format(Locale.US, f, *args)

    private companion object {
        const val TAG = "NANOCast"
        const val PREFS = "nanocast"
        const val PREF_PIN_ON = "pin_on"
        const val PREF_PIN = "pin"
        const val OVERLAY_MS = 500L
        const val LOG_MS = 5000L
    }
}
