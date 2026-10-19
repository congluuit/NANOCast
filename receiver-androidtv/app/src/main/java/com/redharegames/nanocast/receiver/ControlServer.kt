package com.redharegames.nanocast.receiver

import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.util.Log
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject
import java.io.BufferedReader
import java.io.IOException
import java.io.InputStreamReader
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.concurrent.Executors
import java.util.concurrent.RejectedExecutionException
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong

/**
 * Receiver side of the control channel (PLAN.md §4, mirrors `crates/core/src/control.rs`):
 * TCP [Protocol.CONTROL_PORT], one JSON object per line, message type in field "t".
 *
 *   sender → receiver: hello {ver, name} | start {w, h, fps, codec, audio} | audio {on} | stop
 *   receiver → sender: caps {ver, name, max_w, max_h, codecs, low_latency}
 *                    | keyframe_request {reason} | stats {fps, lost, decode_ms}
 *
 * One sender at a time: a new connection replaces the previous one (the PC may have gone
 * away without closing). `stats` goes out every second (`lost` = packets lost since the
 * previous report); `keyframe_request` at most once per 500 ms. Every write runs on one
 * executor thread, so the network and decoder threads never block on TCP.
 */
class ControlServer(
    private val stats: Stats,
    private val deviceName: () -> String,
    /** `audio {on}`, `start {audio}`; `stop` and a lost sender count as off. */
    private val onAudio: (Boolean) -> Unit = {},
) {
    private val lock = Any()
    private var server: ServerSocket? = null
    private var client: Socket? = null
    private var acceptThread: Thread? = null
    @Volatile private var tx: ScheduledExecutorService? = null
    @Volatile private var connected = false

    private val lastKeyframeNs = AtomicLong(0)
    val keyframeRequests = AtomicLong()

    @Volatile var status = "ctrl: off"
        private set

    // Previous stats report; touched only on the tx thread.
    private var lastStatsNs = 0L
    private var lastFramesOut = 0L
    private var lastLost = 0L

    fun start() {
        if (server != null) return
        val s = ServerSocket()
        s.reuseAddress = true
        s.bind(InetSocketAddress(Protocol.CONTROL_PORT))
        server = s
        val exec = Executors.newSingleThreadScheduledExecutor { r -> Thread(r, "nanocast-ctrl-tx") }
        exec.scheduleWithFixedDelay(::sendStats, STATS_MS, STATS_MS, TimeUnit.MILLISECONDS)
        tx = exec
        status = "ctrl: tcp ${Protocol.CONTROL_PORT} waiting"
        acceptThread = Thread({ acceptLoop(s) }, "nanocast-ctrl").apply { start() }
        Log.i(TAG, "listening on TCP ${Protocol.CONTROL_PORT}")
    }

    fun stop() {
        tx?.shutdownNow()
        tx = null
        server?.close()
        server = null
        synchronized(lock) {
            closeQuietly(client)
            client = null
            connected = false
        }
        acceptThread?.join(500)
        acceptThread = null
        status = "ctrl: off"
    }

    /** Asks the sender for an IDR. Cheap and thread-safe; rate-limited to one per 500 ms. */
    fun requestKeyframe(reason: String) {
        if (!connected) return
        val now = System.nanoTime()
        val last = lastKeyframeNs.get()
        if (last != 0L && now - last < KEYFRAME_MIN_INTERVAL_NS) return
        if (!lastKeyframeNs.compareAndSet(last, now)) return
        keyframeRequests.incrementAndGet()
        send(JSONObject().put("t", "keyframe_request").put("reason", reason))
        Log.i(TAG, "keyframe_request ($reason)")
    }

    private fun acceptLoop(s: ServerSocket) {
        while (true) {
            val sock = try {
                s.accept()
            } catch (e: IOException) {
                break // closed by stop()
            }
            sock.tcpNoDelay = true
            sock.keepAlive = true
            synchronized(lock) {
                closeQuietly(client) // its reader thread ends on the closed socket
                client = sock
                connected = true
            }
            lastKeyframeNs.set(0)
            Thread({ readLoop(sock) }, "nanocast-ctrl-rx").start()
        }
    }

    private fun readLoop(sock: Socket) {
        val peer = sock.inetAddress?.hostAddress ?: "?"
        Log.i(TAG, "sender connected from $peer")
        status = "ctrl: $peer connected"
        var sender = peer
        try {
            val reader = BufferedReader(InputStreamReader(sock.getInputStream(), Charsets.UTF_8))
            while (true) {
                val line = reader.readLine() ?: break
                if (line.isBlank()) continue
                val msg = try {
                    JSONObject(line)
                } catch (e: JSONException) {
                    Log.w(TAG, "bad control line: $line")
                    continue
                }
                when (val t = msg.optString("t")) {
                    "hello" -> {
                        sender = msg.optString("name").ifEmpty { peer }
                        Log.i(TAG, "hello from $sender (v${msg.optString("ver")})")
                        status = "ctrl: $sender ($peer)"
                        send(caps())
                    }
                    "start" -> {
                        val desc = "${msg.optInt("w")}x${msg.optInt("h")}@${msg.optInt("fps")} " +
                            "${msg.optString("codec")} audio ${if (msg.optBoolean("audio")) "on" else "off"}"
                        Log.i(TAG, "start: $desc")
                        status = "ctrl: $sender ($peer) $desc"
                        onAudio(msg.optBoolean("audio"))
                    }
                    "audio" -> {
                        val on = msg.optBoolean("on")
                        Log.i(TAG, "audio ${if (on) "on" else "off"}")
                        onAudio(on)
                    }
                    "stop" -> {
                        Log.i(TAG, "stop from $sender")
                        onAudio(false)
                        status = "ctrl: $sender ($peer) stopped"
                    }
                    else -> Log.w(TAG, "unknown control message type '$t'")
                }
            }
        } catch (e: IOException) {
            // Closed by stop(), replaced by a newer sender, or the network dropped.
        } finally {
            closeQuietly(sock)
            synchronized(lock) {
                if (client === sock) {
                    client = null
                    connected = false
                    onAudio(false)
                    status = "ctrl: tcp ${Protocol.CONTROL_PORT} waiting (last: $sender)"
                }
            }
            Log.i(TAG, "sender $sender disconnected")
        }
    }

    private fun caps(): JSONObject = JSONObject()
        .put("t", "caps")
        .put("ver", Protocol.VERSION)
        .put("name", deviceName())
        .put("max_w", Protocol.WIDTH)
        .put("max_h", Protocol.HEIGHT)
        .put("codecs", JSONArray().put("h264"))
        .put("low_latency", lowLatencyDecoder)

    /** Whether the decoder MediaCodec would pick for our stream advertises FEATURE_LowLatency. */
    private val lowLatencyDecoder: Boolean by lazy {
        if (Build.VERSION.SDK_INT < 30) return@lazy false
        try {
            val list = MediaCodecList(MediaCodecList.REGULAR_CODECS)
            val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, Protocol.WIDTH, Protocol.HEIGHT)
            val name = list.findDecoderForFormat(format) ?: return@lazy false
            list.codecInfos.first { it.name == name }
                .getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC)
                .isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency)
        } catch (e: Exception) {
            Log.w(TAG, "low-latency probe failed", e)
            false
        }
    }

    /** Runs every second on the tx thread. */
    private fun sendStats() {
        val now = System.nanoTime()
        val frames = stats.framesOutTotal()
        val lost = stats.lostPackets.get()
        if (lastStatsNs != 0L && connected) {
            val fps = (frames - lastFramesOut) / ((now - lastStatsNs) / 1e9)
            // No frames this interval: report 0 rather than the stale average of old frames.
            val decodeMs = if (frames == lastFramesOut) 0.0 else stats.windowAvgMs()
            val msg = JSONObject()
                .put("t", "stats")
                .put("fps", Math.round(fps * 10) / 10.0)
                .put("lost", (lost - lastLost).coerceAtLeast(0))
                .put("decode_ms", Math.round(decodeMs * 100) / 100.0)
            write(msg)
        }
        lastStatsNs = now
        lastFramesOut = frames
        lastLost = lost
    }

    private fun send(msg: JSONObject) {
        try {
            tx?.execute { write(msg) }
        } catch (e: RejectedExecutionException) {
            // stopping
        }
    }

    /** tx thread only. A failed write closes the socket; the reader thread then cleans up. */
    private fun write(msg: JSONObject) {
        val sock = synchronized(lock) { client } ?: return
        try {
            sock.getOutputStream().write((msg.toString() + "\n").toByteArray(Charsets.UTF_8))
        } catch (e: IOException) {
            Log.w(TAG, "control write failed: $e")
            closeQuietly(sock)
        }
    }

    private fun closeQuietly(s: Socket?) {
        try {
            s?.close()
        } catch (_: IOException) {
        }
    }

    private companion object {
        const val TAG = "NANOCast.Ctrl"
        const val STATS_MS = 1000L
        const val KEYFRAME_MIN_INTERVAL_NS = 500_000_000L
    }
}
