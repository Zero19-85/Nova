//! The Rust↔Kotlin bridge for Echo.
//!
//! Everything here is marshalling. The connection sequence, the control
//! channel, FEC, decryption, and the keyframe gate all live in `echo-client`,
//! which the desktop CLI links too — so this file is deliberately the only code
//! that Android has and the CLI does not, and it contains no protocol logic at
//! all. Every `extern "C"` function below parses its arguments, calls one safe
//! Rust function, and marshals the result back.
//!
//! ## Pull, not push
//!
//! Kotlin calls *down* for frames; Rust never calls *up*. A push callback would
//! need `AttachCurrentThread` plus a `GlobalRef` on the receive thread for every
//! frame, and — more importantly — it would invert control so the decoder's
//! backpressure could not reach the network layer. Pulling means the bounded
//! queue in [`frames`] *is* the backpressure signal, the drop policy lives next
//! to the frame metadata that informs it, and this library never needs a
//! `JavaVM` handle.
//!
//! The frame path is zero-allocation by construction: Kotlin dequeues a
//! MediaCodec input buffer (already a direct `ByteBuffer`) and passes it down;
//! [`Java_com_nova_echo_EchoNative_nativeFillBuffer`] writes the frame straight
//! into it. One copy, no Java array, no per-frame object.
//!
//! ## Threading contract
//!
//! - `nativeConnect` returns **immediately**. The session runs on a Tokio
//!   runtime this library owns; progress and failures arrive as events.
//! - `nativePollEvent` and `nativeFillBuffer` block with a timeout, so they must
//!   be called from worker threads — never the Android main thread.
//! - `nativeClose` is idempotent and safe to call while other threads are
//!   blocked; it wakes them.
//!
//! ## Kotlin declaration
//!
//! ```kotlin
//! object EchoNative {
//!     init { System.loadLibrary("echo") }
//!     external fun nativeInit()
//!     external fun nativeIdentityFingerprint(dir: String): String
//!     external fun nativePair(configJson: String): Long
//!     external fun nativeConnect(configJson: String): Long
//!     external fun nativePollEvent(handle: Long, timeoutMs: Int): String?
//!     external fun nativeFillBuffer(handle: Long, buf: ByteBuffer, meta: LongArray, timeoutMs: Int): Int
//!     external fun nativeStats(handle: Long): String
//!     external fun nativeClose(handle: Long)
//! }
//! ```

pub mod frames;

use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jni::objects::{JByteBuffer, JClass, JLongArray, JString};
use jni::sys::{jint, jlong};
use jni::JNIEnv;

use echo_client::{input, pairing};
use echo_client::session::{self, ConnectOptions, Event, OpenPath, Progress, StreamOptions};
use nova_core::identity::Identity;

use frames::{FrameQueue, QueueSink};

// ── Return codes for nativeFillBuffer ───────────────────────────────────────
/// No frame arrived within the timeout. Not an error — the normal state of a
/// feeder thread that is keeping up.
pub const FILL_TIMEOUT: jint = -1;
/// The frame does not fit the supplied buffer; `meta[0]` holds the size needed.
/// MediaCodec input buffers are sized by the decoder, so this means the stream's
/// geometry disagrees with how the codec was configured.
pub const FILL_TOO_SMALL: jint = -2;
/// The session has ended. Stop feeding.
pub const FILL_ENDED: jint = -3;
/// The handle was invalid, or the arguments could not be read.
pub const FILL_BAD_HANDLE: jint = -4;

/// How long `nativeClose` waits for the session to tell the host it is done.
///
/// Long enough for one RUDP round trip including a retransmit, short enough
/// that quitting never feels stuck — this runs on a thread the UI is waiting on.
const CLOSE_GRACE: Duration = Duration::from_millis(1500);

/// Distinguishes a live handle from a stale or fabricated `jlong`. A double
/// `close()` from Kotlin is an ordinary bug; without this it would be a
/// use-after-free reachable from managed code.
const MAGIC: u64 = 0xEC0_5E5_51_04;

