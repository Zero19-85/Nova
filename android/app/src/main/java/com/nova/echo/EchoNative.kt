package com.nova.echo

import java.nio.ByteBuffer

/**
 * The Rust bridge.
 *
 * Every declaration here must match a `Java_com_nova_echo_EchoNative_*` symbol
 * in `echo-android/src/lib.rs` exactly — package, object name, method name, and
 * signature. A mismatch is an `UnsatisfiedLinkError` at call time rather than a
 * compile error, so this file is the one place in the app worth reading against
 * the Rust source line by line.
 *
 * ## Threading
 *
 * [nativePollEvent] and [nativeFillBuffer] block until their timeout expires, so
 * neither may be called from the main thread. [nativePair] and [nativeConnect]
 * return immediately; everything they do afterwards is reported through events.
 */
object EchoNative {
    init { System.loadLibrary("echo") }

    /** Bytes were written. */
    const val FILL_OK = 0
    /** No frame arrived in time. Normal — a feeder that is keeping up sees this. */
    const val FILL_TIMEOUT = -1
    /** Frame larger than the supplied buffer; `meta[0]` holds the size needed. */
    const val FILL_TOO_SMALL = -2
    /** The session ended. Stop feeding. */
    const val FILL_ENDED = -3
    /** Bad handle or unreadable arguments. */
    const val FILL_BAD_HANDLE = -4

    /** Process-wide setup: logging and the panic hook. Idempotent. */
    external fun nativeInit()

    /**
     * This device's certificate fingerprint, generating the RSA-2048 identity on
     * first call. [dir] must be app-private storage — it will hold a private key.
     */
    external fun nativeIdentityFingerprint(dir: String): String

    /** Begin LAN pairing. Returns a handle; poll it with [nativePollEvent]. */
    external fun nativePair(configJson: String): Long

    /** Begin a streaming session. Returns a handle; poll it with [nativePollEvent]. */
    external fun nativeConnect(configJson: String): Long

    /** Next event as JSON, or null on timeout. Blocks up to [timeoutMs]. */
    external fun nativePollEvent(handle: Long, timeoutMs: Int): String?

    /**
     * Copy the next frame into [buf], which must be a **direct** ByteBuffer — in
     * practice a MediaCodec input buffer, which is what makes this a single copy
     * into memory the decoder already owns.
     *
     * Returns bytes written, or one of the `FILL_*` codes. On success [meta]
     * receives `[size, flags, ptsUs]`; flags bit 0 marks a keyframe.
     */
    external fun nativeFillBuffer(handle: Long, buf: ByteBuffer, meta: LongArray, timeoutMs: Int): Int

    // ── Input ───────────────────────────────────────────────────────────────
    // Decomposed integers rather than a byte array: these are sent at pointer
    // rates, and a JNI array allocation per twelve-byte packet would be pure
    // waste. Rust builds the GameStream packet, so no wire format lives in
    // Kotlin. Fire-and-forget — `false` means "not queued" (bad handle or a
    // no-op like a zero delta), never that the host refused it.

    /** Relative pointer motion — what games read. [a]=dx, [b]=dy. */
    const val INPUT_MOUSE_MOVE = 1
    /** Absolute position: [a]=x, [b]=y within a [c]×[d] reference surface. */
    const val INPUT_MOUSE_ABS = 2
    /** [a]=button 1..5 (left, middle, right, X1, X2), [b]=1 down / 0 up. */
    const val INPUT_MOUSE_BUTTON = 3
    /** [a]=amount in WHEEL_DELTA units (120 per notch). */
    const val INPUT_SCROLL = 4
    /** [a]=Windows virtual-key code, [b]=1 down / 0 up, [c]=modifier mask. */
    const val INPUT_KEY = 5
    /**
     * Release every modifier and mouse button. No arguments.
     *
     * Send whenever input stops mid-gesture — backgrounding, toggling input
     * off, ending a session — or the host is left with a key held down.
     */
    const val INPUT_RELEASE_ALL = 6

    external fun nativeSendInput(handle: Long, kind: Int, a: Int, b: Int, c: Int, d: Int): Boolean

    // ── Microphone ──────────────────────────────────────────────────────────

