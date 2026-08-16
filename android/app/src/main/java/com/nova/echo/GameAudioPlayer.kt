package com.nova.echo

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Process
import android.util.Log
import java.nio.ByteBuffer
import java.nio.ByteOrder
import kotlin.concurrent.thread

/**
 * Plays the host's game audio: pulls scheduled Opus packets from Rust, decodes
 * them, and writes PCM to an [AudioTrack].
 *
 * The mirror of [MicCapture], and the same single-threaded shape for the same
 * reason: read, decode, write are strictly serial at 20 ms cadence, and
 * splitting them across threads would add a hand-off queue whose only product is
 * latency. Here the clock is [AudioTrack.write] in blocking mode — the hardware
 * consumes at exactly real time, so a full track buffer parks this thread until
 * the next frame is genuinely due. Nothing needs a timer.
 *
 * ## The division of labour with Rust
 *
 * **Rust owns *when*; this owns *what*.** The jitter buffer, sequence window,
 * drift correction and the underran/paused split all live in
 * `echo_client::audio`, where they are unit-testable on a desktop and identical
 * to the buffer that already runs the host's microphone path. This class asks
 * for one step and renders whatever it is told.
 *
 * That split is forced rather than chosen: `audiopus` builds libopus from C via
 * cmake and does not cross-compile under `cargo-ndk` (`could not find native
 * static library 'opus'` — verified, not assumed), which is the same wall that
 * put the microphone's *encoder* here rather than in Rust.
 *
 * ## The consequence, stated plainly: no PLC
 *
 * MediaCodec exposes no packet-loss-concealment entry point. The host's
 * microphone path conceals a lost packet with genuine Opus PLC; the best this
 * can do is a frame of silence. `AUDIO_CONCEAL` is still handled separately from
 * `AUDIO_SILENCE` so the counters never confuse a lost packet with a quiet host,
 * and so a decoder that *can* conceal is a change to one branch here and nothing
 * else.
 *
 * ## The trap: codec config runs the other way
 *
 * [MicCapture] exists partly to **strip** the Opus identification header from
 * its encoder's output. Decoding needs the mirror image: MediaCodec will not
 * start an Opus decoder without `csd-0`/`csd-1`/`csd-2`, and Echo's wire carries
 * bare Opus packets with no container to take them from. So they are
 * synthesised here — see [decoderFormat]. Omit them and the codec throws at
 * `configure` with nothing to say which field it wanted.
 *
 * ## Echo cancellation
 *
 * Not handled here, and deliberately: cancellation belongs to the *capture*
 * side, and [MicCapture] already attaches [android.media.audiofx.AcousticEchoCanceler]
 * to its `AudioRecord` session and records from `VOICE_COMMUNICATION`.
 *
 * One caveat worth knowing rather than discovering: this track uses
 * `USAGE_MEDIA`, which is correct for stereo game audio and what keeps it out of
 * the platform's mono voice path — but a hardware AEC references the voice
 * downlink, so on many devices it will **not** cancel media playback. On a phone
 * speaker at volume, expect the microphone to hear some of the game. Headphones
 * remove the problem entirely; muting the microphone removes it in the other
 * direction. Routing this through `USAGE_VOICE_COMMUNICATION` would let the AEC
 * see it, at the cost of downmixing the game to the voice path — a bad trade for
 * a game-streaming client, and the reason it is not done.
 */
