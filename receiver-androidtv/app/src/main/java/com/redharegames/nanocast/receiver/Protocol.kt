package com.redharegames.nanocast.receiver

/**
 * NANOCast wire constants (PLAN.md §4). All multi-byte header fields are big-endian.
 *
 * Video packet = 14-byte header + H.264 Annex B payload (≤ 1200 bytes):
 *   magic u16 | seq u32 | frame_id u32 | idx u16 | count u16
 * `seq` counts packets across the whole stream, `frame_id` counts access units,
 * `idx` is this packet's position inside the frame and `count` the frame's packet total.
 *
 * Audio packet (UDP [AUDIO_PORT]) = 10-byte header + one 20 ms Opus frame, 48 kHz stereo:
 *   magic u16 = [AUDIO_MAGIC] | seq u32 | pts_ms u32
 * An empty payload means 20 ms of silence (see [AudioPlayer]).
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

    const val AUDIO_PORT = 48324
    const val AUDIO_MAGIC = 0x4E41 // "NA"
    const val AUDIO_HEADER_LEN = 10
    const val AUDIO_SAMPLE_RATE = 48000
    const val AUDIO_FRAME_MS = 20
    const val AUDIO_FRAME_SAMPLES = 960
    /** Sender's Opus encoder delay in samples (libopus application Audio at 48 kHz). */
    const val OPUS_PRE_SKIP = 312

    const val SERVICE_TYPE = "_nanocast._udp"
    const val VERSION = "0.1"
    const val WIDTH = 1920
    const val HEIGHT = 1080
    const val FPS = 60
}