/// Everything one session owns. Boxed, and its raw pointer is the `jlong`
/// handle Kotlin holds.
struct EchoHandle {
    magic: u64,
    /// `Option` so `close` can consume the runtime for an orderly shutdown
    /// while the handle itself stays valid enough to reject later calls.
    runtime: Option<tokio::runtime::Runtime>,
    events: Mutex<Receiver<String>>,
    frames: Arc<FrameQueue>,
    stop: tokio::sync::watch::Sender<bool>,
    /// Where Kotlin posts GameStream input packets, when a session carries them.
    input: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    /// The session task, so `close` can wait for it to unwind.
    ///
    /// Load-bearing: `Runtime::shutdown_timeout` only waits for *blocking*
    /// tasks — async tasks are cancelled the instant the runtime drops. The
    /// session's final act is telling the host `stop_session`, so without this
    /// wait that call is killed mid-flight and the host holds the session until
    /// its idle sweep reclaims it up to a minute later. Live 2026-08-15: every
    /// session in the host log ended with `⌛ … reclaiming the slot`, never a
    /// clean stop.
    session: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl EchoHandle {
    /// Reconstitute a handle from Kotlin. Returns `None` for null or for
    /// anything that does not carry our magic.
    ///
    /// # Safety
    /// `handle` must be a value previously returned by `nativeConnect` and not
    /// yet passed to `nativeClose`.
    unsafe fn from_raw<'a>(handle: jlong) -> Option<&'a mut EchoHandle> {
        if handle == 0 {
            return None;
        }
        let ptr = handle as *mut EchoHandle;
        let candidate = &mut *ptr;
        (candidate.magic == MAGIC).then_some(candidate)
    }
}

/// Sends session events to whoever is polling, as JSON.
struct ChannelProgress(Sender<String>);

impl Progress for ChannelProgress {
    fn event(&mut self, event: Event) {
        // A closed receiver means Kotlin stopped polling; the session should
        // keep running regardless, so the error is deliberately ignored.
        let _ = self.0.send(event.to_json().to_string());
    }
}

impl ChannelProgress {
    /// Emit a bridge-level event — one that has no `session::Event` because it
    /// describes this layer rather than the protocol.
    fn raw(&mut self, value: serde_json::Value) {
        let _ = self.0.send(value.to_string());
    }
}

// ── JNI entry points ────────────────────────────────────────────────────────

/// One-time process setup: logging and the panic hook.
///
/// Safe to call more than once — Android calls it from a `companion object`
/// initialiser, which runs once per class load, and that is not a guarantee
/// worth relying on.
#[no_mangle]
pub extern "system" fn Java_com_nova_echo_EchoNative_nativeInit(_env: JNIEnv, _class: JClass) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        #[cfg(target_os = "android")]
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag("EchoNative"),
        );
    });
}

/// This client's certificate fingerprint, generating the identity on first call.
///
/// `dir` must be app-private storage (`context.filesDir`): it will hold a
/// private key, and Echo's identity *is* that key.
#[no_mangle]
pub extern "system" fn Java_com_nova_echo_EchoNative_nativeIdentityFingerprint<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    dir: JString<'local>,
) -> jni::sys::jstring {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let dir: String = env
            .get_string(&dir)
            .map_err(|e| format!("read directory argument: {e}"))?
            .into();
        let identity = Identity::load_or_create_rsa2048(std::path::Path::new(&dir), "echo", "echo-android")?;
        Ok::<String, String>(identity.fingerprint)
    }));

    match flatten(result) {
        Ok(fp) => match env.new_string(fp) {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        Err(msg) => {
            throw(&mut env, &msg);
            std::ptr::null_mut()
        }
    }
}

