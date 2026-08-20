package com.nova.echo

import android.annotation.SuppressLint
import android.content.Context
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue

/**
 * Everything the gear icon controls.
 *
 * These are *requests*, not facts. The host negotiates its own answer and the
 * grant is authoritative — [EchoController.onGranted] configures the decoder
 * from what came back, never from what was asked for, and a host that caps
 * H264 at 24 fps because of Level 5.2 will simply do that. So the sliders here
 * are honest about being an opening bid: nothing in the app reports these
 * values back as though they were the running configuration.
 */
data class StreamPrefs(
    /** `hevc`, `av1`, or `h264`. Sent verbatim; the host parses these names. */
    val codec: String = "hevc",
    /** Opening bid for video, in kbps. The host applies its own ceiling. */
    val bitrateKbps: Int = 20_000,
    /** `WxH`. Nova accepts this and shorthand like `1080p` equally. */
    val resolution: String = "1920x1080",
    val fps: Int = 60,
    /**
     * Whether the phone microphone should be forwarded to the PC.
     *
     * Persisted here as *intent*: it survives a process death, and the capture
     * itself only exists while a session does. Turning it on outside a session
     * is therefore not an error — it takes effect at the next grant.
     */
    val micEnabled: Boolean = false,
    /**
     * Whether to show raw fingerprints, hashes and the event log.
     *
     * Defaults off. A 64-character SHA-256 on the front page is the single
     * biggest reason this app read as a debug harness, and it is a string
     * nobody can verify by eye anyway — the app compares fingerprints itself
     * and says "PAIRED". The people who genuinely need the hex are the people
     * who will find this switch.
     */
    val showTelemetry: Boolean = false,
    /** What this phone calls itself when pairing. */
    val deviceName: String = "Echo Android",
) {
    /** Human label for the codec, for the one place it is displayed. */
    val codecLabel: String get() = when (codec) {
        "av1" -> "AV1"
        "h264" -> "H.264"
        else -> "HEVC"
    }
}

/**
 * Persisted global settings.
 *
 * Reuses the `echo` SharedPreferences file the setup screen already wrote to,
 * so an install that predates this keeps its device name and relay details
 * rather than starting blank.
 *
 * State is exposed as a Compose `mutableStateOf` rather than a StateFlow: every
 * reader is a composable, and this way a write recomposes exactly the callers
 * that read it, with no collector to wire up. Writes go to disk through
 * `apply()`, which is asynchronous, so no frame pays for the IO.
 */
class EchoSettings private constructor(context: Context) {

    private val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    var prefsState by mutableStateOf(load())
        private set

    private fun load(): StreamPrefs {
        val d = StreamPrefs()
        return StreamPrefs(
            codec = prefs.getString(KEY_CODEC, d.codec) ?: d.codec,
            bitrateKbps = prefs.getInt(KEY_BITRATE, d.bitrateKbps),
            resolution = prefs.getString(KEY_RES, d.resolution) ?: d.resolution,
            fps = prefs.getInt(KEY_FPS, d.fps),
            micEnabled = prefs.getBoolean(KEY_MIC, d.micEnabled),
            showTelemetry = prefs.getBoolean(KEY_TELEMETRY, d.showTelemetry),
            deviceName = prefs.getString(KEY_DEVICE, d.deviceName) ?: d.deviceName,
        )
    }

    /**
     * Pick a resolution, and move the bitrate to match it.
     *
     * The snap is the point: 4K at the bitrate that suited 720p looks like a
     * broken decoder, and 720p at a 4K bitrate wastes a link that may be
     * charging for it. Nobody wants to maintain that relationship by hand every
     * time they change one of the two, so changing either re-derives the other.
     *
     * A deliberate consequence: this **discards a manual bitrate**. That is the
     * right trade because the manual figure was chosen for the old resolution
     * and is rarely still right for the new one — and re-overriding is one drag
     * of a slider that now starts from a sensible place.
     */
    fun selectResolution(resolution: String) = edit {
        it.copy(resolution = resolution, bitrateKbps = recommendedBitrateKbps(resolution, it.fps))
    }

    /** Pick a framerate, and move the bitrate to match. See [selectResolution]. */
    fun selectFps(fps: Int) = edit {
        it.copy(fps = fps, bitrateKbps = recommendedBitrateKbps(it.resolution, fps))
    }

    /** Change one or more settings; the write and the recomposition are atomic. */
    fun edit(change: (StreamPrefs) -> StreamPrefs) {
        val next = change(prefsState)
        prefs.edit()
            .putString(KEY_CODEC, next.codec)
            .putInt(KEY_BITRATE, next.bitrateKbps)
            .putString(KEY_RES, next.resolution)
            .putInt(KEY_FPS, next.fps)
            .putBoolean(KEY_MIC, next.micEnabled)
            .putBoolean(KEY_TELEMETRY, next.showTelemetry)
            .putString(KEY_DEVICE, next.deviceName)
            .apply()
        prefsState = next
    }