class GameAudioPlayer(
    private val handle: Long,
    private val onError: (String) -> Unit,
) {

    @Volatile private var running = false
    private var worker: Thread? = null

    val isRunning: Boolean get() = running

    /**
     * Begin playback. Returns false if no Opus decoder exists, having already
     * reported why.
     *
     * Safe to start before the session is granted: an unarmed buffer answers
     * `AUDIO_IDLE`, so this simply idles until audio begins to arrive.
     */
    fun start(): Boolean {
        if (running) return true
        if (findOpusDecoder() == null) {
            onError("this device has no Opus decoder — game audio unavailable")
            return false
        }
        running = true
        worker = thread(name = "echo-audio") { playLoop() }
        return true
    }

    fun stop() {
        running = false
        worker?.join(1_000)
        worker = null
    }

    private fun playLoop() {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)

        var track: AudioTrack? = null
        var codec: MediaCodec? = null

        try {
            val minBuffer = AudioTrack.getMinBufferSize(SAMPLE_RATE, CHANNEL_MASK, ENCODING)
            if (minBuffer <= 0) {
                fail("this device cannot play 48 kHz stereo")
                return
            }
            // Four frames of headroom over the platform minimum. The track
            // buffer is the shock absorber for *this thread's* scheduling, which
            // is a separate problem from network jitter — that one is already
            // absorbed in Rust. Sizing it much larger would only add latency
            // that the jitter buffer has no way to claw back.
            val bufferBytes = maxOf(minBuffer, BYTES_PER_FRAME * 4)

            track = AudioTrack.Builder()
                .setAudioAttributes(
                    AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_MEDIA)
                        .setContentType(AudioAttributes.CONTENT_TYPE_MOVIE)
                        .build()
                )
                .setAudioFormat(
                    AudioFormat.Builder()
                        .setEncoding(ENCODING)
                        .setSampleRate(SAMPLE_RATE)
                        .setChannelMask(CHANNEL_MASK)
                        .build()
                )
                .setBufferSizeInBytes(bufferBytes)
                .setTransferMode(AudioTrack.MODE_STREAM)
                .build()

            if (track.state != AudioTrack.STATE_INITIALIZED) {
                fail("could not open the audio output")
                return
            }

            codec = MediaCodec.createByCodecName(findOpusDecoder()!!)
            codec.configure(decoderFormat(), null, null, 0)
            codec.start()
            track.play()
            Log.i(TAG, "game audio open: 48 kHz stereo Opus, ${FRAME_MS}ms frames")

            // Sized to the largest packet Opus can emit, so AUDIO_TOO_SMALL is
            // unreachable. Direct, because the JNI side writes to the buffer's
            // base address and refuses anything else rather than silently
            // falling back to a per-packet Java array.
            val packet = ByteBuffer.allocateDirect(MAX_OPUS_PACKET)
            val meta = LongArray(1)
            val info = MediaCodec.BufferInfo()
            val silence = ByteArray(BYTES_PER_FRAME)
            var samplesWritten = 0L
            var idleLogged = false

            while (running) {
                packet.clear()
                when (val step = EchoNative.nativePollAudio(handle, packet, meta)) {
                    EchoNative.AUDIO_PACKET -> {
                        idleLogged = false
                        val size = meta[0].toInt()
                        val index = codec.dequeueInputBuffer(DEQUEUE_TIMEOUT_US)
                        if (index >= 0) {
                            val input = codec.getInputBuffer(index)!!
                            input.clear()
                            packet.limit(size)
                            packet.position(0)
                            input.put(packet)
                            // Derived from samples, never a wall clock: the
                            // decoder wants a monotonic timestamp, and a clock
                            // would fold this thread's scheduling jitter into
                            // it. The sample count is exact by construction.
                            val ptsUs = samplesWritten * 1_000_000L / SAMPLE_RATE
                            codec.queueInputBuffer(index, 0, size, ptsUs, 0)
                            samplesWritten += SAMPLES_PER_FRAME
                        }
                        // No input buffer means the decoder is backed up. The
                        // packet is dropped rather than queued late — stale
                        // audio arriving after its moment is worse than a gap,
                        // and the buffer has already moved on.
                    }

                    // A packet was lost. Real Opus PLC would extrapolate it;
                    // MediaCodec offers no way to ask, so this is a frame of
                    // silence. Kept as its own branch, not folded into the one
                    // below, because they mean different things and the fix
                    // lands exactly here.
                    EchoNative.AUDIO_CONCEAL -> {
                        idleLogged = false
                        writeFully(track, silence)
                        samplesWritten += SAMPLES_PER_FRAME
                    }

                    // Buffer still filling, or momentarily dry. Silence keeps
                    // the track fed so the hardware does not underrun and click.
                    EchoNative.AUDIO_SILENCE -> {
                        idleLogged = false
                        writeFully(track, silence)
                        samplesWritten += SAMPLES_PER_FRAME
                    }

                    // Nothing expected. Writing silence forever would hold the
                    // output device open for a host that has gone quiet, so this
                    // parks instead — and because that costs the track's clock,
                    // it is the one branch that needs a timer of its own.
                    EchoNative.AUDIO_IDLE -> {
                        if (!idleLogged) {
                            Log.d(TAG, "no audio expected — idling")
                            idleLogged = true
                        }
                        Thread.sleep(FRAME_MS.toLong())
                    }

                    EchoNative.AUDIO_BAD_HANDLE -> {
                        // The session is gone. Not an error worth surfacing —
                        // this races a normal disconnect every time.
                        Log.i(TAG, "audio handle closed — stopping playback")
                        return
                    }

                    EchoNative.AUDIO_TOO_SMALL -> {
                        // Unreachable with a MAX_OPUS_PACKET buffer, so if it
                        // ever fires the wire format changed underneath us.
                        fail("audio packet of ${meta[0]} bytes exceeds $MAX_OPUS_PACKET")
                        return
                    }

                    else -> {
                        fail("unknown audio step $step")
                        return
                    }
                }

                drainDecoder(codec, info, track)
            }
        } catch (e: InterruptedException) {
            // stop() during the idle sleep. Normal.
            Thread.currentThread().interrupt()
        } catch (e: Throwable) {
            fail("game audio failed: ${e.message ?: e.javaClass.simpleName}")
        } finally {
            running = false
            runCatching { track?.stop() }
            runCatching { track?.release() }
            runCatching { codec?.stop() }
            runCatching { codec?.release() }
            Log.i(TAG, "game audio closed")
        }
    }

    /** Write every decoded frame the codec has ready. Returns when it has none. */
    private fun drainDecoder(codec: MediaCodec, info: MediaCodec.BufferInfo, track: AudioTrack) {
        while (true) {
            val index = codec.dequeueOutputBuffer(info, 0)
            if (index < 0) return // TRY_AGAIN_LATER, or a format change we ignore

            try {
                if (info.size > 0) {
                    val output = codec.getOutputBuffer(index)
                    if (output != null) {
                        output.position(info.offset)
                        output.limit(info.offset + info.size)
                        // Blocking write: this is the clock. A full track buffer
                        // parks the thread until the hardware has consumed
                        // enough, which is exactly the pacing this loop wants —
                        // and the reason nothing here sleeps on the audio path.
                        track.write(output, info.size, AudioTrack.WRITE_BLOCKING)
                    }
                }
            } finally {
                codec.releaseOutputBuffer(index, false)
            }
        }
    }

    /** [AudioTrack.write] may take less than offered; silence must go out whole. */
    private fun writeFully(track: AudioTrack, pcm: ByteArray) {
        var offset = 0
        while (offset < pcm.size && running) {
            val n = track.write(pcm, offset, pcm.size - offset, AudioTrack.WRITE_BLOCKING)
            if (n <= 0) return // dead or stopping; the loop's own checks handle it
            offset += n
        }
    }

    private fun fail(message: String) {
        running = false
        Log.w(TAG, message)
        onError(message)
    }

    /**
     * The decoder's input format, including the codec-specific data MediaCodec
     * refuses to start an Opus decoder without.
     *
     * Echo's wire carries **bare Opus packets** — the transport is deliberately
     * container-free — so there is nothing to copy these out of and they are
     * built here. This is [MicCapture]'s codec-config trap seen from the other
     * end: there the header had to be stripped before sending, here an identical
     * header has to be manufactured before decoding.
     *
     * - `csd-0` — the 19-byte `OpusHead` identification header (RFC 7845 §5.1).
     * - `csd-1` — pre-skip, in **nanoseconds**, as a little-endian 64-bit value.
     * - `csd-2` — seek pre-roll, same encoding. 80 ms is the value RFC 7845
     *   specifies for Opus and what every Android decoder expects.
     *
     * Little-endian is not a choice: both the `OpusHead` fields and Android's
     * csd longs are defined that way, and a big-endian pre-skip is read as an
     * absurd number of samples to discard — which silently swallows the opening
     * of the stream rather than failing.
     */
    private fun decoderFormat(): MediaFormat =
        MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, SAMPLE_RATE, CHANNELS).apply {
            val head = ByteBuffer.allocate(19).order(ByteOrder.LITTLE_ENDIAN).apply {
                put("OpusHead".toByteArray(Charsets.US_ASCII))
                put(1)                        // version
                put(CHANNELS.toByte())        // channel count — stereo, unlike the mic
                putShort(PRE_SKIP_SAMPLES.toShort())
                putInt(SAMPLE_RATE)           // original input rate
                putShort(0)                   // output gain, Q7.8 dB
                put(0)                        // channel mapping family: 0 = mono/stereo
            }
            head.flip()
            setByteBuffer("csd-0", head)
            setByteBuffer("csd-1", nanosLe(PRE_SKIP_SAMPLES * 1_000_000_000L / SAMPLE_RATE))
            setByteBuffer("csd-2", nanosLe(SEEK_PREROLL_NS))
        }

    private fun nanosLe(value: Long): ByteBuffer =
        ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(value).apply { flip() }

    /**
     * The name of an Opus decoder, or null if this device has none.
     *
     * Probed with a bare format for the same reason [MicCapture] does: a query
     * carrying csd or bitrate hints can fail to match a decoder that would in
     * fact accept them, reading as "no Opus decoder" on a device that has one.
     */
    private fun findOpusDecoder(): String? = runCatching {
        val probe = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, SAMPLE_RATE, CHANNELS)
        MediaCodecList(MediaCodecList.REGULAR_CODECS).findDecoderForFormat(probe)
    }.getOrNull()

    private companion object {
        const val TAG = "EchoAudio"

        const val SAMPLE_RATE = 48_000

        /**
         * Stereo, where the microphone is mono. The host captures a game's mix,
         * not a voice, and collapsing it would throw away positional audio the
         * player is using. This is also why the decoder here cannot be shared
         * with the microphone's: channel count is fixed at configure time.
         */
        const val CHANNELS = 2
        const val CHANNEL_MASK = AudioFormat.CHANNEL_OUT_STEREO
        const val ENCODING = AudioFormat.ENCODING_PCM_16BIT
        const val BYTES_PER_SAMPLE = 2

        /** Matches the 20 ms the host negotiates for an Echo session. */
        const val FRAME_MS = 20
        const val SAMPLES_PER_FRAME = SAMPLE_RATE * FRAME_MS / 1000
        const val BYTES_PER_FRAME = SAMPLES_PER_FRAME * BYTES_PER_SAMPLE * CHANNELS

        /** The largest packet Opus will emit — `audio_channel::MAX_PAYLOAD`. */
        const val MAX_OPUS_PACKET = 1275

        /** libopus's default at 48 kHz: 312 samples, 6.5 ms. */
        const val PRE_SKIP_SAMPLES = 312L

        /** RFC 7845's seek pre-roll for Opus: 80 ms. */
        const val SEEK_PREROLL_NS = 80_000_000L

        /**
         * Zero would spin; a long wait would stall past the next frame. One
         * frame is the natural bound — a decoder that cannot take a buffer
         * within that is behind, and this packet is dropped.
         */
        const val DEQUEUE_TIMEOUT_US = FRAME_MS * 1000L
    }
}
