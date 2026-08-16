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
    /** The user's intent for the microphone; persists across sessions. */
    val micEnabled: Boolean = false,
    /** Whether audio is actually being captured right now. */
    val micActive: Boolean = false,
    /**
     * Why the microphone is not running despite being switched on — no
     * permission, no Opus encoder, no microphone.
     *
     * Separate from [error] because it must not present as a session failure:
     * the stream is fine, one optional feature is not.
     */
    val micProblem: String? = null,
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
class EchoController(private val context: android.content.Context) {

    private val filesDir: String = context.filesDir.absolutePath

    var state by mutableStateOf(UiState())
        private set

    private val lock = Any()
    private var handle: Long = 0
    private var poller: Thread? = null
    private var player: VideoPlayer? = null
    private var mic: MicCapture? = null

    /**
     * Master switch for forwarding input. Read by the Activity's key dispatch
     * as well as the surface view, so both must consult the same answer.
     */
    @Volatile var inputEnabled: Boolean = true

    /**
     * Surface to decode onto, supplied by the SurfaceView as one appears and
     * disappears.
     *
     * Assigning this while a stream is live re-attaches the running decoder
     * rather than being remembered for next time. A SurfaceView's Surface is
     * destroyed on backgrounding, screen lock, and any move to another display;
     * before this forwarded, the decoder kept rendering into the destroyed one
     * and the picture froze while the network stayed perfectly healthy.
     */
    @Volatile var surface: Surface? = null
        set(value) {
            field = value
            player?.setSurface(value)
        }

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
        // From here on the keepalive threads must survive backgrounding, and
        // that includes pairing: waiting for someone to walk to the PC and type
        // a PIN is exactly when the user switches away from the app.
        EchoService.start(context)
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

        // The microphone starts only once a session exists. Its channel is
        // created with the handle, but nothing drains it until the host grants
        // the session and the transport has a key — so audio captured before
        // this point would sit in a queue and then arrive as a burst of stale
        // speech the moment the stream came up.
        if (state.micEnabled) startMic(h)
    }

    // ── Microphone ──────────────────────────────────────────────────────────

    /**
     * Turn the microphone on or off.
     *
     * The switch is the user's *intent* and is remembered across sessions; the
     * capture itself only exists while a session does. Turning it on outside a
     * session is therefore not an error — it takes effect at the next grant.
     */
    fun setMicEnabled(enabled: Boolean) {
        post { it.copy(micEnabled = enabled, micProblem = if (enabled) it.micProblem else null) }
        if (enabled) {
            val h = synchronized(lock) { handle }
            if (h != 0L && state.streaming) startMic(h)
        } else {
            stopMic()
        }
    }

    /**
     * Re-evaluate after the user answers the permission dialog.
     *
     * Also restarts the foreground service, because its type is chosen at
     * `startForeground` time: a service that came up before the permission
     * existed is running as `mediaPlayback` alone and may not legally capture
     * while backgrounded until it is started again.
     */
    fun onMicPermissionResult(granted: Boolean) {
        if (!granted) {
            post { it.copy(micEnabled = false, micProblem = "microphone permission denied") }
            return
        }
        EchoService.start(context)
        val h = synchronized(lock) { handle }
        if (h != 0L && state.streaming && state.micEnabled) startMic(h)
    }

    private fun startMic(h: Long) {
        if (mic?.isRunning == true) return
        val capture = MicCapture(context, h) { message ->
            // A microphone failure is not a session failure: the stream keeps
            // running and only this feature goes quiet, so it is reported in
            // its own field rather than as `error`.
            post { it.copy(micActive = false, micProblem = message) }
        }
        mic = capture
        val started = capture.start()
        post { it.copy(micActive = started, micProblem = if (started) null else it.micProblem) }
    }

    private fun stopMic() {
        mic?.stop()
        mic = null
        post { it.copy(micActive = false) }
    }

    // ── Input ───────────────────────────────────────────────────────────────
    // Fire-and-forget by design: the UI thread must never wait on a network
    // round trip to report a keystroke. Rust queues the packet and returns.

    private fun send(kind: Int, a: Int, b: Int = 0, c: Int = 0, d: Int = 0) {
        val h = synchronized(lock) { handle }
        if (h != 0L) EchoNative.nativeSendInput(h, kind, a, b, c, d)
    }

    fun mouseMove(dx: Int, dy: Int) = send(EchoNative.INPUT_MOUSE_MOVE, dx, dy)

    fun mouseAbsolute(x: Int, y: Int, width: Int, height: Int) =
        send(EchoNative.INPUT_MOUSE_ABS, x, y, width, height)

    fun mouseButton(button: Int, down: Boolean) =
        send(EchoNative.INPUT_MOUSE_BUTTON, button, if (down) 1 else 0)

    fun scroll(amount: Int) = send(EchoNative.INPUT_SCROLL, amount)

    /** Returns whether the key was recognised — unmapped keys must not be sent. */
    fun key(androidKeyCode: Int, down: Boolean, metaState: Int): Boolean {
        val vk = Keycodes.toWindows(androidKeyCode)
        if (vk == 0) return false
        send(EchoNative.INPUT_KEY, vk, if (down) 1 else 0, Keycodes.modifiers(metaState))
        return true
    }

    /**
     * Release every modifier and mouse button on the host.
     *
     * Cheap and idempotent, so it is sent generously: whenever input stops
     * mid-gesture the key-up never went, and the host is left with a key held.
     */
    fun releaseAllInput() = send(EchoNative.INPUT_RELEASE_ALL, 0)

    fun stats(): String = synchronized(lock) {
        if (handle == 0L) "{}" else runCatching { EchoNative.nativeStats(handle) }.getOrDefault("{}")
    }

    fun stop() {
        // Before the handle goes: a session that ends mid-keystroke would
        // otherwise leave that key held on the host with nothing left to
        // release it.
        releaseAllInput()
        // Before the handle goes: the capture thread calls `nativeSendMic` with
        // it, and a handle freed underneath a running thread is a use-after-free
        // the magic check would only sometimes catch.
        stopMic()
        player?.stop()
        player = null
        // Clear the handle before closing, so the poller sees the change and a
        // second stop() cannot close the same pointer twice.
        val h = synchronized(lock) { val current = handle; handle = 0; current }
        if (h != 0L) EchoNative.nativeClose(h)
        poller?.join(1_500)
        poller = null
        EchoService.stop(context)
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
