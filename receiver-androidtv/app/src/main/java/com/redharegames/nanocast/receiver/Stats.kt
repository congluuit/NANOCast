package com.redharegames.nanocast.receiver

import java.util.concurrent.atomic.AtomicLong

/**
 * Counters shared by the network thread, the decoder callback thread and the UI.
 *
 * "Decode latency" = time from the moment the last packet of a frame arrived (frame complete)
 * to the moment MediaCodec hands the decoded output buffer back, i.e. queue + decode.
 */
class Stats {
    val lostPackets = AtomicLong()
    val badPackets = AtomicLong()
    val droppedFrames = AtomicLong()
    val streamRestarts = AtomicLong()
    val decoderErrors = AtomicLong()

    @Volatile var codecName = "-"
    @Volatile var codecInfo = ""
    @Volatile var videoSize = "-"

    private val lock = Any()
    private var packets = 0L
    private var bytes = 0L
    private var framesIn = 0L
    private var framesOut = 0L
    private var assemblySumNs = 0L

    /** Recent decode latencies (µs) for the overlay window. */
    private val window = LongArray(WINDOW)
    private var windowLen = 0
    private var windowPos = 0

    /** Whole-session histogram, 0.1 ms buckets up to 200 ms (last bucket = overflow). */
    private val hist = LongArray(2001)
    private var histCount = 0L
    private var histSumUs = 0L

    // Rate snapshot.
    private var lastSnapNs = System.nanoTime()
    private var lastFramesOut = 0L
    private var lastFramesIn = 0L
    private var lastBytes = 0L

    fun onPacket(len: Int) = synchronized(lock) {
        packets++
        bytes += len
    }

    fun onAssembled(assemblyNs: Long) = synchronized(lock) {
        framesIn++
        assemblySumNs += assemblyNs
    }

    /** [latencyNs] < 0 means the frame's arrival time is unknown: counted for fps only. */
    fun onDecoded(latencyNs: Long) = synchronized(lock) {
        framesOut++
        if (latencyNs < 0) return@synchronized
        val us = latencyNs / 1000
        window[windowPos] = us
        windowPos = (windowPos + 1) % WINDOW
        if (windowLen < WINDOW) windowLen++
        hist[(us / 100).coerceAtMost(2000L).toInt()]++
        histCount++
        histSumUs += us
    }

    /** Side-effect-free reads for the control channel ([snapshot] resets the overlay's rate window). */
    fun framesOutTotal(): Long = synchronized(lock) { framesOut }

    fun windowAvgMs(): Double = synchronized(lock) {
        if (windowLen == 0) 0.0 else (0 until windowLen).sumOf { window[it] } / windowLen.toDouble() / 1000
    }

    class Snapshot(
        val fpsOut: Double,
        val fpsIn: Double,
        val mbps: Double,
        val windowAvgMs: Double,
        val windowP95Ms: Double,
        val sessionAvgMs: Double,
        val sessionP50Ms: Double,
        val sessionP95Ms: Double,
        val sessionP99Ms: Double,
        val sessionN: Long,
        val packets: Long,
        val framesIn: Long,
        val framesOut: Long,
        val avgAssemblyMs: Double,
    )

    fun snapshot(): Snapshot = synchronized(lock) {
        val now = System.nanoTime()
        val dt = (now - lastSnapNs).coerceAtLeast(1) / 1e9
        val fpsOut = (framesOut - lastFramesOut) / dt
        val fpsIn = (framesIn - lastFramesIn) / dt
        val mbps = (bytes - lastBytes) * 8 / dt / 1e6
        lastSnapNs = now
        lastFramesOut = framesOut
        lastFramesIn = framesIn
        lastBytes = bytes

        val w = window.copyOf(windowLen).also { it.sort() }
        val wAvg = if (w.isEmpty()) 0.0 else w.average() / 1000
        val wP95 = if (w.isEmpty()) 0.0 else w[((w.size - 1) * 0.95).toInt()] / 1000.0

        Snapshot(
            fpsOut, fpsIn, mbps, wAvg, wP95,
            if (histCount == 0L) 0.0 else histSumUs / histCount.toDouble() / 1000,
            histPercentile(0.50), histPercentile(0.95), histPercentile(0.99), histCount,
            packets, framesIn, framesOut,
            if (framesIn == 0L) 0.0 else assemblySumNs / framesIn.toDouble() / 1e6,
        )
    }

    private fun histPercentile(p: Double): Double {
        if (histCount == 0L) return 0.0
        val target = (histCount * p).toLong().coerceAtLeast(1)
        var acc = 0L
        for (i in hist.indices) {
            acc += hist[i]
            if (acc >= target) return (i + 1) / 10.0 // upper edge of the bucket, ms
        }
        return 200.0
    }

    private companion object {
        const val WINDOW = 120 // ~2 s at 60 fps
    }
}
