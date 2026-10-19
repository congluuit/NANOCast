package com.redharegames.nanocast.receiver

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer

/**
 * Hardware H.264 / HEVC decoder rendering straight to a Surface, tuned for latency:
 * one access unit per input buffer, output released for rendering the moment it appears.
 * The codec follows `start.codec` ([setCodec]); HEVC is only offered in `caps` when this
 * device has an HEVC decoder ([hasDecoder]).
 *
 * Uses MediaCodec's async mode on a dedicated thread. [submit] may be called from the
 * network thread; frames wait in a small queue only if the codec has no free input buffer.
 * The codec is created lazily from the first access unit that carries the parameter sets
 * (H.264: SPS + PPS; HEVC: VPS + SPS + PPS).
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

    /** `pause {on}`: keep decoding but do not render, so the last picture stays on the Surface. */
    @Volatile var hold = false

    /** MediaCodec MIME type of the stream (`start.codec`). */
    private var mime = MediaFormat.MIMETYPE_VIDEO_AVC

    /** Switches to the codec named in `start` ("h264" / "hevc"); a change recreates the decoder. */
    fun setCodec(codec: String) {
        val m = if (codec.equals(Protocol.CODEC_HEVC, ignoreCase = true)) MediaFormat.MIMETYPE_VIDEO_HEVC else MediaFormat.MIMETYPE_VIDEO_AVC
        synchronized(lock) {
            if (m == mime) return
            Log.i(TAG, "codec $mime -> $m")
            mime = m
            releaseLocked() // the next keyframe with parameter sets starts the new decoder
        }
    }

    fun submit(frameId: Long, data: ByteArray, completeNs: Long) {
        synchronized(lock) {
            val hevc = mime == MediaFormat.MIMETYPE_VIDEO_HEVC
            if (codec == null) {
                if (!starting) {
                    val csd = Nal.parameterSets(data, hevc)
                    if (csd == null) {
                        stats.droppedFrames.incrementAndGet() // waiting for a keyframe with parameter sets
                        onNeedKeyframe("decoder_start")
                        return
                    }
                    // Creating a codec takes 100+ ms: do it on the decoder thread so the network
                    // thread keeps draining the socket (otherwise the receive buffer overflows).
                    starting = true
                    val gen = generation
                    val m = mime
                    handler.post { startOnDecoderThread(gen, m, csd) }
                }
                // Queued during startup: kept for decoding but excluded from latency stats.
                if (pending.size < MAX_PENDING) pending.addLast(frameId to data) else stats.droppedFrames.incrementAndGet()
                return
            }
            if (needKeyframe) {
                if (!Nal.isKeyframe(data, hevc)) {
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
                needKeyframe = !Nal.isKeyframe(data, hevc)
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
    private fun startOnDecoderThread(gen: Int, mime: String, csd: List<ByteArray>) {
        val c = try {
            createCodec(gen, mime, csd)
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
        describe(c ?: return, mime)
        Log.i(TAG, "decoder started: ${stats.codecName} ${stats.codecInfo}")
    }

    private fun createCodec(gen: Int, mime: String, csd: List<ByteArray>): MediaCodec {
        val format = MediaFormat.createVideoFormat(mime, Protocol.WIDTH, Protocol.HEIGHT).apply {
            csd.forEachIndexed { i, b -> setByteBuffer("csd-$i", ByteBuffer.wrap(b)) }
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 2 * 1024 * 1024)
            setInteger(MediaFormat.KEY_PRIORITY, 0) // realtime
            setInteger(MediaFormat.KEY_OPERATING_RATE, Protocol.FPS)
            if (Build.VERSION.SDK_INT >= 30) setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        }

        val c = MediaCodec.createDecoderByType(mime)
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

    private fun describe(c: MediaCodec, mime: String) {
        stats.codecName = c.name
        val info = c.codecInfo
        val parts = mutableListOf<String>()
        if (Build.VERSION.SDK_INT >= 29) {
            parts += if (info.isHardwareAccelerated) "HW" else "SW"
        }
        if (Build.VERSION.SDK_INT >= 30) {
            val caps = info.getCapabilitiesForType(mime)
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
                codec.releaseOutputBuffer(index, info.size > 0 && !hold)
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

    companion object {
        private const val TAG = "NANOCast.Decoder"
        private const val FRAME_US = 1_000_000L / 60 // synthetic pts: frame_id * 16666 µs
        private const val MAX_PENDING = 60 // ~1 s at 60 fps

        /** Whether this device can decode [mime] at 1080p (for `caps.codecs` / TXT `codec`). */
        fun hasDecoder(mime: String): Boolean = try {
            val format = MediaFormat.createVideoFormat(mime, Protocol.WIDTH, Protocol.HEIGHT)
            MediaCodecList(MediaCodecList.REGULAR_CODECS).findDecoderForFormat(format) != null
        } catch (e: Exception) {
            false
        }

        /** Codecs to advertise: H.264 always, HEVC when there is a decoder. */
        val supportedCodecs: List<String> by lazy {
            listOf(Protocol.CODEC_H264) +
                if (hasDecoder(MediaFormat.MIMETYPE_VIDEO_HEVC)) listOf(Protocol.CODEC_HEVC) else emptyList()
        }
    }
}

/** NAL helpers for H.264 and HEVC Annex B access units. */
object Nal {
    /** NAL unit type of the header byte at the start of a NAL. */
    private fun type(b: Byte, hevc: Boolean): Int = if (hevc) (b.toInt() shr 1) and 0x3F else b.toInt() and 0x1F

    /**
     * MediaFormat codec-specific data from a keyframe: H.264 [SPS, PPS] (csd-0, csd-1),
     * HEVC [VPS + SPS + PPS] (one csd-0). Null when they are not all there.
     */
    fun parameterSets(au: ByteArray, hevc: Boolean): List<ByteArray>? {
        if (!hevc) {
            val sps = H264.findNal(au, 7) ?: return null
            val pps = H264.findNal(au, 8) ?: return null
            return listOf(sps, pps)
        }
        val vps = H264.findNal(au) { type(it, true) == 32 } ?: return null
        val sps = H264.findNal(au) { type(it, true) == 33 } ?: return null
        val pps = H264.findNal(au) { type(it, true) == 34 } ?: return null
        return listOf(vps + sps + pps)
    }

    /** IDR (H.264 type 5) / IRAP (HEVC types 16..21: BLA, IDR, CRA). */
    fun isKeyframe(au: ByteArray, hevc: Boolean): Boolean =
        if (hevc) H264.findNal(au) { type(it, true) in 16..21 } != null else H264.findNal(au, 5) != null
}

/** Minimal Annex B helpers. */
object H264 {
    /** Returns the first NAL of [type] including its 4-byte start code, or null. */
    fun findNal(au: ByteArray, type: Int): ByteArray? = findNal(au) { (it.toInt() and 0x1F) == type }

    /** Returns the first NAL whose header byte matches, including a 4-byte start code, or null. */
    fun findNal(au: ByteArray, header: (Byte) -> Boolean): ByteArray? {
        var i = 0
        while (true) {
            val start = nextStartCode(au, i) ?: return null
            val hdr = start.second
            if (hdr >= au.size) return null
            val end = nextStartCode(au, hdr)?.first ?: au.size
            if (header(au[hdr])) {
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
