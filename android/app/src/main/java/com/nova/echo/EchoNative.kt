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

    /** Queue and receive statistics as JSON. */
    external fun nativeStats(handle: Long): String

    /** End the session and free the handle. Wakes anything blocked. Idempotent. */
    external fun nativeClose(handle: Long)
}