    /**
     * Post one encoded Opus packet to the host.
     *
     * [buf] must be a **direct** ByteBuffer — in practice a MediaCodec *output*
     * buffer, which makes this a single copy out of memory the encoder already
     * owns. [offset] and [size] come from the codec's `BufferInfo`, because an
     * output buffer is not required to start at position zero and the native
     * side reads from the buffer's base address.
     *
     * Returns false for a bad handle, a pairing handle, or a buffer that could
     * not be read — never that the host refused the audio. Fire-and-forget: a
     * capture thread that blocks on the network overruns AudioRecord's ring
     * buffer, and audio lost there cannot be recovered anywhere downstream.
     *
     * **Never pass a `BUFFER_FLAG_CODEC_CONFIG` buffer.** Those carry the Opus
     * identification header and pre-skip — container metadata, not audio — and
     * the host feeds what it receives straight to a decoder. [MicCapture] is
     * where that filter lives.
     */
    external fun nativeSendMic(handle: Long, buf: ByteBuffer, offset: Int, size: Int): Boolean

    // ── Downstream game audio ───────────────────────────────────────────────

    /** An encoded Opus packet is in the buffer; `meta[0]` holds its length. */
    const val AUDIO_PACKET = 1
    /** A packet was lost in flight. Conceal one frame. */
    const val AUDIO_CONCEAL = 0
    /** Nothing to play yet — the buffer is filling. Write one frame of silence. */
    const val AUDIO_SILENCE = -1
    /** Nothing to play and nothing expected; the track may pause. */
    const val AUDIO_IDLE = -2
    /** Bad handle, or the buffer could not be read. Stop. */
    const val AUDIO_BAD_HANDLE = -3
    /** Packet larger than the supplied buffer; `meta[0]` holds the size needed. */
    const val AUDIO_TOO_SMALL = -4

    /**
     * Take one playout step of downstream game audio.
     *
     * **Call this on the audio device's clock, once per 20 ms it needs** — the
     * jitter buffer schedules against the caller, so making the network the
     * clock instead is exactly how a jitter buffer stops being one. Never
     * blocks; a `AUDIO_SILENCE` return is a normal answer, not a failure.
     *
     * [buf] must be a **direct** ByteBuffer of at least 1275 bytes (the largest
     * packet Opus emits). Returns one of the `AUDIO_*` codes above.
     *
     * `AUDIO_CONCEAL` and `AUDIO_SILENCE` are rendered the same way today —
     * MediaCodec has no packet-loss-concealment entry point — but they are
     * reported apart so a lost packet never reads as a quiet host.
     */
    external fun nativePollAudio(handle: Long, buf: ByteBuffer, meta: LongArray): Int

    /**
     * Hold video frames [delayMs] past arrival so they land with the audio.
     *
     * The A/V sync engine's one control. It moves **video** because video is the
     * side with slack: audio latency is dominated by a device output stage that
     * cannot be bypassed, while video renders as soon as it decodes.
     *
     * Returns the value actually applied — it is clamped, so do not assume.
     * Zero restores undelayed rendering exactly.
     *
     * **Costs input latency.** A delayed frame is a delayed view of your own
     * mouse, so this belongs to watching, not playing.
     */
    external fun nativeSetVideoDelay(handle: Long, delayMs: Int): Int

    /** Queue and receive statistics as JSON. */
    external fun nativeStats(handle: Long): String

    /**
     * Hand the platform layer's input telemetry to the session, which folds it
     * into the report it sends the host. Rate and capture state exist only in
     * the View, and the host log is where they are actually useful.
     */
    external fun nativeReportUiState(peakRate: Int, peakSamples: Int, captureHeld: Boolean)

    /** End the session and free the handle. Wakes anything blocked. Idempotent. */
    external fun nativeClose(handle: Long)

    /**
     * Ask the host to end whatever session it is holding for this device, with
     * no handle and no session of our own.
     *
     * **Blocking** — one punch and one RPC — so call it off the main thread.
     * Returns the host's answer; throws if the host could not be reached.
     *
     * For a session that outlived the app: swiped away or network lost, so
     * `stop_session` was never sent and the host is holding the display for the
     * grace period. Deliberately NOT "connect and then stop": that raced the
     * session it had just started and needed several presses to land.
     */
    external fun nativeRelease(configJson: String): String
}