/// Pair with a Nova host on the LAN. Returns a handle immediately; progress
/// arrives as events, exactly like `nativeConnect`.
///
/// Pairing is event-driven rather than blocking because its middle step is a
/// human: Rust generates the PIN and emits `awaiting_consent`, the app shows it,
/// and somebody walks over to the PC and types it into Nova's dialog. A blocking
/// call would give the UI nothing to display during the one part of the flow
/// that is entirely about display.
///
/// Config JSON:
/// ```json
/// { "identity_dir": "/data/…/files", "host": "10.0.0.205",
///   "device_name": "Pixel 9 Pro", "consent_secs": 180 }
/// ```
///
/// Must be on the LAN: this is Nova's unauthenticated HTTP port, which is
/// exactly why the handshake carries its own mutual proof. Streaming afterwards
/// works from anywhere.
#[no_mangle]
pub extern "system" fn Java_com_nova_echo_EchoNative_nativePair<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    config_json: JString<'local>,
) -> jlong {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let raw: String = env
            .get_string(&config_json)
            .map_err(|e| format!("read config argument: {e}"))?
            .into();
        start_pairing(&raw)
    }));

    match flatten(result) {
        Ok(handle) => handle,
        Err(msg) => {
            throw(&mut env, &msg);
            0
        }
    }
}

/// Start a session. Returns a handle immediately; the work happens on this
/// library's own runtime.
///
/// Failures are reported as `{"type":"error"}` events rather than exceptions,
/// because by the time most of them can happen this call has already returned.
/// Only a malformed config or a runtime that will not start throws.
///
/// Config JSON — the CLI's own argument set, so both configure identically:
/// ```json
/// {
///   "identity_dir": "/data/…/files",
///   "relay_url": "https://relay:8443/v1/signal",
///   "relay_pin": "<64 hex>",
///   "host_fingerprint": "<64 hex>",
///   "res": "1080p", "fps": 60, "codec": "hevc", "bitrate_kbps": 20000,
///   "punch_secs": 8
/// }
/// ```
#[no_mangle]
pub extern "system" fn Java_com_nova_echo_EchoNative_nativeConnect<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    config_json: JString<'local>,
) -> jlong {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let raw: String = env
            .get_string(&config_json)
            .map_err(|e| format!("read config argument: {e}"))?
            .into();
        start_session(&raw)
    }));

    match flatten(result) {
        Ok(handle) => handle,
        Err(msg) => {
            throw(&mut env, &msg);
            0
        }
    }
}

/// The next control-plane event as JSON, or `null` on timeout.
///
/// Blocks up to `timeout_ms`, so it belongs on a worker thread. Kotlin should
/// treat `null` as "nothing happened" and keep polling.
#[no_mangle]
pub extern "system" fn Java_com_nova_echo_EchoNative_nativePollEvent<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    timeout_ms: jint,
) -> jni::sys::jstring {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let session = unsafe { EchoHandle::from_raw(handle) }.ok_or("invalid session handle")?;
        let events = session.events.lock().unwrap_or_else(|e| e.into_inner());
        match events.recv_timeout(Duration::from_millis(timeout_ms.max(0) as u64)) {
            Ok(json) => Ok::<Option<String>, String>(Some(json)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            // The sender is gone: the session task finished. Not an error —
            // `ended` was already delivered before the channel dropped.
            Err(RecvTimeoutError::Disconnected) => Ok(None),
        }
    }));

    match flatten(result) {
        Ok(Some(json)) => match env.new_string(json) {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        Ok(None) => std::ptr::null_mut(),
        Err(msg) => {
            throw(&mut env, &msg);
            std::ptr::null_mut()
        }
    }
}

