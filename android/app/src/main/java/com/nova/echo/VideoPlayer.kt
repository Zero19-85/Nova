package com.nova.echo

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.util.Log
import android.view.Surface
import kotlin.concurrent.thread

/**
 * Feeds decrypted frames from Rust into MediaCodec, decoding straight onto a
 * Surface.
 *
 * ## Two threads, synchronous mode
 *
 * A feeder pulls frames from Rust and queues them; a renderer dequeues decoded
 * output and releases it to the Surface. Synchronous mode is the right fit here
 * precisely because [EchoNative.nativeFillBuffer] blocks: the feeder parks
 * *inside* Rust waiting for the next frame, which is the whole point of the pull
 * model. Async callback mode would invert that and buy nothing.
 *
 * ## Nothing is copied twice
 *
 * `getInputBuffer` hands back a direct ByteBuffer that the codec already owns.
 * That buffer goes straight down to Rust, which writes the reassembled frame
 * into it. There is no Java `byte[]` anywhere on the frame path, and no
 * per-frame allocation.
 *
 * ## No CSD to extract
 *
 * Nova inlines parameter sets on every IDR (`NV_ENC_PIC_FLAG_OUTPUT_SPSPPS`), so
 * the stream is self-describing Annex-B and `configure` needs no `csd-0`. That
 * is also why the keyframe gate in Rust matters: the first frame the decoder
 * sees must be the IDR that carries them.
 *
 * ## The Surface outlives nothing
 *
 * A `SurfaceView`'s Surface is destroyed and recreated constantly — backgrounding
 * the app, locking the screen, and moving the window to another display all do
 * it. A decoder configured once against the first Surface goes on writing into a
 * dead one, which looks exactly like a frozen picture with a perfectly healthy
 * network underneath (observed live 2026-08-15: the host logged a fine session
 * while the phone sat silent for two minutes). [setSurface] re-attaches, so the
 * Surface is treated as a thing that comes and goes rather than a constructor
 * argument.
 *
 * ## Why every codec call is wrapped
 *
 * Re-attaching was not enough, and the reason is worth keeping. Only
 * `dequeueOutputBuffer` used to be guarded; `releaseOutputBuffer` was not. When
 * the Surface is destroyed, the output buffers already in flight get rendered
 * into an abandoned surface, `releaseOutputBuffer` throws
 * `IllegalStateException`, and that exception propagated straight out of the
 * render loop — killing the thread with no [onError], no log the app could see,
 * and nothing to restart it. On resume `setOutputSurface` then succeeded and
 * cheerfully reported success while nothing was dequeuing output any more.
 *
 * The visible result was a black screen with a completely healthy network, and
 * the host log agreed: 60 fps going out, client sending pings, no errors on
 * either side (live 2026-08-19). The second-order effect completed the picture —
 * with nobody releasing output buffers the input buffers stopped recycling, so
 * the feeder span on `dequeueInputBuffer` returning −1 forever.
 *
 * Two rules came out of it, and both are load-bearing:
 *
 * 1. **Never render into a Surface that is gone.** [renderLoop] discards
 *    (`render = false`) whenever there is no live Surface, which removes the
 *    throw at its source instead of catching it afterwards. That turned out to
 *    be necessary and not sufficient — a codec whose Surface is destroyed stops
 *    cycling altogether, silently, so [detach] releases it rather than trying to
 *    keep it alive without anywhere to draw.
 * 2. **A decode thread must never die quietly.** Every codec call runs inside
 *    [guarded]; anything that escapes it stops the player and reports, so a
 *    failure becomes a visible error and a rebuild rather than a black screen.
 */
