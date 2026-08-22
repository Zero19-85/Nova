package com.nova.echo

import android.annotation.SuppressLint
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
    /**
     * Whether a native session handle exists — i.e. whether there is anything to
     * send a command through.
     *
     * Deliberately distinct from [streaming], which only becomes true once video
     * is flowing: a session that is connecting, pairing, or waiting for a grant
     * has a live handle and no picture.
     *
     * It exists because the answer used to be invisible to the UI. [stop] frees
     * the handle and zeroes it, so a second press found `handle == 0`, skipped
     * `nativeClose`, and sent nothing at all — a button that looked live and did
     * nothing (reported 2026-08-17).
     *
     * The dashboard button reads this to decide WHICH action it performs, not
     * whether to appear: with a handle it stops the session directly, without
     * one it goes through [releaseHostSession]. Hiding it instead was a worse
     * answer — it removed the only control that could release a session the host
     * was still holding.
     */
    val connected: Boolean = false,
    /**
     * Which route the live path took: `lan`, `wan_punch`, `direct_wan`.
     *
     * Set from the `path_open` event and cleared when the session ends, so it
     * is never a stale claim about a path that no longer exists.
     *
     * **The engine decides this, not the app.** Rust classifies from the peer
     * address the punch actually latched — which is why a relay-signalled
     * session that landed on a private address correctly reports `lan`. Any
     * attempt to infer it here from what was configured would get exactly that
     * case wrong, and it is the case that matters most: it is the fast one.
     */
    val transport: String? = null,
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
    /**
     * Whether the host's game audio is playing.
     *
     * No `audioEnabled` beside it, unlike the microphone: downstream audio needs
     * no permission and carries no privacy question, so there is nothing for a
     * user to opt into. The device's own volume control is the off switch.
     */
    val audioActive: Boolean = false,
    /** Why game audio is not playing. Not an [error], for the same reason as [micProblem]. */
    val audioProblem: String? = null,
    /**
     * Whether video is being delayed to match audio.
     *
     * Off by default, and that default is a judgement rather than caution: this
     * buys A/V sync with input latency, which is the wrong trade while playing
     * and the right one while watching. Only the user knows which they are doing.
     */
    val syncEnabled: Boolean = false,
    /** How much video is currently being held back, in milliseconds. */
    val videoDelayMs: Int = 0,
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
class EchoController private constructor(private val context: android.content.Context) {


    private val filesDir: String = context.filesDir.absolutePath

    var state by mutableStateOf(UiState())
        private set

