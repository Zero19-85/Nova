package com.nova.echo

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaCodec
import android.media.MediaCodecList
import android.media.MediaFormat
import android.media.MediaRecorder
import android.media.audiofx.AcousticEchoCanceler
import android.media.audiofx.NoiseSuppressor
import android.os.Process
import android.util.Log
import kotlin.concurrent.thread

/**
 * Captures the microphone, encodes it as Opus, and hands each packet to Rust.
 *
 * One thread does all three steps in sequence — read PCM, feed the encoder,
 * drain the encoder — because they are strictly serial at 20 ms cadence and
 * splitting them across threads would add a hand-off queue whose only effect is
 * latency. The thread blocks in [AudioRecord.read], which is the clock: the
 * audio hardware paces the loop, so nothing here needs a timer.
 *
 * ## Why Opus is encoded here rather than in Rust
 *
 * The obvious symmetry would put the encoder next to the transport in
 * `nova-core`, since the host already links libopus for its own audio. But
 * `audiopus` builds libopus from C through cmake, and cross-compiling that under
 * `cargo-ndk` is the same class of fight the workspace manifest already
 * documents about `aws-lc-rs` — for a codec Android has had in `MediaCodec`
 * since API 29. Encoding here keeps `libecho.so` free of a new native
 * dependency, which is a stated goal of this project, and costs nothing: the
 * packets cross the JNI boundary either way.
 *
 * ## The trap this class exists to avoid
 *
 * MediaCodec emits the Opus identification header, pre-skip, and seek pre-roll
 * as [MediaCodec.BUFFER_FLAG_CODEC_CONFIG] buffers before any audio. Those are
 * **container metadata, not audio**. The host feeds what it receives straight
 * to an Opus decoder, so passing one along would put non-packet bytes into the
 * stream and produce silence or a decoder error — with nothing in any log to say
 * why.
 *
 * This is not a hypothetical for this codebase. Nova's AV1 support lost days to
 * exactly this shape of bug: the NVIDIA SDK's encoder wrapper prepended an IVF
 * file header and a 12-byte frame header to every AV1 frame, Moonlight handed
 * the payload to its decoder unchanged, and every frame was undecodable while
 * H.264 and HEVC — which the wrapper left alone — worked perfectly. The lesson
 * recorded then was to check what a codec wraps its output in *before* trusting
 * the bytes, so it is checked here, in the one place that can see the flag.
 *
 * ## Availability
 *
 * The app's `minSdk` is 26, but Android's Opus **encoder** arrived in API 29.
 * On an older device [findOpusEncoder] returns null and this reports a clear
 * refusal rather than throwing — the stream keeps working without a microphone,
 * which is the correct degradation.
 */
