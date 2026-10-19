package com.redharegames.nanocast.receiver

import android.app.Activity
import android.graphics.Color
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.util.TypedValue
import android.view.Gravity
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.TextView
import java.net.Inet4Address
import java.net.NetworkInterface
import java.util.Locale

/**
 * Spike receiver: full-screen SurfaceView fed by UDP → FrameAssembler → MediaCodec,
 * a TCP control channel (keyframe requests, stats) and a small stats overlay in the top-left corner.
 */
class MainActivity : Activity(), SurfaceHolder.Callback {
    private val stats = Stats()
    private lateinit var discovery: Discovery
    private lateinit var control: ControlServer
    private lateinit var overlay: TextView
    private var decoder: VideoDecoder? = null
    private var receiver: UdpReceiver? = null

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
        control = ControlServer(stats) { discovery.registeredName ?: discovery.deviceName() }

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
        val root = FrameLayout(this)
        root.setBackgroundColor(Color.BLACK)
        root.addView(surfaceView, FrameLayout.LayoutParams(-1, -1))
        root.addView(overlay, overlayParams)
        setContentView(root)
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
        ui.post(tick)
    }

    override fun onStop() {
        ui.removeCallbacks(tick)
        control.stop()
        discovery.unregister()
        super.onStop()
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        val dec = VideoDecoder(holder.surface, stats) { reason -> control.requestKeyframe(reason) }
        val assembler = FrameAssembler(stats, onLoss = { control.requestKeyframe("packet_loss") }) { id, data, completeNs ->
            dec.submit(id, data, completeNs)
        }
        val rx = UdpReceiver(Protocol.VIDEO_PORT, assembler)
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

    private fun updateOverlay() {
        val s = stats.snapshot()
        val text = buildString {
            append("NANOCast  ").append(localIps()).append(" udp:").append(Protocol.VIDEO_PORT)
                .append(" tcp:").append(Protocol.CONTROL_PORT).append('\n')
            append(discovery.status).append('\n')
            append(control.status).append("  kf req ").append(control.keyframeRequests.get()).append('\n')
            append("decoder ").append(stats.codecName)
            if (stats.codecInfo.isNotEmpty()) append(" (").append(stats.codecInfo).append(')')
            append("  ").append(stats.videoSize).append('\n')
            append(fmt("fps %.1f (in %.1f)  %.1f Mbps", s.fpsOut, s.fpsIn, s.mbps)).append('\n')
            append(fmt("lost pkts %d  dropped frames %d  dec err %d",
                stats.lostPackets.get(), stats.droppedFrames.get(), stats.decoderErrors.get())).append('\n')
            append(fmt("decode ms  avg %.2f  p95 %.2f   (session p50 %.1f p95 %.1f p99 %.1f, n=%d)",
                s.windowAvgMs, s.windowP95Ms, s.sessionP50Ms, s.sessionP95Ms, s.sessionP99Ms, s.sessionN))
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
        const val OVERLAY_MS = 500L
        const val LOG_MS = 5000L
    }
}
