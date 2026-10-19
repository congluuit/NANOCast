package com.redharegames.nanocast.receiver

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTimestamp
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.util.Log
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetSocketAddress
import java.net.SocketException
import java.net.SocketTimeoutException
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.Locale
import java.util.concurrent.atomic.AtomicLong

/**
 * System sound from the PC (PLAN.md §4): UDP [Protocol.AUDIO_PORT], one 20 ms Opus frame per
 * packet (48 kHz stereo) → MediaCodec "audio/opus" → AudioTrack (low-latency, streaming).
 *
 * Everything (socket, decoder, track) lives on one thread, so control messages only flip
 * volatile flags. Latency is kept small and bounded:
 *  - the track's usable buffer is [TRACK_BUFFER_MS]; playback starts once [START_BUFFER_MS]
 *    are queued (API 31+ start threshold; older AudioFlinger waits for a full buffer);
 *  - sound that does not fit (track full, or more than [MAX_BUFFER_MS] queued) is dropped,
 *    so a sender clock running faster than the TV's, or a burst after a stall, never
 *    builds up delay;
 *  - after a gap of [GAP_MS] without packets the buffer is refilled from scratch.
 * An empty payload means 20 ms of silence: zeros are written without decoding.
 */
class AudioPlayer {
    @Volatile private var enabled = true
    /** Source filter (4-digit code on: only the accepted sender); set before [start]. */
    @Volatile var allow: (java.net.InetAddress) -> Boolean = { true }
    @Volatile private var flushRequested = false
    @Volatile private var socket: DatagramSocket? = null
    private var thread: Thread? = null

    // Stats (written on the audio thread, read by the UI).
    val packets = AtomicLong()
    val bytes = AtomicLong()
    val lost = AtomicLong()
    val silentPackets = AtomicLong()
    val lateDrops = AtomicLong()
    val decodeErrors = AtomicLong()
    @Volatile var bufferedMs = 0
        private set
    @Volatile var underruns = 0
        private set
    /** Write → speaker (track queue + mixer + HAL), from AudioTimestamp; -1 = unknown. */
    @Volatile var outputLatencyMs = -1
        private set
    @Volatile var codecName = "-"
        private set
    /** Usable AudioTrack buffer actually granted (TRACK_BUFFER_MS requested). */
    @Volatile var trackBufferMs = 0
        private set
    @Volatile var state = "audio: idle"
        private set

    private var lastSnapNs = System.nanoTime()
    private var lastBytes = 0L

    fun start() {
        if (thread != null) return
        val s = DatagramSocket(null)
        s.reuseAddress = true
        s.receiveBufferSize = 256 * 1024
        s.soTimeout = 20
        s.bind(InetSocketAddress(Protocol.AUDIO_PORT))
        socket = s
        thread = Thread({ loop(s) }, "nanocast-audio").apply {
            priority = Thread.MAX_PRIORITY
            start()
        }
        Log.i(TAG, "listening on UDP ${Protocol.AUDIO_PORT}")
    }

    fun stop() {
        socket?.close()
        socket = null
        thread?.join(500)
        thread = null
        state = "audio: idle"
    }

    /** From the control channel (`audio {on}`, `start {audio}`, `stop`). Off = flush and stay silent. */
    fun setEnabled(on: Boolean) {
        if (enabled != on) Log.i(TAG, "audio ${if (on) "on" else "off"}")
        enabled = on
        if (!on) flushRequested = true
    }

    /** Audio kbps since the previous call (UI thread only). */
    fun kbps(): Double {
        val now = System.nanoTime()
        val b = bytes.get()
        val k = (b - lastBytes) * 8 / ((now - lastSnapNs).coerceAtLeast(1) / 1e9) / 1e3
        lastSnapNs = now
        lastBytes = b
        return k
    }

    fun overlayLine(): String = String.format(
        Locale.US, "%s  buf %d/%d ms  out %d ms  lost %d  late-drop %d  underrun %d  silent %d  [%s]",
        state, bufferedMs, trackBufferMs, outputLatencyMs, lost.get(), lateDrops.get(), underruns, silentPackets.get(), codecName,
    )

    // ---------------- audio thread ----------------

    private var codec: MediaCodec? = null
    private var track: AudioTrack? = null
    private var framesWritten = 0L
    private var playing = false
    private var expectSeq = -1L
    private val bufferInfo = MediaCodec.BufferInfo()
    private val timestamp = AudioTimestamp()
    private val zeros = ByteArray(Protocol.AUDIO_FRAME_SAMPLES * BYTES_PER_FRAME)

