package com.nova.echo

import android.media.MediaCodec
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
 *    throw at its source instead of catching it afterwards.
 * 2. **A decode thread must never die quietly.** Every codec call runs inside
 *    [guarded]; anything that escapes it stops the player and reports, so a
 *    failure becomes a visible error and a rebuild rather than a black screen.
 */
class VideoPlayer(
    private val handle: Long,
    surface: Surface,
    private val width: Int,
    private val height: Int,
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
    /**
     * Set once a loop has failed, so the two threads do not each report the same
     * underlying fault and so [stop] knows the codec is already unusable.
     */
    @Volatile private var failed = false

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
        feeder = thread(name = "echo-feeder") { feedLoop(c) }
        renderer = thread(name = "echo-render") { renderLoop(c) }
        Log.i(TAG, "decoder started: $mime ${width}x$height")
    }

    private fun createCodec(target: Surface): MediaCodec? {
        val format = MediaFormat.createVideoFormat(mime, width, height)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        }
        // Vendor key for devices predating KEY_LOW_LATENCY. Unknown keys are
        // ignored, so setting both is safe and covers far more hardware.
        format.setInteger("vdec-lowlatency", 1)

        return try {
            MediaCodec.createDecoderByType(mime).also {
                it.configure(format, target, null, 0)
                it.start()
            }
        } catch (e: Exception) {
            onError("decoder for $mime at ${width}x$height: ${e.message}")
            null
        }
    }

    /**
     * Point the decoder at a new Surface, or at none while one is unavailable.
     *
     * `setOutputSurface` swaps the output of a *running* codec without dropping
     * the reference chain — which matters on an infinite-GOP stream, where
     * tearing the decoder down and rebuilding it would leave the picture frozen
     * until the host happened to send another IDR.
     *
     * A null Surface is recorded but NOT pushed to the codec: MediaCodec has no
     * way to detach an output surface, and passing null throws. [renderLoop]
     * consults the recorded value and discards output while it is null, so
     * nothing is ever rendered into a dead Surface.
     *
     * **A new Surface always asks the host for a keyframe.** The chain that
     * `setOutputSurface` preserves is a decoder-implementation promise rather
     * than a guarantee, and Nova's GOP is infinite — so if it did break, nothing
     * would ever repair it on its own. One IDR costs a frame; the alternative is
     * a picture that never comes back.
     */
    fun setSurface(next: Surface?) {
        val c = synchronized(lock) {
            surface = next
            codecInstance
        }
        if (next == null || c == null) return
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

    /** First failure wins; the second thread to notice stays quiet. */
    private fun fail(message: String) {
        if (failed) return
        failed = true
        running = false
        Log.e(TAG, "decoder failed — $message")
        onError(message)
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
            // Nothing to rebuild onto. The next surfaceCreated will start us.
            stopThreadsAndCodec()
            return
        }
        stopThreadsAndCodec()
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
    }

    private companion object {
        const val TAG = "EchoVideo"
        const val DEQUEUE_TIMEOUT_US = 10_000L
        // Long enough that a healthy 60 fps stream never times out, short enough
        // that a dead stream is noticed promptly.
        const val FILL_TIMEOUT_MS = 250
    }
}