/// Copy the next frame into `buf`, waiting up to `timeout_ms`.
///
/// `buf` must be a **direct** `ByteBuffer` — in practice a MediaCodec input
/// buffer, which is what makes this a single copy into memory the decoder
/// already owns.
///
/// Returns the number of bytes written, or one of the `FILL_*` codes. On
/// success `meta` receives `[size, flags, pts_us]`, where flags bit 0 marks a
/// keyframe; Kotlin passes that straight to `queueInputBuffer`.
///
/// The presentation timestamp is derived from the wire frame index rather than a
/// clock. It only has to be monotonic for a decoder that is not doing
/// presentation timing itself — the frame index is exactly that, and it survives
/// loss, whereas a receive-time clock would encode our own jitter into the
/// stream.
#[no_mangle]
pub extern "system" fn Java_com_nova_echo_EchoNative_nativeFillBuffer<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    buf: JByteBuffer<'local>,
    meta: JLongArray<'local>,
    timeout_ms: jint,
) -> jint {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(session) = (unsafe { EchoHandle::from_raw(handle) }) else {
            return FILL_BAD_HANDLE;
        };
        let queue = session.frames.clone();

        let (Ok(addr), Ok(capacity)) =
            (env.get_direct_buffer_address(&buf), env.get_direct_buffer_capacity(&buf))
        else {
            // Not a direct buffer. Copying via a Java array would silently
            // reintroduce the per-frame allocation this design exists to avoid,
            // so this is refused rather than worked around.
            return FILL_BAD_HANDLE;
        };

        let Some(frame) = queue.pop_timeout(Duration::from_millis(timeout_ms.max(0) as u64)) else {
            return if queue.is_closed() { FILL_ENDED } else { FILL_TIMEOUT };
        };

        let size = frame.data.len();
        if size > capacity {
            let _ = env.set_long_array_region(&meta, 0, &[size as jlong]);
            return FILL_TOO_SMALL;
        }

        // SAFETY: `addr`/`capacity` describe the direct buffer's storage, which
        // the JVM guarantees is valid and pinned for as long as the ByteBuffer
        // object is alive — it is alive for this call, since Kotlin holds the
        // reference across it. `size <= capacity` was just checked.
        unsafe { std::ptr::copy_nonoverlapping(frame.data.as_ptr(), addr, size) };

        let flags = if frame.is_keyframe() { 1i64 } else { 0i64 };
        let pts_us = frame.index as i64;
        let _ = env.set_long_array_region(&meta, 0, &[size as jlong, flags, pts_us]);
        size as jint
    }));

    match result {
        Ok(code) => code,
        Err(_) => {
            throw(&mut env, "panic while filling a frame buffer");
            FILL_BAD_HANDLE
        }
    }
}

/// Receive and queue statistics as JSON, for diagnostics and an on-screen
/// overlay.
#[no_mangle]
pub extern "system" fn Java_com_nova_echo_EchoNative_nativeStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let session = unsafe { EchoHandle::from_raw(handle) }.ok_or("invalid session handle")?;
        let q = session.frames.stats();
        let (batch_last, batch_worst) = echo_client::input::batch_stats();
        let (rtt_last, rtt_best) = echo_client::session::rtt_stats();
        Ok::<String, String>(
            serde_json::json!({
                "input_batch": batch_last,
                "input_batch_worst": batch_worst,
                "rtt_ms": rtt_last,
                "rtt_best_ms": rtt_best,
                "queue_depth": q.depth,
                "frames_delivered": q.delivered,
                "frames_dropped_overflow": q.dropped_overflow,
                "frames_dropped_waiting_keyframe": q.dropped_waiting_keyframe,
                "frame_age_ms": q.last_frame_age_ms,
                "worst_frame_age_ms": q.worst_frame_age_ms,
            })
            .to_string(),
        )
    }));

    match flatten(result) {
        Ok(json) => match env.new_string(json) {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        Err(msg) => {
            throw(&mut env, &msg);
            std::ptr::null_mut()
        }
    }
}

