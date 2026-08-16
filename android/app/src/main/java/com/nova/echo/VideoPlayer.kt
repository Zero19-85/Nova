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

    fun start() {
        val format = MediaFormat.createVideoFormat(mime, width, height)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        }
        // Vendor key for devices predating KEY_LOW_LATENCY. Unknown keys are
        // ignored, so setting both is safe and covers far more hardware.
        format.setInteger("vdec-lowlatency", 1)

        val target = synchronized(lock) { surface }
        if (target == null) {
            onError("no surface to decode onto")
            return
        }

        val c = try {
            MediaCodec.createDecoderByType(mime).also {
                it.configure(format, target, null, 0)
                it.start()
            }
        } catch (e: Exception) {
            onError("decoder for $mime at ${width}x$height: ${e.message}")
            return
        }

        synchronized(lock) { codecInstance = c }
        running = true
        feeder = thread(name = "echo-feeder") { feedLoop(c) }
        renderer = thread(name = "echo-render") { renderLoop(c) }
        Log.i(TAG, "decoder started: $mime ${width}x$height")
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
     * way to detach an output surface, and passing null throws. Frames keep
     * decoding into the old (dead) Surface until a live one arrives, which is
     * wasteful for a moment and harmless — the alternative, stopping the codec,
     * is what caused the freeze in the first place.
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
            // Some decoders refuse a swap. Report rather than pretend: the
            // caller can restart the session, and a silent failure here is
            // indistinguishable from the bug this method exists to fix.
            onError("could not re-attach the decoder to the new surface: ${e.message}")
        }
    }

    private fun feedLoop(c: MediaCodec) {
        val meta = LongArray(3)
        while (running) {
            val index = try {
                c.dequeueInputBuffer(DEQUEUE_TIMEOUT_US)
            } catch (e: IllegalStateException) {
                if (running) onError("decoder input failed: ${e.message}")
                return
            }
            if (index < 0) continue

            val buffer = c.getInputBuffer(index)
            if (buffer == null) {
                c.queueInputBuffer(index, 0, 0, 0, 0)
                continue
            }
            buffer.clear()

            when (val written = EchoNative.nativeFillBuffer(handle, buffer, meta, FILL_TIMEOUT_MS)) {
                EchoNative.FILL_ENDED -> {
                    c.queueInputBuffer(index, 0, 0, 0, MediaCodec.BUFFER_FLAG_END_OF_STREAM)
                    return
                }
                EchoNative.FILL_TIMEOUT -> {
                    // MediaCodec has no way to hand an input buffer back
                    // unused, so an empty queue is the only way to release it.
                    // Harmless, and only reached when the stream has stalled.
                    c.queueInputBuffer(index, 0, 0, 0, 0)
                }
                EchoNative.FILL_TOO_SMALL -> {
                    c.queueInputBuffer(index, 0, 0, 0, 0)
                    onError("frame of ${meta[0]} bytes exceeds the decoder's input buffer — " +
                            "the stream's geometry disagrees with how the codec was configured")
                    return
                }
                EchoNative.FILL_BAD_HANDLE -> {
                    c.queueInputBuffer(index, 0, 0, 0, 0)
                    return
                }
                else -> {
                    val flags =
                        if (meta[1] and 1L != 0L) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
                    c.queueInputBuffer(index, 0, written, meta[2], flags)
                }
            }
        }
    }

    private fun renderLoop(c: MediaCodec) {
        val info = MediaCodec.BufferInfo()
        while (running) {
            val index = try {
                c.dequeueOutputBuffer(info, DEQUEUE_TIMEOUT_US)
            } catch (e: IllegalStateException) {
                if (running) onError("decoder output failed: ${e.message}")
                return
            }
            when {
                index >= 0 -> {
                    // `true` renders to the Surface. Timing is deliberately not
                    // managed here: the stream is live, so the correct moment to
                    // show a frame is as soon as it exists.
                    c.releaseOutputBuffer(index, true)
                    if (info.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) return
                }
                index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                    Log.i(TAG, "decoder format: ${c.outputFormat}")
                }
            }
        }
    }

    fun stop() {
        running = false
        feeder?.join(1_000)
        renderer?.join(1_000)
        val c = synchronized(lock) { val current = codecInstance; codecInstance = null; current }
        c?.let {
            runCatching { it.stop() }
            runCatching { it.release() }
        }
    }

    private companion object {
        const val TAG = "EchoVideo"
        const val DEQUEUE_TIMEOUT_US = 10_000L
        // Long enough that a healthy 60 fps stream never times out, short enough
        // that a dead stream is noticed promptly.
        const val FILL_TIMEOUT_MS = 250
    }
}
