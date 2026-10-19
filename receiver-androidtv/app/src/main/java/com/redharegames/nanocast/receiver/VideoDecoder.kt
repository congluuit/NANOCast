package com.redharegames.nanocast.receiver

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer

/**
 * Hardware H.264 decoder rendering straight to a Surface, tuned for latency:
 * one access unit per input buffer, output released for rendering the moment it appears.
 *
 * Uses MediaCodec's async mode on a dedicated thread. [submit] may be called from the
 * network thread; frames wait in a small queue only if the codec has no free input buffer.
 * The codec is created lazily from the first access unit that carries SPS + PPS.
 *
 * [onNeedKeyframe] is called (with a reason) whenever frames are being discarded while waiting
 * for an IDR: no decoder yet / after a codec error, or after the backlog was flushed. It is
 * called under [lock] and must only hand off (the control channel rate-limits it).
 */
class VideoDecoder(
    private val surface: Surface,
    private val stats: Stats,
    private val onNeedKeyframe: (reason: String) -> Unit = {},
) {
    private val thread = HandlerThread("nanocast-decoder", android.os.Process.THREAD_PRIORITY_URGENT_DISPLAY)
        .apply { start() }
    private val handler = Handler(thread.looper)

    private val lock = Any()
    private var codec: MediaCodec? = null
    private var generation = 0
    private var starting = false
    private var needKeyframe = false
    /** True from codec start until the startup backlog has drained: those latencies measure catch-up, not decode. */
    private var warmingUp = true
    private val freeInputs = ArrayDeque<Int>()
    private val pending = ArrayDeque<Pair<Long, ByteArray>>()

    /** completeNs per frame, indexed by frameId % size (frameId is stored to detect stale slots). */
    private val arrivalNs = LongArray(512)
    private val arrivalId = LongArray(512) { -1 }

    fun submit(frameId: Long, data: ByteArray, completeNs: Long) {
        synchronized(lock) {
            if (codec == null) {
                if (!starting) {
                    val sps = H264.findNal(data, 7)
                    val pps = H264.findNal(data, 8)
                    if (sps == null || pps == null) {
                        stats.droppedFrames.incrementAndGet() // waiting for a keyframe with SPS/PPS
                        onNeedKeyframe("decoder_start")
                        return
                    }
                    // Creating a codec takes 100+ ms: do it on the decoder thread so the network
                    // thread keeps draining the socket (otherwise the receive buffer overflows).
                    starting = true
                    val gen = generation
                    handler.post { startOnDecoderThread(gen, sps, pps) }
                }
                // Queued during startup: kept for decoding but excluded from latency stats.
                if (pending.size < MAX_PENDING) pending.addLast(frameId to data) else stats.droppedFrames.incrementAndGet()
                return
            }
            if (needKeyframe) {
                if (H264.findNal(data, 5) == null) {
                    stats.droppedFrames.incrementAndGet()
                    onNeedKeyframe("decoder_backlog") // repeats (rate-limited) in case the IDR got lost
                    return
                }
                needKeyframe = false
            }
            if (pending.size >= MAX_PENDING) {
                // Decoder is ~1 s behind: drop the backlog and resume at the next IDR rather than
                // keep growing latency (decoding past a gap would only show corrupted frames).
                // The IDR is requested over the control channel instead of waiting for the GOP.
                stats.droppedFrames.addAndGet(pending.size.toLong())
                pending.clear()
                needKeyframe = H264.findNal(data, 5) == null
                if (needKeyframe) {
                    stats.droppedFrames.incrementAndGet()
                    onNeedKeyframe("decoder_backlog")
                    return
                }
            }
            if (!warmingUp) {
                val slot = (frameId % arrivalNs.size).toInt()
                arrivalNs[slot] = completeNs
                arrivalId[slot] = frameId
            }
            pending.addLast(frameId to data)
            feedLocked()
        }
    }

    private fun feedLocked() {
        val c = codec ?: return
        while (freeInputs.isNotEmpty() && pending.isNotEmpty()) {
            val index = freeInputs.removeFirst()
            val (frameId, data) = pending.removeFirst()
            try {
                val buf: ByteBuffer = c.getInputBuffer(index) ?: continue
                buf.clear()
                if (data.size > buf.capacity()) {
                    Log.w(TAG, "AU ${data.size} B > input buffer ${buf.capacity()} B, dropped")
                    c.queueInputBuffer(index, 0, 0, frameId * FRAME_US, 0)
                    stats.droppedFrames.incrementAndGet()
                    continue
                }
                buf.put(data)
                c.queueInputBuffer(index, 0, data.size, frameId * FRAME_US, 0)
            } catch (e: IllegalStateException) {
                Log.e(TAG, "queueInputBuffer failed", e)
                stats.decoderErrors.incrementAndGet()
                return
            }
        }
        if (pending.isEmpty()) warmingUp = false
    }

    /** Runs on the decoder thread without holding [lock]. */
    private fun startOnDecoderThread(gen: Int, sps: ByteArray, pps: ByteArray) {
        val c = try {
            createCodec(gen, sps, pps)
        } catch (e: Exception) {
            Log.e(TAG, "decoder start failed", e)
            stats.decoderErrors.incrementAndGet()
            null
        }
        synchronized(lock) {
            if (gen == generation) starting = false
            if (c == null || gen != generation) {
                c?.release() // released while starting
                pending.clear()
                return
            }
            codec = c
            warmingUp = true
            feedLocked()
        }
        describe(c ?: return)
        Log.i(TAG, "decoder started: ${stats.codecName} ${stats.codecInfo}")
    }

    private fun createCodec(gen: Int, sps: ByteArray, pps: ByteArray): MediaCodec {
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, Protocol.WIDTH, Protocol.HEIGHT).apply {
            setByteBuffer("csd-0", ByteBuffer.wrap(sps))
            setByteBuffer("csd-1", ByteBuffer.wrap(pps))
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 2 * 1024 * 1024)
            setInteger(MediaFormat.KEY_PRIORITY, 0) // realtime
            setInteger(MediaFormat.KEY_OPERATING_RATE, Protocol.FPS)
            if (Build.VERSION.SDK_INT >= 30) setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        }

        val c = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        try {
            c.setCallback(Callback(gen), handler)
            try {
                c.configure(format, surface, null, 0)
            } catch (e: Exception) {
                // Some decoders reject unknown keys; retry with the bare format.
                Log.w(TAG, "configure with low-latency keys failed, retrying plain", e)
                c.reset()
                c.setCallback(Callback(gen), handler)
                format.removeKey(MediaFormat.KEY_PRIORITY)
                format.removeKey(MediaFormat.KEY_OPERATING_RATE)
                if (Build.VERSION.SDK_INT >= 30) format.removeKey(MediaFormat.KEY_LOW_LATENCY)
                c.configure(format, surface, null, 0)
            }
            c.setVideoScalingMode(MediaCodec.VIDEO_SCALING_MODE_SCALE_TO_FIT)
            c.start()
        } catch (e: Exception) {
            c.release()
            throw e
        }
        return c
    }

    private fun describe(c: MediaCodec) {
        stats.codecName = c.name
        val info = c.codecInfo
        val parts = mutableListOf<String>()
        if (Build.VERSION.SDK_INT >= 29) {
            parts += if (info.isHardwareAccelerated) "HW" else "SW"
        }
        if (Build.VERSION.SDK_INT >= 30) {
            val caps = info.getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC)
            val ll = caps.isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency)
            parts += "low-latency " + if (ll) "yes" else "no"
        } else {
            parts += "API ${Build.VERSION.SDK_INT}: no KEY_LOW_LATENCY"
        }
        stats.codecInfo = parts.joinToString(", ")
    }

    private inner class Callback(private val gen: Int) : MediaCodec.Callback() {
        override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {
            synchronized(lock) {
                if (gen != generation) return
                freeInputs.addLast(index)
                feedLocked()
            }
        }

        override fun onOutputBufferAvailable(codec: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
            val now = System.nanoTime()
            val frameId = (info.presentationTimeUs + FRAME_US / 2) / FRAME_US
            try {
                codec.releaseOutputBuffer(index, info.size > 0)
            } catch (e: IllegalStateException) {
                return
            }
            if (info.size == 0) return
            val slot = (frameId % arrivalNs.size).toInt()
            val latency = synchronized(lock) {
                if (arrivalId[slot] == frameId) now - arrivalNs[slot] else -1L
            }
            stats.onDecoded(latency)
        }

        override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {
            val w = format.getInteger(MediaFormat.KEY_WIDTH)
            val h = format.getInteger(MediaFormat.KEY_HEIGHT)
            stats.videoSize = "${w}x$h"
            Log.i(TAG, "output format: $format")
        }

        override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
            Log.e(TAG, "codec error (recoverable=${e.isRecoverable}, transient=${e.isTransient})", e)
            stats.decoderErrors.incrementAndGet()
            if (e.isTransient) return
            // Tear down; the next keyframe with SPS/PPS recreates the decoder.
            synchronized(lock) {
                if (gen == generation) releaseLocked()
            }
        }
    }

    private fun releaseLocked() {
        generation++
        starting = false
        needKeyframe = false
        freeInputs.clear()
        pending.clear()
        codec?.let {
            try { it.stop() } catch (_: Exception) {}
            it.release()
        }
        codec = null
    }

    fun release() {
        synchronized(lock) { releaseLocked() }
        thread.quitSafely()
    }

    private companion object {
        const val TAG = "NANOCast.Decoder"
        const val FRAME_US = 1_000_000L / 60 // synthetic pts: frame_id * 16666 µs
        const val MAX_PENDING = 60 // ~1 s at 60 fps
    }
}

/** Minimal Annex B helpers. */
object H264 {
    /** Returns the first NAL of [type] including its 4-byte start code, or null. */
    fun findNal(au: ByteArray, type: Int): ByteArray? {
        var i = 0
        while (true) {
            val start = nextStartCode(au, i) ?: return null
            val hdr = start.second
            if (hdr >= au.size) return null
            val end = nextStartCode(au, hdr)?.first ?: au.size
            if ((au[hdr].toInt() and 0x1F) == type) {
                val nal = ByteArray(4 + end - hdr)
                nal[3] = 1
                System.arraycopy(au, hdr, nal, 4, end - hdr)
                return nal
            }
            i = hdr
        }
    }

    /** (index of the start code, index of the first byte after it) or null. */
    private fun nextStartCode(b: ByteArray, from: Int): Pair<Int, Int>? {
        var i = from
        while (i + 2 < b.size) {
            if (b[i].toInt() == 0 && b[i + 1].toInt() == 0) {
                if (b[i + 2].toInt() == 1) {
                    val s = if (i > from && b[i - 1].toInt() == 0) i - 1 else i
                    return s to i + 3
                }
                if (b[i + 2].toInt() != 0) i += 2 else i++
            } else {
                i++
            }
        }
        return null
    }
}