class MicCapture(
    private val context: Context,
    private val handle: Long,
    private val onError: (String) -> Unit,
) {

    @Volatile private var running = false
    private var worker: Thread? = null

    /** True once [start] has a capture loop actually running. */
    val isRunning: Boolean get() = running

    /**
     * Begin capturing. Returns false if the microphone cannot be opened, having
     * already reported why through [onError].
     *
     * Safe to call when already running: it does nothing and returns true.
     */
    fun start(): Boolean {
        if (running) return true

        if (context.checkSelfPermission(Manifest.permission.RECORD_AUDIO)
            != PackageManager.PERMISSION_GRANTED
        ) {
            onError("Echo has no microphone permission")
            return false
        }
        if (findOpusEncoder() == null) {
            // API 28 and below, or an OEM build with the encoder stripped.
            onError("this device has no Opus encoder — microphone needs Android 10 or newer")
            return false
        }

        running = true
        worker = thread(name = "echo-mic") { captureLoop() }
        return true
    }

    /**
     * Stop capturing and release the microphone.
     *
     * Releasing rather than pausing is deliberate: Android shows a microphone
     * indicator for as long as an [AudioRecord] is open, and a muted app that
     * kept the recorder alive would keep that indicator lit. A user who mutes
     * expects the light to go out, and it should be telling the truth.
     */
    fun stop() {
        running = false
        worker?.join(1_000)
        worker = null
    }

    private fun captureLoop() {
        // The audio hardware paces this loop and a late buffer is a dropout, so
        // it asks for the priority the platform reserves for exactly this.
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)

        var record: AudioRecord? = null
        var codec: MediaCodec? = null
        var aec: AcousticEchoCanceler? = null
        var ns: NoiseSuppressor? = null

        try {
            // A larger hardware buffer than one frame, so a scheduling hiccup
            // costs latency rather than lost audio — the recorder keeps filling
            // while this thread is away, and the loop catches up.
            val minBuffer = AudioRecord.getMinBufferSize(SAMPLE_RATE, CHANNEL_MASK, ENCODING)
            if (minBuffer <= 0) {
                fail("this device cannot record 48 kHz mono")
                return
            }
            val bufferBytes = maxOf(minBuffer, BYTES_PER_FRAME * 4)

            record = AudioRecord(
                // VOICE_COMMUNICATION rather than MIC: it is the source that
                // asks the platform for echo cancellation and noise
                // suppression, which matters here because the host's audio is
                // playing out of the same device that is listening.
                MediaRecorder.AudioSource.VOICE_COMMUNICATION,
                SAMPLE_RATE,
                CHANNEL_MASK,
                ENCODING,
                bufferBytes,
            )
            if (record.state != AudioRecord.STATE_INITIALIZED) {
                fail("could not open the microphone")
                return
            }

            // Belt and braces: VOICE_COMMUNICATION usually implies both of
            // these, but not on every OEM build, and they are free where the
            // platform has already applied them.
            aec = runCatching {
                if (AcousticEchoCanceler.isAvailable()) {
                    AcousticEchoCanceler.create(record.audioSessionId)?.apply { enabled = true }
                } else null
            }.getOrNull()
            ns = runCatching {
                if (NoiseSuppressor.isAvailable()) {
                    NoiseSuppressor.create(record.audioSessionId)?.apply { enabled = true }
                } else null
            }.getOrNull()

            codec = MediaCodec.createByCodecName(findOpusEncoder()!!)
            codec.configure(encoderFormat(), null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            codec.start()
            record.startRecording()
            Log.i(TAG, "microphone open: 48 kHz mono Opus at $BITRATE bps, ${FRAME_MS}ms frames")

            // Read into a reused array rather than straight into the codec's
            // input ByteBuffer. `AudioRecord.read(ByteBuffer, int)` returns 0
            // forever if the buffer is not direct, which would be a silent,
            // permanent failure with no exception to catch — and the copy it
            // saves is 1920 bytes fifty times a second. One reused array costs
            // no allocation per packet and cannot fail that way.
            val pcm = ByteArray(BYTES_PER_FRAME)
            val info = MediaCodec.BufferInfo()
            var samplesRead = 0L

            while (running) {
                val read = record.read(pcm, 0, BYTES_PER_FRAME)
                if (read < 0) {
                    fail("microphone read failed: $read")
                    return
                }
                if (read > 0) {
                    val index = codec.dequeueInputBuffer(DEQUEUE_TIMEOUT_US)
                    if (index >= 0) {
                        val input = codec.getInputBuffer(index)!!
                        input.clear()
                        input.put(pcm, 0, read)
                        // Derived from samples, never from a clock: MediaCodec
                        // requires a monotonic timestamp, and a wall clock would
                        // fold this thread's own scheduling jitter into it. The
                        // sample count is exact by construction.
                        val ptsUs = samplesRead * 1_000_000L / SAMPLE_RATE
                        codec.queueInputBuffer(index, 0, read, ptsUs, 0)
                        samplesRead += read / BYTES_PER_SAMPLE
                    }
                    // An input buffer that never arrives means the encoder is
                    // backed up; the PCM is dropped rather than queued, because
                    // stale audio delivered late is worse than a gap.
                }

                drainEncoder(codec, info)
            }
        } catch (e: Throwable) {
            fail("microphone failed: ${e.message ?: e.javaClass.simpleName}")
        } finally {
            running = false
            runCatching { record?.stop() }
            runCatching { record?.release() }
            runCatching { codec?.stop() }
            runCatching { codec?.release() }
            runCatching { aec?.release() }
            runCatching { ns?.release() }
            Log.i(TAG, "microphone closed")
        }
    }

    /** Hand every ready packet to Rust. Returns when the encoder has none left. */
    private fun drainEncoder(codec: MediaCodec, info: MediaCodec.BufferInfo) {
        while (true) {
            val index = codec.dequeueOutputBuffer(info, 0)
            if (index < 0) return // TRY_AGAIN_LATER, or a format change we ignore

            try {
                // ── The codec-config filter. See the class documentation. ──
                //
                // These carry the Opus identification header, pre-skip, and seek
                // pre-roll — container metadata that is not part of the audio
                // stream. The host decodes raw Opus packets, so sending one of
                // these would corrupt the stream with nothing to show why.
                val isConfig = info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0
                if (isConfig || info.size <= 0) {
                    if (isConfig) Log.i(TAG, "dropped ${info.size}-byte Opus codec config")
                    continue
                }

                val output = codec.getOutputBuffer(index) ?: continue
                // `info.offset` is passed through rather than assumed zero: the
                // JNI side reads from the buffer's base address, which ignores
                // the position MediaCodec sets, so the offset has to travel
                // alongside the buffer or the packet is read from the wrong
                // place.
                EchoNative.nativeSendMic(handle, output, info.offset, info.size)
            } finally {
                codec.releaseOutputBuffer(index, false)
            }
        }
    }

    private fun fail(message: String) {
        running = false
        Log.w(TAG, message)
        onError(message)
    }

    private fun encoderFormat(): MediaFormat =
        MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, SAMPLE_RATE, CHANNELS).apply {
            setInteger(MediaFormat.KEY_BIT_RATE, BITRATE)
            // One frame's worth with headroom. Left to the codec's own default
            // this is sometimes sized for a whole second, which would let the
            // encoder accept a buffer far larger than a frame and emit packets
            // in bursts rather than at the cadence the loop reads them.
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, BYTES_PER_FRAME * 2)
        }

    /**
     * The name of an Opus encoder, or null if this device has none.
     *
     * Queried with a bare format — mime, rate, channels — because a query
     * carrying bitrate and buffer hints can fail to match an encoder that would
     * in fact accept them, which would read as "no Opus encoder" on a device
     * that has one.
     */
    private fun findOpusEncoder(): String? = runCatching {
        val probe = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, SAMPLE_RATE, CHANNELS)
        MediaCodecList(MediaCodecList.REGULAR_CODECS).findEncoderForFormat(probe)
    }.getOrNull()

    private companion object {
        const val TAG = "EchoMic"

        /** Opus is a 48 kHz codec internally; feeding it 48 kHz avoids a resample. */
        const val SAMPLE_RATE = 48_000
        const val CHANNELS = 1
        const val CHANNEL_MASK = AudioFormat.CHANNEL_IN_MONO
        const val ENCODING = AudioFormat.ENCODING_PCM_16BIT
        const val BYTES_PER_SAMPLE = 2

        /**
         * 20 ms per packet — the Opus default and the usual VoIP trade. Ten
         * would halve the packetisation delay and nearly double the packet rate
         * for a phone uplink to carry; forty would save bandwidth nobody is
         * short of and add delay to the one path that cannot absorb it.
         */
        const val FRAME_MS = 20
        const val BYTES_PER_FRAME = SAMPLE_RATE * BYTES_PER_SAMPLE * FRAME_MS / 1000

        /** Voice at 24 kbps is transparent for speech; the video stream is a thousand times this. */
        const val BITRATE = 24_000

        /**
         * Zero would spin, and a long wait would stall the loop past the next
         * hardware buffer. One frame is the natural bound: if the encoder cannot
         * take a buffer within that, it is behind and this frame is dropped.
         */
        const val DEQUEUE_TIMEOUT_US = FRAME_MS * 1000L
    }
}
