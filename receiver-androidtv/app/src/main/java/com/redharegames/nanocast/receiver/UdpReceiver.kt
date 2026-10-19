package com.redharegames.nanocast.receiver

import android.util.Log
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetSocketAddress
import java.net.SocketException

/** Blocking UDP receive loop on its own high-priority thread. */
class UdpReceiver(private val port: Int, private val assembler: FrameAssembler) {
    @Volatile private var socket: DatagramSocket? = null
    private var thread: Thread? = null

    fun start() {
        val s = DatagramSocket(null)
        s.reuseAddress = true
        s.receiveBufferSize = 4 * 1024 * 1024 // absorbs the burst of a large keyframe
        s.bind(InetSocketAddress(port))
        socket = s
        Log.i(TAG, "listening on UDP $port (rcvbuf ${s.receiveBufferSize} B)")
        thread = Thread({ loop(s) }, "nanocast-udp").apply {
            priority = Thread.MAX_PRIORITY
            start()
        }
    }

    private fun loop(s: DatagramSocket) {
        android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_URGENT_DISPLAY)
        val buf = ByteArray(2048)
        val packet = DatagramPacket(buf, buf.size)
        while (true) {
            try {
                packet.setLength(buf.size)
                s.receive(packet)
                assembler.onPacket(buf, packet.length, System.nanoTime())
            } catch (e: SocketException) {
                break // closed by stop()
            } catch (e: Exception) {
                Log.e(TAG, "receive loop error", e)
            }
        }
        Log.i(TAG, "receive loop ended")
    }

    fun stop() {
        socket?.close()
        socket = null
        thread?.join(500)
        thread = null
    }

    private companion object {
        const val TAG = "NANOCast.Udp"
    }
}
