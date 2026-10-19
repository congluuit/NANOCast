package com.redharegames.nanocast.receiver

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Build
import android.provider.Settings
import android.util.Log

/**
 * Advertises this receiver as `_nanocast._udp.local` so the PC app finds it without typing an IP.
 * TXT `pin=1` when the 4-digit code is on (the PC then asks for it), `codec` lists the
 * decoders (`h264` or `h264,hevc`).
 */
class Discovery(private val context: Context) {
    private val nsd = context.getSystemService(NsdManager::class.java)
    private var listener: NsdManager.RegistrationListener? = null
    @Volatile private var pin = false
    /** Set while waiting for the old registration to go away before registering again. */
    @Volatile private var reRegister = false

    /** Name actually registered (NSD may append " (2)" on conflicts), for the overlay. */
    @Volatile var registeredName: String? = null
        private set
    @Volatile var status = "mDNS: starting"
        private set

    fun deviceName(): String =
        Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME)
            ?.takeIf { it.isNotBlank() }
            ?: Build.MODEL

    /** Re-advertises with the new TXT `pin` (unregister, then register once NSD confirms). */
    fun setPin(on: Boolean) {
        if (on == pin) return
        pin = on
        if (listener == null) return
        reRegister = true
        drop()
    }

    fun register() {
        if (listener != null) return
        val info = NsdServiceInfo().apply {
            serviceName = deviceName()
            serviceType = Protocol.SERVICE_TYPE
            port = Protocol.VIDEO_PORT
            setAttribute("w", Protocol.WIDTH.toString())
            setAttribute("h", Protocol.HEIGHT.toString())
            setAttribute("codec", VideoDecoder.supportedCodecs.joinToString(","))
            setAttribute("pin", if (pin) "1" else "0")
            setAttribute("ver", Protocol.VERSION)
            setAttribute("ctrl", Protocol.CONTROL_PORT.toString())
        }
        val l = object : NsdManager.RegistrationListener {
            override fun onServiceRegistered(info: NsdServiceInfo) {
                registeredName = info.serviceName
                status = "mDNS: \"${info.serviceName}\" ${Protocol.SERVICE_TYPE}.local"
                Log.i(TAG, "registered ${info.serviceName} ${Protocol.SERVICE_TYPE} port ${Protocol.VIDEO_PORT}")
            }

            override fun onRegistrationFailed(info: NsdServiceInfo, errorCode: Int) {
                status = "mDNS: registration failed ($errorCode)"
                Log.e(TAG, "registration failed: $errorCode")
                listener = null
            }

            override fun onServiceUnregistered(info: NsdServiceInfo) {
                Log.i(TAG, "unregistered ${info.serviceName}")
                if (reRegister) {
                    reRegister = false
                    register()
                }
            }

            override fun onUnregistrationFailed(info: NsdServiceInfo, errorCode: Int) {
                Log.w(TAG, "unregistration failed: $errorCode")
            }
        }
        listener = l
        nsd.registerService(info, NsdManager.PROTOCOL_DNS_SD, l)
    }

    fun unregister() {
        reRegister = false
        drop()
    }

    private fun drop() {
        val l = listener ?: return
        listener = null
        registeredName = null
        try {
            nsd.unregisterService(l)
        } catch (e: IllegalArgumentException) {
            Log.w(TAG, "unregister: $e")
        }
    }

    private companion object {
        const val TAG = "NANOCast.Nsd"
    }
}