/// Post one input event to the host.
///
/// Takes decomposed integers rather than a byte array because it is called at
/// pointer-motion rates: a `[u8]` per event would mean a JNI array allocation
/// and copy for twelve bytes, tens of times a second. The packet is built on
/// this side, by [`echo_client::input`], so Kotlin never encodes wire format.
///
/// `kind` selects the event; the remaining arguments mean different things per
/// kind, documented on the Kotlin side in `EchoNative`:
///
/// | kind | meaning     | a          | b          | c      | d       |
/// |------|-------------|------------|------------|--------|---------|
/// | 1    | mouse move  | dx         | dy         | —      | —       |
/// | 2    | mouse abs   | x          | y          | width  | height  |
/// | 3    | mouse button| button 1-5 | 1=down     | —      | —       |
/// | 4    | scroll      | amount     | —          | —      | —       |
/// | 5    | key         | VK code    | 1=down     | mods   | —       |
/// | 6    | release all | —          | —          | —      | —       |
///
/// Returns `true` if the event was queued. `false` means the handle was invalid
/// or the packet was a no-op (a zero delta), never that the host rejected it —
/// input is fire-and-forget, because blocking a UI thread on a network round
/// trip per keystroke is not a thing anyone wants.
/// Push the platform layer's input telemetry down so the session report can
/// carry it. Rate and capture state are only visible in the View.
#[no_mangle]
pub extern "system" fn Java_com_nova_echo_EchoNative_nativeReportUiState(
    _env: JNIEnv,
    _class: JClass,
    peak_rate: jint,
    peak_samples: jint,
    capture_held: jni::sys::jboolean,
) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        echo_client::input::record_ui_state(
            peak_rate.max(0) as u32,
            peak_samples.max(0) as u32,
            capture_held != 0,
        );
    }));
}

#[no_mangle]
pub extern "system" fn Java_com_nova_echo_EchoNative_nativeSendInput(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    kind: jint,
    a: jint,
    b: jint,
    c: jint,
    d: jint,
) -> jni::sys::jboolean {
    let queued = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(echo) = (unsafe { EchoHandle::from_raw(handle) }) else {
            return false;
        };
        let Some(tx) = echo.input.as_ref() else {
            return false; // a pairing handle has no session to inject into
        };

        // Saturating rather than wrapping: a flick of the wrist past i16 range
        // should pin the pointer's motion, not reverse it.
        let clamp = |v: jint| v.clamp(i16::MIN as jint, i16::MAX as jint) as i16;

        let packet = match kind {
            1 => input::mouse_move_relative(clamp(a), clamp(b)),
            2 => input::mouse_move_absolute(clamp(a), clamp(b), clamp(c), clamp(d)),
            3 => input::MouseButton::from_code(a as u8).map(|btn| input::mouse_button(btn, b == 1)),
            4 => input::scroll(clamp(a)),
            5 => Some(input::keyboard(a as u16, c as u8, b == 1)),
            // Release everything held. Queued like any other input so it cannot
            // overtake a key-down already in flight — arriving out of order
            // would release a key before it was pressed and leave it stuck,
            // which is the exact failure this exists to prevent.
            6 => {
                return input::release_all()
                    .into_iter()
                    .all(|p| tx.send(p).is_ok());
            }
            _ => None,
        };

        match packet {
            Some(p) => tx.send(p).is_ok(),
            None => false,
        }
    }))
    .unwrap_or(false);

    u8::from(queued)
}

