package com.redharegames.nanocast.receiver

/**
 * NANOCast wire constants (PLAN.md §4). All multi-byte header fields are big-endian.
 *
 * Video packet = 14-byte header + H.264 Annex B payload (≤ 1200 bytes):
 *   magic u16 | seq u32 | frame_id u32 | idx u16 | count u16
 * `seq` counts packets across the whole stream, `frame_id` counts access units,
 * `idx` is this packet's position inside the frame and `count` the frame's packet total.
 */
object Protocol {
    const val VIDEO_PORT = 48322
    /** TCP control channel (JSON lines, see [ControlServer]); advertised in TXT `ctrl`. */
    const val CONTROL_PORT = 48323
    const val MAGIC = 0x4E43 // "NC"
    const val HEADER_LEN = 14
    /** Packets per frame we preallocate for (1024 x 1200 B = 1.2 MB); larger frames are dropped. */
    const val MAX_PACKETS = 1024
    const val MAX_PAYLOAD = 1200

    const val SERVICE_TYPE = "_nanocast._udp"
    const val VERSION = "0.1"
    const val WIDTH = 1920
    const val HEIGHT = 1080
    const val FPS = 60
}
