package com.redharegames.nanocast.receiver

import android.app.Activity
import android.app.PendingIntent
import android.content.Intent
import android.content.pm.PackageInfo
import android.content.pm.PackageInstaller
import android.content.pm.PackageManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.widget.Toast
import java.io.File
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.Executors

/**
 * Updates from GitHub releases (release builds only, [BuildConfig.SELF_UPDATE]; the Play build
 * is updated by the store). Every few hours: which tag does `releases/latest` redirect to? When
 * it is newer than this app, download `NANOCast-Receiver-<v>-androidtv.apk` (the name
 * `release.yml` gives it) into the cache and, while no PC is casting, hand it to
 * [PackageInstaller]. The TV asks "Install?" (the first time also to allow installing apps
 * from NANOCast); on Android 12+ later updates of an app it installed itself need no answer.
 * A dialog that was cancelled comes back with ▲ on the remote.
 *
 * An APK signed with another key cannot replace this one (an older build from another PC):
 * then the corner text says to install the app again from the PC ("Install the app on this
 * TV" there removes the old one first).
 */
class Updater(
    private val activity: Activity,
    /** A PC is casting: do not interrupt it. */
    private val busy: () -> Boolean,
    /** Corner text (main thread), null when there is nothing to say. */
    private val onMessage: (String?) -> Unit,
) {
    private enum class Phase { IDLE, CHECKING, READY, INSTALLING, OTHER_KEY }

    private class Found(val version: String, val apk: File, val otherKey: Boolean)

    private val ui = Handler(Looper.getMainLooper())
    private val io = Executors.newSingleThreadExecutor { r -> Thread(r, "nanocast-update").apply { isDaemon = true } }
    private val prefs = activity.getSharedPreferences(PREFS, Activity.MODE_PRIVATE)

    // Main thread only.
    private var phase = Phase.IDLE
    private var found: Found? = null
    /** The install dialog came up by itself once since the app opened; after that only ▲. */
    private var asked = false
    private var installingSince = 0L
    private var running = false

    private val tick = object : Runnable {
        override fun run() {
            step()
            ui.postDelayed(this, TICK_MS)
        }
    }

    fun start() {
        if (!BuildConfig.SELF_UPDATE || running) return
        running = true
        ui.post(tick)
    }

    fun stop() {
        running = false
        ui.removeCallbacks(tick)
    }

    /** ▲ on the remote. True when there was an update to install. */
    fun installNow(): Boolean {
        if (phase != Phase.READY) return false
        install()
        return true
    }

    private fun step() {
        val now = System.currentTimeMillis()
        when (phase) {
            Phase.IDLE -> if (now - prefs.getLong(PREF_CHECKED, 0) !in 0 until CHECK_EVERY_MS) check()
            Phase.READY -> if (!asked && !busy()) install()
            // No answer from the installer (some TVs drop it): offer ▲ again.
            Phase.INSTALLING -> if (now - installingSince > INSTALL_TIMEOUT_MS) phase = Phase.READY
            else -> {}
        }
        show()
    }

    private fun show() {
        val v = found?.version
        onMessage(
            when {
                busy() -> null
                phase == Phase.READY -> "Có bản mới $v: bấm ▲ để cập nhật / New version $v: press ▲ to update"
                phase == Phase.INSTALLING -> "Đang cập nhật lên $v… / Updating to $v…"
                phase == Phase.OTHER_KEY -> "Có bản mới $v: cài lại app từ máy tính / reinstall the app from the PC"
                else -> null
            },
        )
    }

    private fun check() {
        phase = Phase.CHECKING
        io.execute {
            val result = runCatching { fetch() }
            ui.post { checked(result) }
        }
    }

    private fun checked(result: Result<Found?>) {
        val now = System.currentTimeMillis()
        result.onSuccess { f ->
            prefs.edit().putLong(PREF_CHECKED, now).apply()
            found = f
            phase = when {
                f == null -> Phase.IDLE
                f.otherKey -> Phase.OTHER_KEY
                else -> Phase.READY
            }
            Log.i(TAG, if (f == null) "up to date (${BuildConfig.VERSION_NAME})" else "update ${f.version} ready, other key: ${f.otherKey}")
        }.onFailure { e ->
            Log.w(TAG, "update check: $e")
            prefs.edit().putLong(PREF_CHECKED, now - CHECK_EVERY_MS + RETRY_MS).apply()
            phase = Phase.IDLE
        }
        step()
    }

    /** Background: the newer release, downloaded; null when this app is the newest. */
    private fun fetch(): Found? {
        val dir = File(activity.cacheDir, "update").apply { mkdirs() }
        dir.listFiles()?.filter { !newer(versionIn(it.name) ?: "0", BuildConfig.VERSION_NAME) }?.forEach { it.delete() }
        val latest = latestVersion()
        if (!newer(latest, BuildConfig.VERSION_NAME)) return null
        val apk = File(dir, "NANOCast-Receiver-$latest-androidtv.apk")
        if (!isApk(apk)) download("$REPO/releases/download/v$latest/${apk.name}", apk)
        val info = archiveInfo(apk)
        if (info == null || info.packageName != activity.packageName) {
            apk.delete()
            throw IOException("${apk.name} is not a NANOCast receiver")
        }
        return Found(latest, apk, otherKey(info))
    }

    /** The version `releases/latest` redirects to (".../releases/tag/v0.2.0"). */
    private fun latestVersion(): String {
        val c = open("$REPO/releases/latest")
        c.instanceFollowRedirects = false
        try {
            val code = c.responseCode
            val location = c.getHeaderField("Location") ?: throw IOException("releases/latest: HTTP $code without a redirect")
            return TAG_RE.find(location)?.groupValues?.get(1) ?: throw IOException("releases/latest redirects to $location")
        } finally {
            c.disconnect()
        }
    }

    private fun download(url: String, apk: File) {
        Log.i(TAG, "downloading $url")
        val c = open(url)
        try {
            if (c.responseCode != HttpURLConnection.HTTP_OK) throw IOException("$url: HTTP ${c.responseCode}")
            val total = c.contentLengthLong
            if (total > MAX_APK) throw IOException("$url: $total bytes")
            val part = File(apk.path + ".part")
            c.inputStream.use { input -> part.outputStream().use { input.copyTo(it) } }
            if (total >= 0 && part.length() != total) throw IOException("$url: got ${part.length()} of $total bytes")
            if (!isApk(part) || !part.renameTo(apk)) {
                part.delete()
                throw IOException("$url: not an APK")
            }
        } finally {
            c.disconnect()
        }
    }

    private fun open(url: String) = (URL(url).openConnection() as HttpURLConnection).apply {
        connectTimeout = 15_000
        readTimeout = 30_000
        setRequestProperty("User-Agent", "NANOCast-Receiver/${BuildConfig.VERSION_NAME}")
    }

    @Suppress("DEPRECATION")
    private fun signingFlags() = if (Build.VERSION.SDK_INT >= 28) PackageManager.GET_SIGNING_CERTIFICATES else PackageManager.GET_SIGNATURES

    private fun archiveInfo(apk: File): PackageInfo? = activity.packageManager.getPackageArchiveInfo(apk.path, signingFlags())

    /** Both signatures known and different (unknown: let the installer decide). */
    @Suppress("DEPRECATION")
    private fun otherKey(theirs: PackageInfo): Boolean {
        val ours = activity.packageManager.getPackageInfo(activity.packageName, signingFlags())
        val a = signers(theirs)
        val b = signers(ours)
        return a.isNotEmpty() && b.isNotEmpty() && a != b
    }

    @Suppress("DEPRECATION")
    private fun signers(info: PackageInfo): Set<String> =
        (if (Build.VERSION.SDK_INT >= 28) info.signingInfo?.apkContentsSigners else info.signatures)
            ?.map { it.toCharsString() }?.toSet().orEmpty()

    private fun install() {
        val apk = found?.apk ?: return
        asked = true
        phase = Phase.INSTALLING
        installingSince = System.currentTimeMillis()
        show()
        io.execute {
            try {
                commit(apk)
            } catch (e: Exception) {
                Log.w(TAG, "install: $e")
                ui.post {
                    phase = Phase.READY
                    show()
                }
            }
        }
    }

    /** Background: a [PackageInstaller] session; the answer comes back through [onIntent]. */
    private fun commit(apk: File) {
        val installer = activity.packageManager.packageInstaller
        val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL)
        params.setAppPackageName(activity.packageName)
        if (Build.VERSION.SDK_INT >= 31) params.setRequireUserAction(PackageInstaller.SessionParams.USER_ACTION_NOT_REQUIRED)
        val id = installer.createSession(params)
        try {
            installer.openSession(id).use { session ->
                session.openWrite("base.apk", 0, apk.length()).use { out ->
                    apk.inputStream().use { it.copyTo(out) }
                    session.fsync(out)
                }
                // singleTask: back to this activity (onNewIntent), or a fresh one after the update.
                val intent = Intent(activity, activity.javaClass).setAction(ACTION_INSTALL_STATUS)
                val mutable = if (Build.VERSION.SDK_INT >= 31) PendingIntent.FLAG_MUTABLE else 0
                session.commit(PendingIntent.getActivity(activity, 0, intent, PendingIntent.FLAG_UPDATE_CURRENT or mutable).intentSender)
            }
        } catch (e: Exception) {
            installer.abandonSession(id)
            throw e
        }
    }

    /** The installer's answer, delivered to the activity. True when [intent] was one. */
    fun onIntent(intent: Intent?): Boolean {
        if (intent?.action != ACTION_INSTALL_STATUS) return false
        val status = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)
        when (status) {
            PackageInstaller.STATUS_PENDING_USER_ACTION -> {
                val confirm = confirmIntent(intent)
                try {
                    if (confirm == null) throw IllegalStateException("no confirm intent")
                    activity.startActivity(confirm)
                } catch (e: Exception) {
                    Log.w(TAG, "install dialog: $e")
                    phase = Phase.READY
                }
            }
            PackageInstaller.STATUS_SUCCESS -> {
                // Usually seen by the new version, started again by this intent.
                Log.i(TAG, "updated to ${BuildConfig.VERSION_NAME}")
                phase = Phase.IDLE
                Toast.makeText(activity, "Đã cập nhật NANOCast ${BuildConfig.VERSION_NAME} / Updated", Toast.LENGTH_LONG).show()
            }
            else -> {
                Log.w(TAG, "install: status $status ${intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE)}")
                val conflict = status == PackageInstaller.STATUS_FAILURE_CONFLICT || status == PackageInstaller.STATUS_FAILURE_INCOMPATIBLE
                phase = if (found == null) Phase.IDLE else if (conflict) Phase.OTHER_KEY else Phase.READY
            }
        }
        show()
        return true
    }

    @Suppress("DEPRECATION")
    private fun confirmIntent(intent: Intent): Intent? =
        if (Build.VERSION.SDK_INT >= 33) intent.getParcelableExtra(Intent.EXTRA_INTENT, Intent::class.java)
        else intent.getParcelableExtra(Intent.EXTRA_INTENT)

    companion object {
        private const val TAG = "NANOCast.Update"
        private const val REPO = "https://github.com/congluuit/NANOCast"
        private const val PREFS = "nanocast"
        private const val PREF_CHECKED = "update_checked"
        private const val ACTION_INSTALL_STATUS = "com.redharegames.nanocast.receiver.INSTALL_STATUS"
        private const val TICK_MS = 30_000L
        private const val CHECK_EVERY_MS = 6 * 3600_000L
        /** After a failed check (no network, release still uploading). */
        private const val RETRY_MS = 30 * 60_000L
        private const val INSTALL_TIMEOUT_MS = 5 * 60_000L
        private const val MAX_APK = 64L * 1024 * 1024
        private val TAG_RE = Regex("""/tag/v?(\d+\.\d+\.\d+)/?$""")
        private val NAME_RE = Regex("""(\d+\.\d+\.\d+)""")

        /** "0.2.0" > "0.1.10"; a suffix after `-` is ignored. */
        fun newer(version: String, than: String): Boolean {
            fun parts(v: String) = v.substringBefore('-').split('.').map { it.toIntOrNull() ?: 0 }.let { it + List(3) { 0 } }.take(3)
            val a = parts(version)
            val b = parts(than)
            for (i in 0 until 3) if (a[i] != b[i]) return a[i] > b[i]
            return false
        }

        private fun versionIn(name: String) = if (name.endsWith(".apk")) NAME_RE.find(name)?.value else null

        private fun isApk(f: File): Boolean = f.length() > 1024 && f.inputStream().use { s ->
            val head = ByteArray(4)
            s.read(head) == 4 && head.contentEquals(byteArrayOf(0x50, 0x4B, 0x03, 0x04))
        }
    }
}