/// End the session and free the handle.
///
/// Wakes anything blocked in `nativeFillBuffer`/`nativePollEvent` first, so a
/// feeder thread cannot be left parked. Kotlin must zero its handle field under
/// a lock before calling — the magic check catches a double close, but relying
/// on it is racy under concurrent calls.
#[no_mangle]
pub extern "system" fn Java_com_nova_echo_EchoNative_nativeClose(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if handle == 0 {
            return;
        }
        let ptr = handle as *mut EchoHandle;
        // SAFETY: checked for our magic before taking ownership, so a stale or
        // fabricated handle is refused rather than freed.
        unsafe {
            if (*ptr).magic != MAGIC {
                return;
            }
            (*ptr).magic = 0;
            let mut boxed = Box::from_raw(ptr);

            // Order matters: signal the session to stop, wake the feeder, wait
            // for the session to say goodbye to the host, and only then shut the
            // runtime down. Dropping the runtime first would block here waiting
            // for tasks that were never told to finish.
            let _ = boxed.stop.send(true);
            boxed.frames.close();

            let session = boxed.session.lock().ok().and_then(|mut s| s.take());
            if let (Some(runtime), Some(session)) = (boxed.runtime.as_ref(), session) {
                // The session's last act is `stop_session`, which is a network
                // round trip. `shutdown_timeout` would cancel it outright, so it
                // is awaited here first — bounded, because teardown must never
                // hang the caller (this runs on a UI-triggered thread).
                runtime.block_on(async {
                    let _ = tokio::time::timeout(CLOSE_GRACE, session).await;
                });
            }

            if let Some(runtime) = boxed.runtime.take() {
                // Bounded rather than indefinite: teardown must not be able to
                // hang the UI thread that called close().
                runtime.shutdown_timeout(Duration::from_secs(2));
            }
        }
    }));
}

// ── Plumbing ────────────────────────────────────────────────────────────────

/// Build the runtime, spawn the session, and hand back a boxed handle.
fn start_session(config_json: &str) -> Result<jlong, String> {
    let cfg: serde_json::Value =
        serde_json::from_str(config_json).map_err(|e| format!("config is not valid JSON: {e}"))?;

    let identity_dir = str_field(&cfg, "identity_dir")?;
    let connect = ConnectOptions {
        relay_url: str_field(&cfg, "relay_url")?,
        relay_pin: str_field(&cfg, "relay_pin")?,
        host_fingerprint: str_field(&cfg, "host_fingerprint")?,
        punch_timeout: Duration::from_secs(cfg.get("punch_secs").and_then(|v| v.as_u64()).unwrap_or(8)),
    };
    // Unbounded, and that is a considered choice: the producer is the UI thread
    // delivering pointer events, so any bound would mean either blocking the UI
    // or silently discarding input. The consumer only forwards to the control
    // channel, and a session that stops draining is ending anyway.
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    let defaults = StreamOptions::default();
    let stream = StreamOptions {
        res: cfg.get("res").and_then(|v| v.as_str()).unwrap_or(&defaults.res).to_string(),
        fps: cfg.get("fps").and_then(|v| v.as_u64()).unwrap_or(defaults.fps as u64) as u32,
        codec: cfg.get("codec").and_then(|v| v.as_str()).unwrap_or(&defaults.codec).to_string(),
        bitrate_kbps: cfg
            .get("bitrate_kbps")
            .and_then(|v| v.as_u64())
            .unwrap_or(defaults.bitrate_kbps as u64) as u32,
        // The LAN debug door is deliberately not exposed to Kotlin: the host
        // refuses that port from non-private addresses, so offering it in an app
        // would only produce confusing failures.
        control: None,
    };

    let identity = Identity::load_or_create_rsa2048(
        std::path::Path::new(&identity_dir),
        "echo",
        "echo-android",
    )?;

    // Two worker threads: one for the receive loop, one for everything else
    // (the control tunnel's retransmission timers and the STUN keepalive). More
    // would only add scheduler churn on a phone.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("echo-net")
        .build()
        .map_err(|e| format!("start runtime: {e}"))?;

    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let queue = Arc::new(FrameQueue::new());

    let session = runtime.spawn({
        let queue = queue.clone();
        async move {
            let mut progress = ChannelProgress(event_tx);
            let outcome =
                run_session(&identity, connect, stream, &queue, &mut progress, stop_rx, input_rx)
                    .await;
            if let Err(message) = outcome {
                progress.raw(serde_json::json!({"type": "error", "message": message}));
            }
            // Whatever happened, the feeder thread must stop waiting.
            queue.close();
            progress.raw(serde_json::json!({"type": "closed"}));
        }
    });

    let handle = Box::new(EchoHandle {
        magic: MAGIC,
        runtime: Some(runtime),
        events: Mutex::new(event_rx),
        frames: queue,
        stop: stop_tx,
        input: Some(input_tx),
        session: Mutex::new(Some(session)),
    });
    Ok(Box::into_raw(handle) as jlong)
}