    companion object {
        private const val PREFS = "echo"
        private const val KEY_CODEC = "pref_codec"
        private const val KEY_BITRATE = "pref_bitrate_kbps"
        private const val KEY_RES = "pref_resolution"
        private const val KEY_FPS = "pref_fps"
        private const val KEY_MIC = "pref_mic"
        private const val KEY_TELEMETRY = "pref_telemetry"
        private const val KEY_DEVICE = "device_name"

        /** Bitrate slider range, in Mbps. */
        const val MIN_MBPS = 10
        const val MAX_MBPS = 100

        val CODECS = listOf(
            Triple("hevc", "HEVC (H.265)", "Default. Best quality per bit on every phone that decodes it."),
            Triple("av1", "AV1", "Needs an RTX 40-series host and a phone that decodes AV1."),
            Triple("h264", "H.264", "Fallback. Decodes anywhere, costs the most bandwidth."),
        )

        val RESOLUTIONS = listOf("1280x720", "1920x1080", "2560x1440", "3840x2160")

        val FRAMERATES = listOf(60, 120)

        /**
         * Recommended bitrate for a resolution and framerate, in Mbps at 60 fps.
         *
         * Interpolated on **pixel count**, not matched against this list, so a
         * resolution that never appears in the UI still lands somewhere sane.
         * Tune the table, not the call sites.
         *
         * These are working targets, roughly half of Nova's own ceiling in
         * `qos::resolution_ceiling` — the host's figure is the most it will
         * allow, which is the wrong thing to hand someone as a default. A
         * default should look good on an ordinary home network; the slider is
         * there for anyone who wants to spend the rest of their link.
         */
        private val BITRATE_TIERS = listOf(
            921_600L to 10,      // 1280x720
            2_073_600L to 25,    // 1920x1080
            3_686_400L to 45,    // 2560x1440
            8_294_400L to 60,    // 3840x2160
        )

        /**
         * How bitrate scales with framerate.
         *
         * Sub-linear, and the same exponent the host uses: doubling the frame
         * rate does not double the bits needed, because consecutive frames are
         * more alike the closer together they are and inter-frame prediction
         * gets correspondingly cheaper. Linear scaling would ask for 120 fps at
         * twice the bits and mostly buy padding.
         */
        private const val FPS_EXPONENT = 0.75

        /**
         * The bitrate this resolution and framerate should start at, in kbps.
         *
         * Always inside the slider's own range, so the value it returns is one
         * the user can see and move.
         */
        fun recommendedBitrateKbps(resolution: String, fps: Int): Int {
            val pixels = pixelsOf(resolution)
            val base = interpolateTier(pixels)
            val scaled = base * Math.pow(fps.toDouble() / 60.0, FPS_EXPONENT)
            return scaled.toInt().coerceIn(MIN_MBPS, MAX_MBPS) * 1000
        }

        /** `WxH` to a pixel count, falling back to 1080p for anything unparseable. */
        private fun pixelsOf(resolution: String): Long {
            val parts = resolution.lowercase().split('x')
            val w = parts.getOrNull(0)?.trim()?.toLongOrNull()
            val h = parts.getOrNull(1)?.trim()?.toLongOrNull()
            return if (w != null && h != null && w > 0 && h > 0) w * h else 2_073_600L
        }

        /** Linear interpolation between tiers; flat outside the ends. */
        private fun interpolateTier(pixels: Long): Double {
            val first = BITRATE_TIERS.first()
            val last = BITRATE_TIERS.last()
            if (pixels <= first.first) return first.second.toDouble()
            if (pixels >= last.first) return last.second.toDouble()
            for (i in 0 until BITRATE_TIERS.size - 1) {
                val (lowPx, lowMbps) = BITRATE_TIERS[i]
                val (highPx, highMbps) = BITRATE_TIERS[i + 1]
                if (pixels in lowPx..highPx) {
                    val t = (pixels - lowPx).toDouble() / (highPx - lowPx).toDouble()
                    return lowMbps + t * (highMbps - lowMbps)
                }
            }
            return last.second.toDouble()
        }

        @SuppressLint("StaticFieldLeak")
        private var instance: EchoSettings? = null

        /** The one settings object for this process. */
        @Synchronized
        fun of(context: Context): EchoSettings =
            instance ?: EchoSettings(context.applicationContext).also { instance = it }
    }
}