class VideoPlayer(
    private val handle: Long,
    surface: Surface,
    private val width: Int,
    private val height: Int,
    /**
     * The negotiated frame rate, passed to the decoder as `KEY_FRAME_RATE`.
     *
     * Not cosmetic. Without it the decoder is told a resolution and nothing
     * about cadence, so it cannot pick an operating point for the load it is
     * about to get, and cannot refuse a mode it has no chance of sustaining —
     * it simply falls behind. It is also what `areSizeAndRateSupported` needs
     * to answer honestly, which is how [findHardwareDecoder] reports whether
     * this device can really do 4K120 rather than guessing.
     */
    private val fps: Int,
    codec: String,
    private val onError: (String) -> Unit,
) {
    /** Guards [codecInstance] against a surface swap racing start/stop. */
    private val lock = Any()
    private var surface: Surface? = surface
    private val mime = when (codec.lowercase()) {
        "h264", "avc" -> MediaFormat.MIMETYPE_VIDEO_AVC
        "av1" -> MediaFormat.MIMETYPE_VIDEO_AV1
        else -> MediaFormat.MIMETYPE_VIDEO_HEVC
    }

    private var codecInstance: MediaCodec? = null
    private var feeder: Thread? = null
    private var renderer: Thread? = null
    @Volatile private var running = false
    /** Reads and discards frames while there is no decoder. See [startDrain]. */
    private var drain: Thread? = null
    @Volatile private var draining = false
    /**
     * Set once a loop has failed, so the two threads do not each report the same
     * underlying fault and so [stop] knows the codec is already unusable.
     */
    @Volatile private var failed = false
    /**
     * Rebuilds spent on this player, so a decoder that fails immediately on
     * every attempt reports instead of thrashing. Reset by a clean [start].
     */
    private var restarts = 0

    fun start() {
        val target = synchronized(lock) { surface }
        if (target == null) {
            onError("no surface to decode onto")
            return
        }
        val c = createCodec(target) ?: return

        synchronized(lock) { codecInstance = c }
        running = true
        failed = false
        restarts = 0
        feeder = thread(name = "echo-feeder") { feedLoop(c) }
        renderer = thread(name = "echo-render") { renderLoop(c) }
        Log.i(TAG, "decoder started: ${c.name} $mime ${width}x$height @${fps}fps")
    }

    private fun createCodec(target: Surface): MediaCodec? {
        val format = MediaFormat.createVideoFormat(mime, width, height)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        }
        // Vendor key for devices predating KEY_LOW_LATENCY. Unknown keys are
        // ignored, so setting both is safe and covers far more hardware.
        format.setInteger("vdec-lowlatency", 1)

        format.setInteger(MediaFormat.KEY_FRAME_RATE, fps)

        // Ask for a HARDWARE decoder by name rather than taking whatever
        // `createDecoderByType` hands back.
        //
        // `createDecoderByType` returns the first entry the platform lists for
        // the MIME type, and that is not required to be the hardware one. For
        // `video/av01` in particular a device can carry Google's software
        // decoder (`c2.android.av1.decoder`) ahead of the SoC block it also
        // has. HEVC never showed this because its hardware decoder is what
        // enumerates first, which is exactly why AV1 and HEVC diverged at
        // IDENTICAL pixel rates: 1080p120 HEVC ran clean at 0.01 keyframe
        // requests/sec while 1080p120 AV1 flooded at 0.43-1.07/sec, on a link
        // with 3ms RTT and zero host-side packet loss (measured 2026-08-22).
        // Software AV1 cannot sustain those rates; the frame queue backs up,
        // and a client that has stopped consuming asks for keyframes forever.
        //
        // Falls back to the platform default when no hardware decoder exists or
        // the one we picked refuses the format — a software picture beats none.
        // A hardware decoder that will not claim this size and rate is REFUSED,
        // not forced — and there is no software fallback for a mode nothing
        // supports, because that fallback is a slower way to reach the same
        // crash.
        //
        // Confirmed on a Pixel 9 Pro XL (Tensor G4), 2026-08-22: the hardware
        // AV1 block reports areSizeAndRateSupported(3840, 2160, 120.0) == false.
        // Driving it anyway backed the pipeline up until MediaCodec threw
        // "Pending dequeue output buffer request cancelled" in a loop. That is
        // the silicon's pixel-clock limit, and the decoder said so before a
        // single frame was queued — the only mistake was not listening.
        //
        // So: refuse here, cleanly, with a message naming the mode. Nothing is
        // started, so nothing can wedge. [Support.bestSupported] is what stops
        // a session from being requested in this state at all; this is the
        // backstop for a host that grants a mode we did not ask for.
        val hardware = findHardwareDecoder()
        if (hardware == null) {
            if (!Support.anyDecoderClaims(mime, width, height, fps)) {
                Log.e(TAG, "no decoder on this device claims $mime ${width}x${height}@${fps}fps — refusing")
                onError(Support.unsupportedMessage(mime, width, height, fps))
                return null
            }
            Log.w(TAG, "no HARDWARE decoder for $mime ${width}x${height}@${fps}fps, " +
                       "but a software one claims it — falling back to the platform default")
        }
        val codec = hardware?.let { openCodec(it, format, target) }
            ?: openCodec(null, format, target)
        if (codec == null) {
            onError("decoder for $mime at ${width}x${height}@${fps}fps: no usable decoder")
        }
        return codec
    }

    /**
     * Open one decoder, by name or by MIME, returning null if it refuses.
     *
     * A codec that throws from `configure`/`start` still holds resources, so it
     * is released here before the caller tries the next candidate.
     */
    private fun openCodec(name: String?, format: MediaFormat, target: Surface): MediaCodec? {
        var c: MediaCodec? = null
        return try {
            c = if (name != null) MediaCodec.createByCodecName(name)
                else MediaCodec.createDecoderByType(mime)
            c.configure(format, target, null, 0)
            c.start()
            c
        } catch (e: Exception) {
            runCatching { c?.release() }
            Log.w(TAG, "decoder ${name ?: "(platform default)"} refused " +
                       "$mime ${width}x${height}@${fps}fps: ${e.message}")
            null
        }
    }

    /**
     * The name of a hardware decoder for [mime], or null if the device has none.
     *
     * Every candidate is logged with what it claims, because that enumeration is
     * the whole diagnostic: it says in one line whether this device actually has
     * a hardware AV1 block, and whether that block will admit to handling the
     * mode being asked of it. `areSizeAndRateSupported` is the honest answer to
     * "can this phone do 4K120" — better to read it from the decoder than to
     * infer it from a corrupted picture.
     *
     * Where several hardware decoders qualify, one that claims this exact
     * size-and-rate wins over one that does not.
     */
    private fun findHardwareDecoder(): String? = runCatching {
        val candidates = Support.decoders(mime)
        candidates.forEach { info ->
            Log.i(TAG, "  decoder candidate ${info.name}: hardware=${Support.hw(info)}, " +
                       "claims ${width}x${height}@${fps}fps=" +
                       "${Support.claims(info, mime, width, height, fps)}")
        }
        // FILTER, not sort. An earlier version ordered candidates by whether
        // they claimed the mode and took the best one, which still selected a
        // decoder that had already answered "no" when it was the only hardware
        // block present. `areSizeAndRateSupported` is a hard gate here.
        candidates.filter { Support.hw(it) && Support.claims(it, mime, width, height, fps) }
            .firstOrNull()
            ?.name
    }.getOrNull()

    /**
     * Point the decoder at a new Surface, or at none while one is unavailable.
     *
     * `setOutputSurface` swaps the output of a *running* codec without dropping
     * the reference chain — which matters on an infinite-GOP stream, where
     * tearing the decoder down and rebuilding it would leave the picture frozen
     * until the host happened to send another IDR.
     *
     * A null Surface means the SurfaceView's Surface has been destroyed, and it
     * releases the decoder outright — see [detach] for why discarding output was
     * not enough.
     *
     * **A new Surface always asks the host for a keyframe.** The chain that
     * `setOutputSurface` preserves is a decoder-implementation promise rather
     * than a guarantee, and Nova's GOP is infinite — so if it did break, nothing
     * would ever repair it on its own. One IDR costs a frame; the alternative is
     * a picture that never comes back.
     */
    fun setSurface(next: Surface?) {
        if (next == null) {
            detach()
            return
        }
        val c = synchronized(lock) {
            surface = next
            codecInstance
        }
        // Nothing to swap: either the decoder was released when the Surface went
        // away, or one has never been built. Either way the Surface that just
        // arrived is the one to build against.
        if (c == null) {
            rebuild("a surface arrived with no decoder attached")
            return
        }
        try {
            c.setOutputSurface(next)
            Log.i(TAG, "decoder re-attached to a new surface")
        } catch (e: Exception) {
            // Some decoders refuse a swap. Rebuild rather than report and stop:
            // a refused swap with a live Surface in hand is recoverable, and the
            // user-visible alternative is the black screen this class exists to
            // prevent.
            Log.w(TAG, "surface swap refused (${e.message}) — rebuilding the decoder")
            rebuild("the decoder refused a surface swap: ${e.message}")
            return
        }
        requestKeyframe()
    }

    /**
     * The Surface is gone. Release the decoder and keep draining the queue.
     *
     * ## Discarding output was not enough, and this is the evidence
     *
     * [renderLoop] already declines to render into a dead Surface, which was
     * supposed to be sufficient: keep the codec cycling, throw the pictures
     * away, swap a new Surface in on resume. It is not sufficient, because a
     * MediaCodec configured for a Surface allocates its OUTPUT buffers from that
     * Surface's BufferQueue. When the SurfaceView tears the Surface down the
     * queue is abandoned underneath the codec (`About to force-disconnect
     * API_MEDIA` in the platform log), and the codec simply stops: it cannot get
     * an output buffer, so it holds the ones it has, so it never recycles an
     * input buffer either. `dequeueOutputBuffer` returns TRY_AGAIN forever and
     * `dequeueInputBuffer` returns −1 forever. **Nothing throws.** Both loops
     * spin, perfectly alive, consuming nothing.
     *
     * That is why none of the failure reporting fired. Live 2026-08-19: the app
     * was backgrounded 28 s into session 20 and the host then logged **198
     * keyframe requests over the next 59 seconds** with the tunnel healthy, RTT
     * in single-digit milliseconds and 60 fps going out the whole time — the
     * client's frame queue overflowing on every push, which under Nova's
     * infinite GOP is the only repair it knows how to ask for. On resume
     * `setOutputSurface` reported success onto a codec that was already wedged,
     * so the picture never came back.
     *
     * A released codec cannot wedge. The rebuild costs the reference chain,
     * which is exactly what [setSurface]'s swap exists to preserve — but a swap
     * is only worth preserving when there is still a working codec to swap.
     *
     * ## Why the queue is still drained
     *
     * The session stays up while the app is backgrounded, so frames keep
     * arriving with nothing to consume them. Left alone the queue overflows
     * every 16 ms and asks the host for a keyframe each time, which is both the
     * flood above and the most expensive thing the host can be asked to encode.
     * A discarding reader costs one memcpy-free pop per frame and turns that
     * back into ordinary traffic.
     *
     * Suppressing the traffic entirely is the host's job, not this class's — the
     * `PauseEncode` plumbing exists for it and is currently switched off.
     *
     * ## Runs synchronously on the main thread, deliberately
     *
     * `surfaceDestroyed` promises the framework that nothing will touch the
     * Surface once it returns, so the codec has to be released before then. The
     * loops exit within one dequeue timeout and the feeder within one fill
     * timeout, so the wait is bounded at a few hundred milliseconds.
     */
    private fun detach() {
        val had = synchronized(lock) {
            surface = null
            codecInstance
        }
        if (had == null) return
        Log.i(TAG, "surface destroyed — releasing the decoder until one returns")
        stopThreadsAndCodec()
        startDrain()
    }

    /**
     * Keep popping frames while there is no decoder, and throw them away.
     *
     * The scratch buffer is deliberately far too small for a frame: Rust pops
     * the frame off the queue BEFORE it checks whether it fits, so `TOO_SMALL`
     * discards it exactly as intended. Popping is the entire point, and a buffer
     * sized for real frames would only make the discard cost a copy.
     */
    private fun startDrain() {
        if (draining) return
        draining = true
        drain = thread(name = "echo-drain") {
            val scratch = java.nio.ByteBuffer.allocateDirect(DRAIN_SCRATCH_BYTES)
            val meta = LongArray(3)
            while (draining) {
                scratch.clear()
                when (EchoNative.nativeFillBuffer(handle, scratch, meta, FILL_TIMEOUT_MS)) {
                    EchoNative.FILL_ENDED, EchoNative.FILL_BAD_HANDLE -> break
                }
            }
            Log.i(TAG, "drain stopped")
        }
    }

    private fun stopDrain() {
        draining = false
        drain?.takeIf { it != Thread.currentThread() }?.join(1_000)
        drain = null
    }

    /**
     * Ask the host for an IDR. Cheap, non-blocking, and safe to call from the UI
     * thread — the Rust side records a flag the receive loop reads on its next
     * turn (`FrameQueue::request_keyframe`).
     */
    private fun requestKeyframe() {
        if (!EchoNative.nativeRequestIdr(handle)) {
            // Only interesting during teardown, when the handle is being zeroed
            // underneath the Surface callbacks. Not an error.
            Log.d(TAG, "keyframe request skipped — no live session")
        }
    }

    /**
     * Run one codec call, converting any failure into a reported, deliberate
     * stop instead of an uncaught exception on a decode thread.
     *
     * Returns `null` when the call failed, which every caller treats as "leave
     * the loop". `IllegalStateException` is the expected one (a codec in the
     * error state, or a Surface that went away mid-call); the broader catch is
     * because a dead decode thread is a black screen either way, and the one
     * thing that must not happen is silence.
     */
    private inline fun <T> guarded(what: String, body: () -> T): T? =
        try {
            body()
        } catch (e: Exception) {
            fail("$what: ${e.message}")
            null
        }

    /**
     * First failure wins; the second thread to notice stays quiet.
     *
     * **Reporting is not enough, and that was a real gap.** Making the decode
     * threads fail loudly instead of dying silently was the first half of the
     * job; this is the second. A player whose loops have both exited is a
     * player that never consumes another frame — and on the client that shows
     * up as the frame queue overflowing forever, which under Nova's infinite
     * GOP means asking the host for a keyframe roughly every few seconds, for
     * as long as the session lasts. Live 2026-08-19: 60 keyframe requests over
     * 226 s with the stream flowing at a perfect 60 fps and the round trip
     * under 10 ms, while the screen stayed black.
     *
     * So a failure now rebuilds, on a budget. The budget matters: a decoder
     * that throws immediately on every attempt would otherwise thrash forever,
     * and the honest answer in that case is to tell the user.
     *
     * The rebuild runs on its own thread because this is called FROM a decode
     * thread, and rebuilding joins those threads.
     */
    private fun fail(message: String) {
        if (failed) return
        failed = true
        running = false
        Log.e(TAG, "decoder failed — $message")

        val live = synchronized(lock) { surface }?.isValid == true
        if (!live || restarts >= MAX_RESTARTS) {
            // Nothing to rebuild onto, or the budget is gone. Either way the
            // user needs to know rather than watch a black screen.
            onError(message)
            return
        }
        restarts++
        thread(name = "echo-recover") {
            // A moment's pause: the usual cause is a Surface that has just
            // gone away, and rebuilding into the same instant tends to hit the
            // same fault. Cheap insurance on a path that only runs on failure.
            Thread.sleep(RESTART_DELAY_MS)
            rebuild("decoder failed ($restarts/$MAX_RESTARTS): $message")
        }
    }

    /**
     * Tear the decoder down and build a new one against the current Surface.
     *
     * Used where the codec is known to be unusable but the session is not: a
     * refused surface swap is the case that matters. Rebuilding costs the
     * reference chain, which is exactly why [setSurface] tries the swap first
     * and why the rebuild ends with a keyframe request.
     */
    private fun rebuild(why: String) {
        val target = synchronized(lock) { surface }
        if (target == null) {
            // Nothing to rebuild onto. The next surfaceCreated will start us,
            // and until then the queue still needs a reader.
            stopThreadsAndCodec()
            startDrain()
            return
        }
        stopThreadsAndCodec()
        stopDrain()
        val c = createCodec(target) ?: run {
            onError("$why, and rebuilding it failed")
            return
        }
        synchronized(lock) { codecInstance = c }
        running = true
        failed = false
        feeder = thread(name = "echo-feeder") { feedLoop(c) }
        renderer = thread(name = "echo-render") { renderLoop(c) }
        Log.i(TAG, "decoder rebuilt after: $why")
        // A fresh decoder holds no reference frames at all, so this is not
        // belt-and-braces here — nothing it receives is decodable until an IDR.
        requestKeyframe()
    }

    private fun feedLoop(c: MediaCodec) {
        val meta = LongArray(3)
        while (running) {
            val index = guarded("decoder input failed") {
                c.dequeueInputBuffer(DEQUEUE_TIMEOUT_US)
            } ?: return
            if (index < 0) continue

            // Not routed through `guarded`: this call legitimately returns null,
            // which `guarded` would make indistinguishable from a throw.
            val buffer = try {
                c.getInputBuffer(index)
            } catch (e: Exception) {
                fail("decoder input buffer unavailable: ${e.message}")
                return
            }
            if (buffer == null) {
                guarded("releasing an unusable input buffer") {
                    c.queueInputBuffer(index, 0, 0, 0, 0)
                } ?: return
                continue
            }
            buffer.clear()

            when (val written = EchoNative.nativeFillBuffer(handle, buffer, meta, FILL_TIMEOUT_MS)) {
                EchoNative.FILL_ENDED -> {
                    guarded("signalling end of stream") {
                        c.queueInputBuffer(index, 0, 0, 0, MediaCodec.BUFFER_FLAG_END_OF_STREAM)
                    }
                    return
                }
                EchoNative.FILL_TIMEOUT -> {
                    // MediaCodec has no way to hand an input buffer back
                    // unused, so an empty queue is the only way to release it.
                    // Harmless, and only reached when the stream has stalled.
                    guarded("releasing an idle input buffer") {
                        c.queueInputBuffer(index, 0, 0, 0, 0)
                    } ?: return
                }
                EchoNative.FILL_TOO_SMALL -> {
                    guarded("releasing an oversized input buffer") {
                        c.queueInputBuffer(index, 0, 0, 0, 0)
                    }
                    fail("frame of ${meta[0]} bytes exceeds the decoder's input buffer — " +
                            "the stream's geometry disagrees with how the codec was configured")
                    return
                }
                EchoNative.FILL_BAD_HANDLE -> {
                    guarded("releasing an input buffer after the session ended") {
                        c.queueInputBuffer(index, 0, 0, 0, 0)
                    }
                    return
                }
                else -> {
                    val flags =
                        if (meta[1] and 1L != 0L) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
                    guarded("queueing a frame") {
                        c.queueInputBuffer(index, 0, written, meta[2], flags)
                    } ?: return
                }
            }
        }
    }

    private fun renderLoop(c: MediaCodec) {
        val info = MediaCodec.BufferInfo()
        while (running) {
            val index = guarded("decoder output failed") {
                c.dequeueOutputBuffer(info, DEQUEUE_TIMEOUT_US)
            } ?: return
            when {
                index >= 0 -> {
                    // THE rule this loop exists to keep: never render into a
                    // Surface that is gone. `releaseOutputBuffer(_, true)` on an
                    // abandoned Surface throws, and that throw used to kill this
                    // thread outright. Discarding costs one frame; the frames
                    // are 16 ms apart and nobody is looking at the screen while
                    // the app is backgrounded.
                    val live = synchronized(lock) { surface }?.isValid == true
                    guarded("releasing an output buffer") {
                        c.releaseOutputBuffer(index, live)
                    } ?: return
                    if (info.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) return
                }
                index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                    Log.i(TAG, "decoder format: ${c.outputFormat}")
                }
            }
        }
    }

    /** Stop the threads and release the codec, leaving [surface] untouched. */
    private fun stopThreadsAndCodec() {
        running = false
        // Not joined from the decode threads themselves — `rebuild` is only ever
        // reached from `setSurface`, which runs on the main thread.
        feeder?.takeIf { it != Thread.currentThread() }?.join(1_000)
        renderer?.takeIf { it != Thread.currentThread() }?.join(1_000)
        feeder = null
        renderer = null
        val c = synchronized(lock) { val current = codecInstance; codecInstance = null; current }
        c?.let {
            runCatching { it.stop() }
            runCatching { it.release() }
        }
    }

    fun stop() {
        stopThreadsAndCodec()
        stopDrain()
    }

    /**
     * What this device can actually decode, asked before a session is requested.
     *
     * ## Why this is the renegotiation
     *
     * Echo has no in-band "that format is unsupported" message, and does not
     * need one: the CLIENT chooses the mode. `start_session` carries res, fps
     * and codec, and the host grants against them — so the honest place to
     * refuse an impossible mode is BEFORE the ask, not after the grant. A
     * round trip that exists only to be rejected is a round trip that did not
     * need to happen, and it costs a display activation on the host.
     *
     * [bestSupported] is therefore called on the way into `connect`, and the
     * check inside [createCodec] is the backstop for the one case it cannot
     * cover: a host that grants something other than what was asked for.
     */
    object Support {
        /** MIME for a codec name as it travels on the wire. */
        fun mimeOf(codec: String): String = when (codec.lowercase()) {
            "h264", "avc" -> MediaFormat.MIMETYPE_VIDEO_AVC
            "av1" -> MediaFormat.MIMETYPE_VIDEO_AV1
            else -> MediaFormat.MIMETYPE_VIDEO_HEVC
        }

        /** Whether any decoder — hardware or software — claims this exact mode. */
        fun anyDecoderClaims(mime: String, width: Int, height: Int, fps: Int): Boolean =
            decoders(mime).any { claims(it, mime, width, height, fps) }

        /** Whether a HARDWARE decoder claims this exact mode. */
        fun hardwareClaims(mime: String, width: Int, height: Int, fps: Int): Boolean =
            decoders(mime).any { hw(it) && claims(it, mime, width, height, fps) }

        /**
         * The closest mode to the one requested that this device's hardware can
         * actually decode, or null if nothing on the ladder works.
         *
         * The ladder drops frame rate before it changes codec, because fps is
         * the cheaper concession: 4K60 AV1 keeps the codec the operator chose,
         * while falling to HEVC discards it. Resolution is never lowered here —
         * it is the one parameter the user picked for how the picture LOOKS,
         * and silently halving it would be a worse surprise than a lower
         * cadence.
         */
        fun bestSupported(codec: String, width: Int, height: Int, fps: Int): Mode? {
            val ladder = buildList {
                add(Mode(codec, fps))
                if (fps > 60) add(Mode(codec, 60))
                if (!codec.equals("hevc", true)) {
                    add(Mode("hevc", fps))
                    if (fps > 60) add(Mode("hevc", 60))
                }
            }
            return ladder.firstOrNull { hardwareClaims(mimeOf(it.codec), width, height, it.fps) }
        }

        /** One line naming the mode and what the device said about it. */
        fun unsupportedMessage(mime: String, width: Int, height: Int, fps: Int): String =
            "this device has no decoder for $mime at ${width}x${height}@${fps}fps"

        fun decoders(mime: String): List<MediaCodecInfo> = runCatching {
            MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.filter { info ->
                !info.isEncoder && info.supportedTypes.any { it.equals(mime, ignoreCase = true) }
            }
        }.getOrDefault(emptyList())

        fun hw(info: MediaCodecInfo): Boolean =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                info.isHardwareAccelerated
            } else {
                !info.name.startsWith("c2.android.", ignoreCase = true) &&
                    !info.name.startsWith("OMX.google.", ignoreCase = true)
            }

        fun claims(info: MediaCodecInfo, mime: String, w: Int, h: Int, fps: Int): Boolean =
            runCatching {
                info.getCapabilitiesForType(mime)
                    .videoCapabilities
                    .areSizeAndRateSupported(w, h, fps.toDouble())
            }.getOrDefault(false)
    }

    /** A decodable combination of codec and frame rate. */
    data class Mode(val codec: String, val fps: Int)

    private companion object {
        const val TAG = "EchoVideo"
        const val DEQUEUE_TIMEOUT_US = 10_000L
        // Long enough that a healthy 60 fps stream never times out, short enough
        // that a dead stream is noticed promptly.
        const val FILL_TIMEOUT_MS = 250
        /** Too small for any frame on purpose — see [startDrain]. */
        const val DRAIN_SCRATCH_BYTES = 64
        /**
         * Rebuild attempts before giving up and reporting.
         *
         * Three covers the transient causes (a Surface swapped mid-call, a
         * codec upset by one bad buffer) without letting a decoder that is
         * genuinely unusable on this device spin forever.
         */
        const val MAX_RESTARTS = 3
        const val RESTART_DELAY_MS = 250L
    }
}