/// Build the runtime, run the pairing handshake, and hand back a boxed handle.
///
/// Shares [`EchoHandle`] with `nativeConnect` so Kotlin polls one event stream
/// and closes one kind of handle. The frame queue goes unused here, which costs
/// an empty `VecDeque` and buys the app a single lifecycle to reason about.
fn start_pairing(config_json: &str) -> Result<jlong, String> {
    let cfg: serde_json::Value =
        serde_json::from_str(config_json).map_err(|e| format!("config is not valid JSON: {e}"))?;

    let identity_dir = str_field(&cfg, "identity_dir")?;
    let host = str_field(&cfg, "host")?;
    let device_name = cfg
        .get("device_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Echo Android")
        .to_string();
    let consent = Duration::from_secs(
        cfg.get("consent_secs").and_then(|v| v.as_u64()).unwrap_or(180),
    );
    // The PIN is generated here rather than accepted from the app: it must come
    // from the OS CSPRNG, and a caller-supplied one is exactly how that
    // guarantee gets quietly lost.
    let pin = pairing::generate_pin();

    let identity =
        Identity::load_or_create_rsa2048(std::path::Path::new(&identity_dir), "echo", "echo-android")?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .thread_name("echo-pair")
        .build()
        .map_err(|e| format!("start runtime: {e}"))?;

    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (stop_tx, _stop_rx) = tokio::sync::watch::channel(false);
    let queue = Arc::new(FrameQueue::new());

    let session = runtime.spawn({
        let queue = queue.clone();
        async move {
            let opts = pairing::PairOptions {
                device_name,
                consent_timeout: consent,
                ..pairing::PairOptions::new(host.clone(), pin)
            };
            let tx = event_tx.clone();
            let outcome = pairing::pair(&identity, &opts, &mut |e: pairing::PairEvent| {
                let _ = tx.send(e.to_json().to_string());
            })
            .await;

            match outcome {
                Ok(paired) => {
                    // `paired=1` means the handshake finished, not that the
                    // trust store entry works. Confirming over the port that
                    // actually enforces it turns "paired but unusable" into a
                    // distinct, diagnosable outcome.
                    match pairing::verify_paired(&identity, &host, &paired.cert_der).await {
                        Ok(()) => {
                            let _ = event_tx.send(
                                serde_json::json!({
                                    "type": "verified",
                                    "fingerprint": paired.fingerprint,
                                })
                                .to_string(),
                            );
                        }
                        Err(e) => {
                            let _ = event_tx.send(
                                serde_json::json!({"type": "warning", "message": e}).to_string(),
                            );
                        }
                    }
                }
                Err(message) => {
                    let _ = event_tx
                        .send(serde_json::json!({"type": "error", "message": message}).to_string());
                }
            }
            queue.close();
            let _ = event_tx.send(serde_json::json!({"type": "closed"}).to_string());
        }
    });

    let handle = Box::new(EchoHandle {
        magic: MAGIC,
        runtime: Some(runtime),
        events: Mutex::new(event_rx),
        frames: queue,
        stop: stop_tx,
        // Pairing accepts no input; a `None` here makes `nativeSendInput`
        // refuse rather than silently discard on a pairing handle.
        input: None,
        // Pairing has nothing to say goodbye to, but sharing the handle type
        // means sharing the orderly close path.
        session: Mutex::new(Some(session)),
    });
    Ok(Box::into_raw(handle) as jlong)
}

