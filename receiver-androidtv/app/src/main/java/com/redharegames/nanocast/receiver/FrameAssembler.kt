package com.redharegames.nanocast.receiver

/**
 * Reassembles UDP packets into H.264 access units.
 *
 * Frames are delivered strictly in increasing frame_id order as soon as all of their packets
 * have arrived. When a newer frame completes while an older one is still missing packets, the
 * older one is given up (counted in [Stats]) — waiting for it would only add latency.
 * Not thread-safe: call from the single network thread.
 *
 * [onLoss] fires on every gap in the packet sequence, so the sender can be asked for a
 * keyframe right away (the frame with the hole will be dropped and later P-frames would
 * reference it).
 */
class FrameAssembler(
    private val stats: Stats,
    private val onLoss: () -> Unit = {},
    private val onFrame: (frameId: Long, data: ByteArray, completeNs: Long) -> Unit,
) {
    private class Slot {
        var frameId = -1L
        var count = 0
        var received = 0
        var firstNs = 0L
        val have = BooleanArray(Protocol.MAX_PACKETS)
        val lens = IntArray(Protocol.MAX_PACKETS)
        val parts = Array(Protocol.MAX_PACKETS) { ByteArray(Protocol.MAX_PAYLOAD + 64) }

        fun reset(id: Long, count: Int, now: Long) {
            frameId = id
            this.count = count
            received = 0
            firstNs = now
            java.util.Arrays.fill(have, 0, count, false)
        }
    }

    private val slots = Array(SLOTS) { Slot() }
    private var lastDelivered = -1L
    private var lastSeq = -1L
    private var lastPacketNs = 0L

    fun onPacket(buf: ByteArray, len: Int, nowNs: Long) {
        if (len < Protocol.HEADER_LEN) return
        val magic = u16(buf, 0)
        if (magic != Protocol.MAGIC) {
            stats.badPackets.incrementAndGet()
            return
        }
        val seq = u32(buf, 2)
        val frameId = u32(buf, 6)
        val idx = u16(buf, 10)
        val count = u16(buf, 12)
        val payloadLen = len - Protocol.HEADER_LEN
        if (count == 0 || count > Protocol.MAX_PACKETS || idx >= count || payloadLen > Protocol.MAX_PAYLOAD + 64) {
            stats.badPackets.incrementAndGet()
            return
        }

        // Sender restarted (ids went far backwards, or the stream paused): start over.
        val restarted = lastDelivered >= 0 && frameId + RESTART_GAP < lastDelivered
        if (restarted || (lastPacketNs != 0L && nowNs - lastPacketNs > IDLE_RESET_NS)) {
            reset()
            stats.streamRestarts.incrementAndGet()
        }
        lastPacketNs = nowNs
        trackSeq(seq)
        stats.onPacket(len)

        if (frameId <= lastDelivered) return // late packet of a frame we already passed

        val slot = slots[(frameId % SLOTS).toInt()]
        // An older incomplete frame in this slot is lost; deliver() counts it as skipped.
        if (slot.frameId != frameId) slot.reset(frameId, count, nowNs)
        if (slot.have[idx]) return // duplicate
        slot.have[idx] = true
        slot.lens[idx] = payloadLen
        System.arraycopy(buf, Protocol.HEADER_LEN, slot.parts[idx], 0, payloadLen)
        slot.received++

        if (slot.received == slot.count) deliver(slot, nowNs)
    }

    private fun deliver(slot: Slot, nowNs: Long) {
        // Give up on every older frame that is still incomplete.
        if (lastDelivered >= 0) {
            val skipped = slot.frameId - lastDelivered - 1
            if (skipped > 0) stats.droppedFrames.addAndGet(skipped)
        }
        var total = 0
        for (i in 0 until slot.count) total += slot.lens[i]
        val data = ByteArray(total)
        var off = 0
        for (i in 0 until slot.count) {
            System.arraycopy(slot.parts[i], 0, data, off, slot.lens[i])
            off += slot.lens[i]
        }
        lastDelivered = slot.frameId
        slot.frameId = -1
        stats.onAssembled(nowNs - slot.firstNs)
        onFrame(lastDelivered, data, nowNs)
    }

    private fun trackSeq(seq: Long) {
        if (lastSeq >= 0) {
            val gap = seq - lastSeq - 1
            when {
                gap > 0 -> {
                    stats.lostPackets.addAndGet(gap)
                    onLoss()
                }
                gap < 0 && seq < lastSeq -> {
                    // Reordered packet filling an earlier gap.
                    if (stats.lostPackets.get() > 0) stats.lostPackets.decrementAndGet()
                    return
                }
            }
        }
        lastSeq = seq
    }

    private fun reset() {
        for (s in slots) s.frameId = -1
        lastDelivered = -1
        lastSeq = -1
    }

    private companion object {
        const val SLOTS = 8
        const val RESTART_GAP = 60L
        const val IDLE_RESET_NS = 1_000_000_000L

        fun u16(b: ByteArray, o: Int) = ((b[o].toInt() and 0xFF) shl 8) or (b[o + 1].toInt() and 0xFF)

        fun u32(b: ByteArray, o: Int): Long =
            ((b[o].toLong() and 0xFF) shl 24) or ((b[o + 1].toLong() and 0xFF) shl 16) or
                ((b[o + 2].toLong() and 0xFF) shl 8) or (b[o + 3].toLong() and 0xFF)
    }
}
