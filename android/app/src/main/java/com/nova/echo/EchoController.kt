package com.nova.echo

import android.util.Log
import android.view.Surface
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import org.json.JSONObject
import kotlin.concurrent.thread

/** What the UI needs to know. */
data class UiState(
    val status: String = "Idle",
    val pin: String? = null,
    val hostFingerprint: String? = null,
    val myFingerprint: String = "",
    val streaming: Boolean = false,
    val error: String? = null,
    val log: List<String> = emptyList(),
)

/**
 * Owns the native handle and the thread that polls it.
 *
 * One handle at a time, one polling thread, and both are torn down together.
 * Pairing and streaming share this machinery because they share an event stream
 * on the Rust side — which is why [nativePair] and [nativeConnect] return the
 * same kind of handle.
 *
 * ## Why the handle is guarded
 *
 * `nativeClose` frees the Rust allocation. A second close, or a poll racing a
 * close, would be a use-after-free reachable from Kotlin. The Rust side carries
 * a magic value that catches the obvious cases, but relying on it under
 * concurrency would be racy, so the handle is only ever read and cleared under
 * [lock].
 */
class EchoController(private val filesDir: String) {

    var state by mutableStateOf(UiState())
        private set

    private val lock = Any()
    private var handle: Long = 0
    private var poller: Thread? = null
    private var player: VideoPlayer? = null

    /** Surface to decode onto, supplied by the SurfaceView once it is ready. */
    @Volatile var surface: Surface? = null

    fun init() {
        EchoNative.nativeInit()
        thread(name = "echo-identity") {
            // RSA-2048 generation on first run; off the main thread because it
            // is the one call here that takes a noticeable moment.
            val fp = runCatching { EchoNative.nativeIdentityFingerprint(filesDir) }
                .getOrElse { "unavailable: ${it.message}" }
            post { it.copy(myFingerprint = fp) }
        }
    }

    fun pair(host: String, deviceName: String) {
        val config = JSONObject()
            .put("identity_dir", filesDir)
            .put("host", host)
            .put("device_name", deviceName)
            .put("consent_secs", 180)
        start(config.toString(), pairing = true)
    }

    fun connect(relayUrl: String, relayPin: String, hostFingerprint: String) {
        val config = JSONObject()
            .put("identity_dir", filesDir)
            .put("relay_url", relayUrl)
            .put("relay_pin", relayPin)
            .put("host_fingerprint", hostFingerprint)
            .put("res", "1920x1080")
            .put("fps", 60)
            .put("codec", "hevc")
            .put("bitrate_kbps", 20000)
        start(config.toString(), pairing = false)
    }

    private fun start(configJson: String, pairing: Boolean) {
        stop()
        post { it.copy(status = if (pairing) "Pairing…" else "Connecting…", error = null, pin = null, log = emptyList()) }

        val h = try {
            if (pairing) EchoNative.nativePair(configJson) else EchoNative.nativeConnect(configJson)
        } catch (e: Throwable) {
            // Rust throws only for a malformed config or a runtime that will not
            // start; everything reachable later arrives as an error event.
            post { it.copy(status = "Failed", error = e.message ?: "native call failed") }
            return
        }
        if (h == 0L) {
            post { it.copy(status = "Failed", error = "native returned a null handle") }
            return
        }

        synchronized(lock) { handle = h }
        poller = thread(name = "echo-events") { pollLoop(h) }
    }

    private fun pollLoop(h: Long) {
        while (true) {
            val json = try {
                EchoNative.nativePollEvent(h, 500)
            } catch (e: Throwable) {
                post { it.copy(status = "Failed", error = e.message) }
                return
            } ?: run {
                // Timeout. Stop if the handle was closed underneath us.
                if (synchronized(lock) { handle } != h) return
                continue
            }

            val event = runCatching { JSONObject(json) }.getOrNull() ?: continue
            Log.i(TAG, json)
            post { it.copy(log = (it.log + describe(event)).takeLast(40)) }

            when (event.optString("type")) {
                "awaiting_consent" -> post {
                    it.copy(status = "Type this PIN into Nova", pin = event.optString("pin"))
                }
                "verified", "paired" -> post {
                    it.copy(
                        status = "Paired",
                        pin = null,
                        hostFingerprint = event.optString("fingerprint", it.hostFingerprint ?: ""),
                    )
                }
                "granted" -> onGranted(event, h)
                "error" -> post { it.copy(status = "Failed", error = event.optString("message")) }
                "closed", "ended" -> {
                    post { it.copy(status = "Ended", streaming = false) }
                    return
                }
            }
        }
    }

    private fun onGranted(event: JSONObject, h: Long) {
        val target = surface
        if (target == null) {
            post { it.copy(status = "Failed", error = "no surface to decode onto") }
            return
        }
        // Geometry comes from the host's grant, never from what we asked for —
        // the host is free to give us something else, and configuring the
        // decoder for the request would then produce a stream it cannot decode.
        val width = event.optInt("width", 1920)
        val height = event.optInt("height", 1080)
        val codec = event.optString("codec", "hevc")

        player = VideoPlayer(h, target, width, height, codec) { message ->
            post { it.copy(status = "Failed", error = message) }
        }.also { it.start() }

        post { it.copy(status = "Streaming ${width}x$height $codec", streaming = true) }
    }

    fun stats(): String = synchronized(lock) {
        if (handle == 0L) "{}" else runCatching { EchoNative.nativeStats(handle) }.getOrDefault("{}")
    }

    fun stop() {
        player?.stop()
        player = null
        // Clear the handle before closing, so the poller sees the change and a
        // second stop() cannot close the same pointer twice.
        val h = synchronized(lock) { val current = handle; handle = 0; current }
        if (h != 0L) EchoNative.nativeClose(h)
        poller?.join(1_500)
        poller = null
        post { it.copy(streaming = false) }
    }

    private fun describe(event: JSONObject): String = when (event.optString("type")) {
        "socket_bound" -> "socket ${event.optString("local")}"
        "public_address" -> "public ${event.optString("mapped")}"
        "local_candidate" -> "lan ${event.optString("addr")}"
        "relay_connected" -> "relay ${event.optString("authority")}"
        "host_candidates" -> "host candidates ${event.optJSONArray("addrs")}"
        "path_open" -> "path open ${event.optString("peer")} (${event.optInt("rounds")} rounds)"
        "control_authenticated" -> "tunnel authenticated"
        "granted" -> "session ${event.optInt("session_id")} granted"
        "refused" -> "refused: ${event.optString("reason")}"
        else -> event.optString("type")
    }

    private fun post(update: (UiState) -> UiState) {
        // Compose state is safe to write from any thread; reads recompose.
        state = update(state)
    }

    private companion object { const val TAG = "EchoController" }
}