/// Punch, then stream. Split out so the spawn body stays readable and every
/// failure funnels into one `error` event.
async fn run_session(
    identity: &Identity,
    connect: ConnectOptions,
    stream: StreamOptions,
    queue: &Arc<FrameQueue>,
    progress: &mut ChannelProgress,
    stop: tokio::sync::watch::Receiver<bool>,
    input_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
) -> Result<(), String> {
    let path: OpenPath = session::open_path(identity, &connect, progress).await?;
    let mut sink = QueueSink(queue.clone());
    session::stream(
        identity,
        &connect.host_fingerprint,
        path,
        &stream,
        &mut sink,
        progress,
        stop,
        Some(input_rx),
    )
    .await?;
    Ok(())
}

fn str_field(value: &serde_json::Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("config is missing the string field `{key}`"))
}

/// Collapse a `catch_unwind` result into the same error channel as everything
/// else, so a panic crossing the boundary becomes a Java exception rather than
/// an abort that takes the whole app down with it.
fn flatten<T>(
    result: std::thread::Result<Result<T, String>>,
) -> Result<T, String> {
    match result {
        Ok(inner) => inner,
        Err(payload) => Err(match payload.downcast_ref::<&str>() {
            Some(s) => format!("panic in native code: {s}"),
            None => match payload.downcast_ref::<String>() {
                Some(s) => format!("panic in native code: {s}"),
                None => "panic in native code".to_string(),
            },
        }),
    }
}

fn throw(env: &mut JNIEnv, message: &str) {
    // If throwing itself fails there is an exception pending already, which is
    // the outcome we wanted anyway.
    let _ = env.throw_new("java/lang/RuntimeException", message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_missing_a_required_field_is_refused_before_anything_starts() {
        // No runtime, no socket, no identity written: the error must come from
        // parsing, so a typo in the app's config cannot half-start a session.
        let err = start_session(r#"{"relay_url":"https://x/v1/signal"}"#).unwrap_err();
        assert!(err.contains("identity_dir"), "unexpected error: {err}");
    }

    #[test]
    fn malformed_json_is_reported_as_such() {
        let err = start_session("not json at all").unwrap_err();
        assert!(err.contains("not valid JSON"), "unexpected error: {err}");
    }

    #[test]
    fn a_null_or_fabricated_handle_is_refused_rather_than_dereferenced() {
        assert!(unsafe { EchoHandle::from_raw(0) }.is_none());
        // A non-null value that never came from `nativeConnect` must not be
        // trusted; the magic is what makes that check possible.
        let not_a_handle = Box::into_raw(Box::new(0u64)) as jlong;
        assert!(unsafe { EchoHandle::from_raw(not_a_handle) }.is_none());
        unsafe { drop(Box::from_raw(not_a_handle as *mut u64)) };
    }

    #[test]
    fn closing_a_null_handle_is_a_no_op() {
        // Kotlin's `close()` runs in a `finally`; it will be called on handles
        // that were never opened.
        let ptr = std::ptr::null_mut::<EchoHandle>() as jlong;
        assert_eq!(ptr, 0);
    }

    #[test]
    fn a_panic_crossing_the_boundary_becomes_an_error_not_an_abort() {
        let result: std::thread::Result<Result<(), String>> =
            std::panic::catch_unwind(|| panic!("boom"));
        let err = flatten(result).unwrap_err();
        assert!(err.contains("boom"), "the panic message should survive: {err}");
    }
}

#[cfg(test)]
mod pairing_bridge_tests {
    use super::*;

    #[test]
    fn a_pair_config_missing_the_host_is_refused_before_a_pin_is_shown() {
        // Showing a PIN and then failing would send someone to the PC for
        // nothing, so the config is validated before any event is emitted.
        let err = start_pairing(r#"{"identity_dir":"."}"#).unwrap_err();
        assert!(err.contains("host"), "unexpected error: {err}");
    }

    #[test]
    fn pair_and_connect_report_config_errors_the_same_way() {
        assert!(start_pairing("nonsense").unwrap_err().contains("not valid JSON"));
        assert!(start_session("nonsense").unwrap_err().contains("not valid JSON"));
    }
}