    private fun loop(s: DatagramSocket) {
        android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_URGENT_AUDIO)
        val buf = ByteArray(2048)
        val packet = DatagramPacket(buf, buf.size)
        var lastPacketNs = 0L
        try {
            track = createTrack()
            codec = createCodec()
            state = if (codec == null) "audio: no Opus decoder" else "audio: waiting"
            while (true) {
                if (flushRequested) {
                    flushRequested = false
                    reset()
                    state = "audio: off"
                }
                try {
                    packet.setLength(buf.size)
                    s.receive(packet)
                } catch (e: SocketTimeoutException) {
                    drainDecoder()
                    if (playing && System.nanoTime() - lastPacketNs > GAP_MS * 1_000_000L) {
                        // Sender paused or gone: drop what is queued and rebuffer when it resumes.
                        reset()
                        if (enabled) state = "audio: waiting"
                    }
                    updateBuffered()
                    continue
                }
                val len = packet.length
                if (len < Protocol.AUDIO_HEADER_LEN || u16(buf, 0) != Protocol.AUDIO_MAGIC) continue
                if (!enabled || !allow(packet.address)) continue
                lastPacketNs = System.nanoTime()
                val seq = u32(buf, 2)
                if (expectSeq >= 0 && seq != expectSeq) {
                    val gap = (seq - expectSeq) and 0xFFFFFFFFL
                    if (gap < 1000) lost.addAndGet(gap)
                }
                expectSeq = (seq + 1) and 0xFFFFFFFFL
                packets.incrementAndGet()
                bytes.addAndGet(len.toLong())
                onPacket(buf, len, u32(buf, 6))
            }
        } catch (e: SocketException) {
            // closed by stop()
        } catch (e: Exception) {
            Log.e(TAG, "audio loop error", e)
            state = "audio: error ${e.javaClass.simpleName}"
        } finally {
            try { codec?.stop() } catch (_: Exception) {}
            codec?.release()
            codec = null
            track?.release()
            track = null
        }
        Log.i(TAG, "audio loop ended")
    }

    private fun onPacket(buf: ByteArray, len: Int, ptsMs: Long) {
        val t = track ?: return
        val queued = updateBuffered()
        // Only once the track really plays: while filling, AudioFlinger may need a full buffer.
        if (queued > MAX_BUFFER_MS && t.playbackHeadPosition != 0) {
            lateDrops.incrementAndGet()
            return
        }
        if (playing && queued == 0) {
            // Underrun (late packets): pause and refill instead of playing packet by packet.
            t.pause()
            playing = false
            state = "audio: rebuffering"
        }
        val payload = len - Protocol.AUDIO_HEADER_LEN
        if (payload == 0) {
            silentPackets.incrementAndGet()
            write(t, ByteBuffer.wrap(zeros), zeros.size)
        } else {
            val c = codec ?: return
            try {
                val idx = c.dequeueInputBuffer(5_000)
                if (idx >= 0) {
                    val inBuf = c.getInputBuffer(idx)!!
                    inBuf.clear()
                    inBuf.put(buf, Protocol.AUDIO_HEADER_LEN, payload)
                    c.queueInputBuffer(idx, 0, payload, ptsMs * 1000, 0)
                }
            } catch (e: Exception) {
                decodeErrors.incrementAndGet()
                Log.w(TAG, "opus input error", e)
                recreateCodec()
            }
            drainDecoder()
        }
        if (!playing && updateBuffered() >= START_BUFFER_MS) {
            t.play()
            playing = true
            state = "audio: on"
        }
    }

    private fun drainDecoder() {
        val c = codec ?: return
        val t = track ?: return
        try {
            while (true) {
                val idx = c.dequeueOutputBuffer(bufferInfo, 0)
                if (idx < 0) break // TRY_AGAIN_LATER or format/buffers changed
                val out = c.getOutputBuffer(idx)
                if (out != null && bufferInfo.size > 0) {
                    out.position(bufferInfo.offset)
                    out.limit(bufferInfo.offset + bufferInfo.size)
                    write(t, out, bufferInfo.size)
                }
                c.releaseOutputBuffer(idx, false)
            }
        } catch (e: Exception) {
            decodeErrors.incrementAndGet()
            Log.w(TAG, "opus output error", e)
            recreateCodec()
        }
    }

    /** Non-blocking: whatever does not fit in the track's buffer is dropped (counted once). */
    private fun write(t: AudioTrack, data: ByteBuffer, size: Int) {
        val n = t.write(data, size, AudioTrack.WRITE_NON_BLOCKING)
        if (n > 0) framesWritten += n / BYTES_PER_FRAME
        if (n < size) lateDrops.incrementAndGet()
    }

    /** Milliseconds written to the track but not yet played. */
    private fun updateBuffered(): Int {
        val t = track ?: return 0
        val played = t.playbackHeadPosition.toLong() and 0xFFFFFFFFL
        val ms = ((framesWritten - played).coerceAtLeast(0) * 1000 / Protocol.AUDIO_SAMPLE_RATE).toInt()
        bufferedMs = ms
        if (Build.VERSION.SDK_INT >= 24) underruns = t.underrunCount
        if (playing && t.getTimestamp(timestamp)) {
            // Frame being heard now = timestamp frame + time elapsed since it.
            val nowFrame = timestamp.framePosition +
                (System.nanoTime() - timestamp.nanoTime) * Protocol.AUDIO_SAMPLE_RATE / 1_000_000_000L
            outputLatencyMs = ((framesWritten - nowFrame) * 1000 / Protocol.AUDIO_SAMPLE_RATE).toInt()
        }
        return ms
    }

    /** Silence now: drop queued sound and decoder state; playback restarts after rebuffering. */
    private fun reset() {
        track?.let {
            it.pause()
            it.flush()
        }
        playing = false
        framesWritten = 0
        expectSeq = -1
        try { codec?.flush() } catch (_: Exception) { recreateCodec() }
        bufferedMs = 0
        outputLatencyMs = -1
    }

    private fun recreateCodec() {
        try { codec?.stop() } catch (_: Exception) {}
        codec?.release()
        codec = createCodec()
    }

    private fun createTrack(): AudioTrack {
        val format = AudioFormat.Builder()
            .setSampleRate(Protocol.AUDIO_SAMPLE_RATE)
            .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
            .build()
        val min = AudioTrack.getMinBufferSize(Protocol.AUDIO_SAMPLE_RATE, AudioFormat.CHANNEL_OUT_STEREO, AudioFormat.ENCODING_PCM_16BIT)
        // Room for MAX_BUFFER_MS plus one packet; the track only ever holds what we wrote.
        val wanted = (MAX_BUFFER_MS + 2 * Protocol.AUDIO_FRAME_MS) * Protocol.AUDIO_SAMPLE_RATE / 1000 * BYTES_PER_FRAME
        val t = AudioTrack.Builder()
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA)
                    .setContentType(AudioAttributes.CONTENT_TYPE_MOVIE)
                    .build(),
            )
            .setAudioFormat(format)
            .setBufferSizeInBytes(maxOf(min, wanted))
            .setTransferMode(AudioTrack.MODE_STREAM)
            .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
            .setSessionId(AudioManager.AUDIO_SESSION_ID_GENERATE)
            .build()
        // Usable part of the buffer = the most sound that can ever be queued (latency cap).
        t.setBufferSizeInFrames(TRACK_BUFFER_MS * Protocol.AUDIO_SAMPLE_RATE / 1000)
        if (Build.VERSION.SDK_INT >= 31) {
            t.setStartThresholdInFrames(START_BUFFER_MS * Protocol.AUDIO_SAMPLE_RATE / 1000)
        }
        trackBufferMs = t.bufferSizeInFrames * 1000 / Protocol.AUDIO_SAMPLE_RATE
        Log.i(TAG, "AudioTrack: buffer ${t.bufferSizeInFrames}/${t.bufferCapacityInFrames} frames " +
            "($trackBufferMs ms, min $min B), perf mode ${t.performanceMode}" +
            (if (Build.VERSION.SDK_INT >= 31) ", start threshold ${t.startThresholdInFrames}" else ""))
        return t
    }

    private fun createCodec(): MediaCodec? = try {
        val format = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, Protocol.AUDIO_SAMPLE_RATE, 2)
        format.setByteBuffer("csd-0", ByteBuffer.wrap(opusHead()))
        format.setByteBuffer("csd-1", longLe(Protocol.OPUS_PRE_SKIP * 1_000_000_000L / Protocol.AUDIO_SAMPLE_RATE))
        format.setByteBuffer("csd-2", longLe(SEEK_PREROLL_NS))
        val c = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS)
        c.configure(format, null, null, 0)
        c.start()
        codecName = c.name
        Log.i(TAG, "Opus decoder ${c.name}")
        c
    } catch (e: Exception) {
        Log.e(TAG, "cannot create Opus decoder", e)
        codecName = "none"
        null
    }

    /** RFC 7845 identification header: 48 kHz stereo, mapping family 0. */
    private fun opusHead(): ByteArray {
        val b = ByteBuffer.allocate(19).order(ByteOrder.LITTLE_ENDIAN)
        b.put("OpusHead".toByteArray(Charsets.US_ASCII))
        b.put(1) // version
        b.put(2) // channels
        b.putShort(Protocol.OPUS_PRE_SKIP.toShort())
        b.putInt(Protocol.AUDIO_SAMPLE_RATE)
        b.putShort(0) // output gain
        b.put(0) // mapping family
        return b.array()
    }

    private fun longLe(v: Long): ByteBuffer =
        ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(v).also { it.flip() }

    private fun u16(b: ByteArray, o: Int) = ((b[o].toInt() and 0xFF) shl 8) or (b[o + 1].toInt() and 0xFF)

    private fun u32(b: ByteArray, o: Int): Long =
        ((b[o].toLong() and 0xFF) shl 24) or ((b[o + 1].toLong() and 0xFF) shl 16) or
            ((b[o + 2].toLong() and 0xFF) shl 8) or (b[o + 3].toLong() and 0xFF)

    private companion object {
        const val TAG = "NANOCast.Audio"
        const val BYTES_PER_FRAME = 4 // 16-bit stereo
        /** Queued before playback starts: two packets. */
        const val START_BUFFER_MS = 40
        /** Usable AudioTrack buffer: three packets. */
        const val TRACK_BUFFER_MS = 80
        /** Above this, new packets are dropped so delay cannot grow. */
        const val MAX_BUFFER_MS = 100
        /** No packet for this long = stream paused; rebuffer on the next one. */
        const val GAP_MS = 200
        const val SEEK_PREROLL_NS = 80_000_000L
    }
}