    private val lock = Any()
    private var handle: Long = 0
    private var poller: Thread? = null
    private var player: VideoPlayer? = null
    private var mic: MicCapture? = null
    private var gameAudio: GameAudioPlayer? = null

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
            // `player?` is the whole subtlety: between a session being granted
            // and the decoder existing, a Surface can arrive and be recorded
            // with nothing to push it to. That is why the player's creation
            // reads this field back rather than trusting whatever was current
            // when the grant landed — the two orderings have to converge on
            // the same Surface, and the field is the one always up to date.
            player?.setSurface(value)
        }

    fun init() {
        EchoNative.nativeInit()
        // The microphone switch is *intent* and is persisted, so it has to be
        // restored here rather than defaulting off on every process start —
        // otherwise the settings sheet and the controller disagree about a
        // setting the user already made, and the mic silently fails to start at
        // the next grant.
        post { it.copy(micEnabled = EchoSettings.of(context).prefsState.micEnabled) }
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

    /**
     * Open a streaming session.
     *
     * [prefs] carries the user's opening bid for the stream. It is a REQUEST:
     * the host negotiates from it and the grant is authoritative, which is why
     * [onGranted] configures the decoder from the event rather than from these
     * values. A host that caps H264 at 24 fps for Level 5.2, or clamps the
     * bitrate to the resolution ceiling, is doing its job.
     *
     * Takes the whole [KnownHost] rather than four strings. The relay URL, its
     * pin, the fingerprint and the endpoints are four values that only mean
     * anything together — passing them separately is how a caller ends up
     * dialling one host's relay with another host's fingerprint, which fails at
     * the TLS pin with a message about certificates rather than about the mixup.
     *
     * [appId] is which Nova app the session opens into — see [LaunchMode] and
     * [QUICK_START_APP_ID]. It defaults to Desktop so every existing caller
     * keeps its behaviour, and unlike [prefs] it is NOT negotiable: the host
     * either launches that app or it does not.
     */
    fun connect(host: KnownHost, prefs: StreamPrefs, appId: Int = LaunchMode.Mirror.appId) {
        val asked = prefs
        val effective = decodableOrDowngraded(prefs)
        if (effective == null) {
            post {
                it.copy(status = "Failed", error =
                    "This device cannot decode ${asked.resolution} at any supported " +
                    "codec or frame rate. Pick a lower resolution.")
            }
            return
        }
        if (effective.codec != asked.codec || effective.fps != asked.fps) {
            // Logged, not posted: the downgrade becomes visible to the user in
            // the status line the moment video starts ("Streaming 3840x2160@60
            // av1"), which reports the mode actually running rather than a
            // claim about it. Adding a UiState field that no screen renders
            // would be dead weight carried for one message.
            Log.w(TAG, "requested ${asked.codec}@${asked.fps} at ${asked.resolution} " +
                       "is not decodable by this device — asking the host for " +
                       "${effective.codec}@${effective.fps} instead")
        }
        start(transportConfig(host).apply {
            put("res", effective.resolution)
            put("fps", effective.fps)
            put("codec", effective.codec)
            put("bitrate_kbps", effective.bitrateKbps)
            put("app_id", appId)
        }.toString(), pairing = false)
    }

    /**
     * [prefs], adjusted to something this device's decoder will actually accept,
     * or null if nothing at this resolution will do.
     *
     * This is where the "renegotiation" happens, and it happens BEFORE the
     * request rather than after the grant. Echo has no in-band way for a client
     * to tell a host "that format is unsupported" — and it needs none, because
     * the client is what proposes res/fps/codec in `start_session`. Asking for
     * a mode this phone cannot decode and then backing out costs a round trip,
     * a display activation on the host, and a torn-down session, all to learn
     * something `MediaCodecInfo` would have answered for free.
     *
     * The bitrate is recomputed for the mode actually requested: it was chosen
     * for the old frame rate, and carrying it across unchanged would ask for a
     * 120fps budget on a 60fps stream.
     */
    private fun decodableOrDowngraded(prefs: StreamPrefs): StreamPrefs? {
        val (w, h) = parseResolution(prefs.resolution) ?: return prefs
        val mode = VideoPlayer.Support.bestSupported(prefs.codec, w, h, prefs.fps) ?: return null
        if (mode.codec == prefs.codec && mode.fps == prefs.fps) return prefs
        return prefs.copy(
            codec = mode.codec,
            fps = mode.fps,
            bitrateKbps = EchoSettings.recommendedBitrateKbps(prefs.resolution, mode.fps),
        )
    }

    /**
     * `"1920x1080"` to a pixel pair. Returns null for anything unrecognised, and
     * the caller then passes the request through untouched — an unparsed
     * resolution is not evidence that the mode is unsupported, and refusing on
     * it would break a perfectly good session over a spelling this function
     * does not happen to know.
     */
    private fun parseResolution(text: String): Pair<Int, Int>? {
        val parts = text.lowercase().split('x', '*')
        if (parts.size != 2) return null
        val w = parts[0].trim().toIntOrNull() ?: return null
        val h = parts[1].trim().toIntOrNull() ?: return null
        return if (w > 0 && h > 0) w to h else null
    }

    /**
     * The half of the session config that decides *how* the host is reached.
     *
     * Shared by [connect] and [releaseHostSession] so both take the same route.
     * A release that went over the relay while the session it is ending was on
     * the LAN would be a bug visible only from the one network nobody tests on.
     *
     * `lan_endpoint` is omitted when this host has no locally-observed address:
     * Rust skips stage 1 entirely when it is absent, and that is the honest
     * default — an address we have never seen this host answer on is a guess
     * paid for on every connect.
     */
    private fun transportConfig(host: KnownHost): JSONObject = JSONObject()
        .put("identity_dir", filesDir)
        .put("relay_url", host.relayUrl ?: "")
        .put("relay_pin", host.relayPin ?: "")
        .put("host_fingerprint", host.fingerprint)
        .apply {
            host.lanAddress?.takeIf { it.isNotBlank() }
                ?.let { put("lan_endpoint", "$it:${host.port}") }
            host.wanEndpoint?.takeIf { it.isNotBlank() }?.let { put("wan_endpoint", it) }
        }

    /**
     * End a session still held on the host when this app has no handle to it.
     *
     * The case this exists for: the app was swiped away, killed, or lost the
     * network without sending `stop_session`. The host does not tear down on
     * that any more — it DETACHES, holding the virtual display so a reconnect is
     * instant — so the session outlives the app, and reopening the app produces
     * a fresh process that knows nothing about it. [stop] is useless there,
     * because the handle it would send through no longer exists.
     *
     * One punch, one `stop_session`, no session of our own — see
     * [EchoNative.nativeRelease]. The first version of this drove a real
     * connection and stopped it the moment the grant arrived, which raced the
     * session it had just created: the host logged reclaim, start, silence, and
     * the user pressed the button four times for two teardowns (2026-08-17).
     * Ending a session never needed a session.
     */
    fun releaseHostSession(host: KnownHost) {
        post { it.copy(status = "Ending the session on the host…", error = null) }
        val config = transportConfig(host)
        thread(name = "echo-release") {
            try {
                val answer = EchoNative.nativeRelease(config.toString())
                post { it.copy(status = answer, error = null) }
            } catch (e: Throwable) {
                // The host being unreachable is the ordinary failure here — it
                // may be asleep, or the session may already be gone.
                post {
                    it.copy(
                        status = "Could not reach the host",
                        error = e.message ?: "release failed",
                    )
                }
            }
        }
    }

    private fun start(configJson: String, pairing: Boolean) {
        stop()
        post {
            it.copy(
                status = if (pairing) "Pairing…" else "Connecting…",
                error = null,
                pin = null,
                log = emptyList(),
                // Cleared for a pairing run so the fingerprint that arrives is
                // unambiguously THIS handshake's. The dashboard attributes it to
                // whichever card started the flow, and a value left over from a
                // previous pairing would be attributed to the wrong machine —
                // marking a host paired that never was.
                hostFingerprint = if (pairing) null else it.hostFingerprint,
            )
        }

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
        post { it.copy(connected = true) }
        // From here on the keepalive threads must survive backgrounding, and
        // that includes pairing: waiting for someone to walk to the PC and type
        // a PIN is exactly when the user switches away from the app.
        EchoService.start(context)
        poller = thread(name = "echo-events") { pollLoop(h) }
    }

    /** Whether [handle] is still the session this thread is polling for. */
    private fun stillOurs(h: Long): Boolean = synchronized(lock) { handle } == h

    private fun pollLoop(h: Long) {
        while (true) {
            // Checked before EVERY native call, not only after a timeout.
            // `stop()` clears the handle before it frees the pointer, so this is
            // what keeps the poller out of a session that is being torn down.
            // The old version only tested on the timeout path, which left the
            // whole "handled an event, looped round" edge unguarded — and that
            // is the edge a manual disconnect lands on, because stopping while
            // events are flowing is the normal case rather than a rare one.
            if (!stillOurs(h)) return

            val json = try {
                EchoNative.nativePollEvent(h, 500)
            } catch (e: Throwable) {
                // A throw while the handle is already gone is the session
                // ending underneath us, not a failure worth showing anyone.
                // Belt and braces: the Rust side answers `null` for an
                // unrecognised handle now, so this should no longer fire for a
                // disconnect — but the guard costs nothing and the alternative
                // is a teardown race presenting as `Failed`.
                if (stillOurs(h)) post { it.copy(status = "Failed", error = e.message) }
                return
            } ?: continue

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
                // The route the engine settled on. Recorded before the grant
                // arrives, so the badge is already right when the picture
                // appears rather than a frame behind it.
                "path_open" -> post {
                    it.copy(transport = event.optString("transport").ifBlank { null })
                }
                "granted" -> onGranted(event, h)
                "error" -> post { it.copy(status = "Failed", error = event.optString("message")) }
                "closed", "ended" -> {
                    // `transport` goes with the session. Leaving it set would
                    // leave a card claiming a live LAN path to a host nothing
                    // is connected to.
                    post { it.copy(status = "Ended", streaming = false, transport = null) }
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
        // Same rule as width/height: read what the host GRANTED, never what was
        // requested. The decoder is configured for this cadence, so a wrong
        // value here misconfigures it rather than merely mislabelling a log.
        val fps = event.optInt("fps", 60)

        player = VideoPlayer(h, target, width, height, fps, codec) { message ->
            post { it.copy(status = "Failed", error = message) }
        }.also { it.start() }
        // Re-read the field: a surfaceCreated that landed while the decoder was
        // being built would have found `player == null` and gone nowhere,
        // leaving the decoder attached to a Surface that no longer exists.
        // Cheap, and skipped entirely when nothing changed.
        surface?.let { current -> if (current !== target) player?.setSurface(current) }

        post { it.copy(status = "Streaming ${width}x$height@${fps} $codec", streaming = true) }

        // The microphone starts only once a session exists. Its channel is
        // created with the handle, but nothing drains it until the host grants
        // the session and the transport has a key — so audio captured before
        // this point would sit in a queue and then arrive as a burst of stale
        // speech the moment the stream came up.
        if (state.micEnabled) startMic(h)

        // Game audio needs no permission and no user opt-in, so unlike the
        // microphone it simply starts with the stream. Safe here even though the
        // grant has only just landed: an unarmed buffer answers `AUDIO_IDLE` and
        // the loop idles until packets actually arrive.
        val audio = GameAudioPlayer(
            context,
            h,
            onError = { message ->
                // Losing audio is not losing the session — the picture keeps
                // running, so this reports in its own field rather than as
                // `error`.
                post { it.copy(audioActive = false, audioProblem = message) }
            },
            onLatency = { measured -> onAudioLatency(h, measured) },
        )
        gameAudio = audio
        val playing = audio.start()
        post { it.copy(audioActive = playing) }
    }

    // ── A/V sync engine ─────────────────────────────────────────────────────
    //
    // Audio is the late half and cannot be hurried: ~190 ms measured, of which
    // the device's own output stage is ~70 ms with the fast mixer path already
    // granted, and 80 ms is jitter insurance that measurement showed was needed.
    // Video renders as soon as it decodes. So sync is reached by delaying video
    // to meet audio, which is what every media player does and is the only
    // direction with any slack in it.
    //
    // The cost is input latency — a delayed frame is a delayed view of your own
    // mouse — so this is off by default and belongs to watching rather than
    // playing.

    /**
     * Turn A/V sync on or off. Off restores undelayed rendering immediately.
     */
    fun setSyncEnabled(enabled: Boolean) {
        post { it.copy(syncEnabled = enabled) }
        if (!enabled) {
            val h = synchronized(lock) { handle }
            if (h != 0L) EchoNative.nativeSetVideoDelay(h, 0)
            post { it.copy(videoDelayMs = 0) }
        }
        // Switching on applies at the next latency measurement rather than
        // guessing a value now: the audio pipeline reports every 10 s, and a
        // guessed delay that is then corrected is two visible steps instead of
        // one.
    }

    /**
     * The audio pipeline re-measured itself; match video to it.
     *
     * Subtracts the video pipeline's own cost, because the delay needed is the
     * DIFFERENCE between the two paths, not audio's latency outright — video
     * spends time in the decoder and the compositor too, and delaying by the
     * full audio figure would overshoot by exactly that much.
     */
    private fun onAudioLatency(h: Long, audioMs: Int) {
        if (!state.syncEnabled) return

        val target = (audioMs - VIDEO_PIPELINE_MS).coerceIn(0, 350)
        // Hysteresis. The measurement moves a few milliseconds every report as
        // buffer depth breathes, and re-timing the video for that would be a
        // visible hitch in exchange for a difference nobody can perceive.
        if (kotlin.math.abs(target - state.videoDelayMs) < SYNC_HYSTERESIS_MS) return

        val applied = EchoNative.nativeSetVideoDelay(h, target)
        if (applied >= 0) {
            Log.i(TAG, "A/V sync: audio ${audioMs}ms -> video delay ${applied}ms")
            post { it.copy(videoDelayMs = applied) }
        }
    }

    private fun stopGameAudio() {
        gameAudio?.stop()
        gameAudio = null
        post { it.copy(audioActive = false) }
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
        // Persisted here rather than at the call sites, because there are two of
        // them — the settings sheet and the in-stream overlay — and a switch
        // that only sticks when flipped from one of them is worse than one that
        // never sticks at all.
        EchoSettings.of(context).edit { it.copy(micEnabled = enabled) }
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
            EchoSettings.of(context).edit { it.copy(micEnabled = false) }
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
        // Same hazard, other direction: the playback thread calls
        // `nativePollAudio` with the handle, so it has to be joined before the
        // pointer is freed or the poll lands on freed memory.
        stopGameAudio()
        player?.stop()
        player = null
        // Clear the handle before closing, so the poller sees the change and a
        // second stop() cannot close the same pointer twice.
        val h = synchronized(lock) { val current = handle; handle = 0; current }
        if (h != 0L) EchoNative.nativeClose(h)
        poller?.join(1_500)
        poller = null
        EchoService.stop(context)
        // A deliberate stop is a clean landing, so it says so and clears
        // whatever the session left behind. `error` in particular: without this
        // an error from earlier in the session — or from a teardown race —
        // survives the disconnect and greets the user on the setup screen as
        // though the stop itself had failed. `pin` goes too, since a pairing
        // that was abandoned mid-flight would otherwise leave a stale code on
        // screen for a handshake nobody is running any more.
        post {
            it.copy(
                status = "Idle",
                error = null,
                pin = null,
                streaming = false,
                connected = false,
                transport = null,
            )
        }
    }

    private fun describe(event: JSONObject): String = when (event.optString("type")) {
        "socket_bound" -> "socket ${event.optString("local")}"
        "public_address" -> "public ${event.optString("mapped")}"
        "local_candidate" -> "lan ${event.optString("addr")}"
        "relay_connected" -> "relay ${event.optString("authority")}"
        "host_candidates" -> "host candidates ${event.optJSONArray("addrs")}"
        "lan_attempt" -> "LAN first: ${event.optString("endpoint")}"
        "lan_rendezvous" -> "LAN rendezvous accepted, host at ${event.optJSONArray("candidates")}"
        "lan_abandoned" -> "LAN abandoned — ${event.optString("reason")}"
        "path_open" -> "path open ${event.optString("peer")} via ${event.optString("transport")} " +
            "(${event.optInt("rounds")} rounds)"
        "control_authenticated" -> "tunnel authenticated"
        "granted" -> "session ${event.optInt("session_id")} granted"
        "refused" -> "refused: ${event.optString("reason")}"
        else -> event.optString("type")
    }

    private fun post(update: (UiState) -> UiState) {
        // Compose state is safe to write from any thread; reads recompose.
        state = update(state)
    }

    // Not private: [of] and [existing] are how MainActivity and EchoService
    // reach the one controller this process owns.
    companion object {
        const val TAG = "EchoController"

        // Lint sees a static field holding a Context and calls it a leak, which
        // is the right default: the usual version of this holds an Activity and
        // keeps it alive forever. `of` takes the APPLICATION context precisely
        // so there is nothing here to outlive — the application context IS
        // process-scoped, and so is the session this object owns.
        @SuppressLint("StaticFieldLeak")
        private var instance: EchoController? = null

        /**
         * The one controller for this process.
         *
         * **Deliberately outlives the Activity.** A session survives
         * backgrounding because [EchoService] keeps the process alive, so the
         * thing that owns the session must outlive the thing that draws it.
         * It used to be constructed in `MainActivity.onCreate` and stopped in
         * `onDestroy`, which meant an Activity teardown that the process
         * survived (memory pressure, a configuration change, "don't keep
         * activities") ended the session — and the replacement Activity built
         * a fresh controller with `handle == 0` and `player == null`, so every
         * later `surfaceCreated` landed on `player?.` and went nowhere. A live
         * stream, a black screen, and nothing logged.
         *
         * Takes the APPLICATION context. An Activity context here would leak
         * the destroyed Activity for the life of the process, which is the
         * standard cost of getting a singleton's context wrong.
         */
        @Synchronized
        fun of(context: android.content.Context): EchoController =
            instance ?: EchoController(context.applicationContext)
                .also { it.init(); instance = it }

        /** The controller, if one has been created. For [EchoService]. */
        @Synchronized
        fun existing(): EchoController? = instance

        /**
         * What the video path costs after the delay line: decode, composite,
         * and one display refresh.
         *
         * Subtracted from the audio measurement because sync needs the
         * DIFFERENCE between the paths, not audio's figure outright. An
         * estimate rather than a measurement — MediaCodec exposes no equivalent
         * of `AudioTimestamp` for a Surface — which is the one soft number in
         * this engine. If sync lands consistently off in one direction, this is
         * the constant to correct, and being wrong by 10 ms here costs 10 ms of
         * offset rather than anything structural.
         */
        const val VIDEO_PIPELINE_MS = 30

        /**
         * Ignore target changes smaller than this.
         *
         * The audio measurement breathes by a few milliseconds each report as
         * jitter depth moves. Re-timing the video for that would be a visible
         * hitch in exchange for a difference below anyone's perception.
         */
        const val SYNC_HYSTERESIS_MS = 25
    }
}
