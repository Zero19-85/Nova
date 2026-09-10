//! The Rust↔C++/WinRT bridge for Echo on Xbox.
//!
//! Everything here is marshalling. The connection sequence, the control
//! channel, FEC, decryption, the keyframe gate and the handover supervisor all
//! live in `echo-client`, which the desktop CLI and the Android bridge link too
//! — so this file contains no protocol logic at all, exactly like
//! `echo-android`. Every `extern "C"` function parses its arguments, calls one
//! safe Rust function, and marshals the result back.
//!
//! ## Why a flat C ABI and not WinRT
//!
//! A WinRT component would have to be authored in IDL, projected, and activated
//! through the class registry, and it would put a COM apartment between the
//! receive thread and the decoder. None of that buys anything: the app is a
//! single C++/WinRT executable that we also write. A flat ABI is what
//! `dllexport` already gives us, and it stays callable from C# should the UI
//! half ever move.
//!
//! ## The two-CRT rule — read this before adding an entry point
//!
//! `echo_xbox.dll` is built with a **statically linked CRT**; the UWP app links
//! the Store CRT (`vcruntime140_app.dll`). Two C runtimes therefore coexist in
//! the process, each with its own heap. That is safe only for as long as **no
//! allocation ever crosses the boundary**, which is why every string and every
//! buffer here is written into storage the *caller* owns:
//!
//! - Text out: `(char* out, int32_t cap)`, return = bytes written or a negative
//!   code. Never a `*mut c_char` for C++ to free.
//! - Frames out: written straight into the pointer the caller locked from an
//!   `IMFMediaBuffer`.
//! - Data in: `(const uint8_t*, int32_t)`, copied before the call returns.
//!
//! There is deliberately no `echo_free`. If a future entry point seems to need
//! one, it is the wrong shape.
//!
//! ## Pull, not push
//!
//! C++ calls *down* for frames; Rust never calls *up*. A callback would invert
//! control so the decoder's backpressure could not reach the network layer.
//! Pulling means the bounded queue in [`echo_client::frames`] *is* the
//! backpressure signal — the same design, and now literally the same code, as
//! Android.
//!
//! The frame path is one copy: C++ locks a Media Foundation input buffer and
//! passes the pointer down, [`echo_fill_buffer`] writes the frame into it.
//!
//! ## Threading contract
//!
//! - [`echo_connect`] returns **immediately**. The session runs on a Tokio
//!   runtime this library owns; progress and failures arrive as events.
//! - [`echo_poll_event`] and [`echo_fill_buffer`] block with a timeout, so they
//!   belong on worker threads — never on the XAML UI thread.
//! - [`echo_close`] is idempotent and safe to call while other threads are
//!   blocked; it wakes them.
//!
//! ## A bad handle is a return value, never a fault
//!
//! Same rule as the JNI bridge, and it was earned there: closing a session is
//! what makes a handle stop resolving, and [`echo_close`] clears the magic word
//! before it has finished tearing the runtime down — so for the whole of an
//! ordinary disconnect there is a window where a polling thread's next call
//! arrives at a handle already on its way out. Every entry point answers with
//! its own "not a session" code; none of them faults.

use std::ffi::{c_char, CStr};
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use echo_client::audio::PlayoutStep;
use echo_client::frames::{FrameQueue, QueueSink};
use echo_client::session::{self, ConnectOptions, Event, Progress, StreamOptions, Uplink};
use echo_client::{handover, input, pairing};
use nova_core::identity::Identity;

// ── Generic status codes ────────────────────────────────────────────────────
/// The call succeeded and wrote nothing.
pub const ECHO_OK: i32 = 0;
/// The handle was not a live session. The correct response is almost always to
/// stop.
pub const ECHO_BAD_HANDLE: i32 = -1;
/// An argument was null, negative, or not valid UTF-8.
pub const ECHO_BAD_ARG: i32 = -2;
/// The output buffer is too small. Nothing was written.
pub const ECHO_TOO_SMALL: i32 = -3;
/// The call failed; [`echo_last_error`] holds the reason.
pub const ECHO_FAILED: i32 = -4;

// ── Return codes for echo_fill_buffer ───────────────────────────────────────
/// No frame arrived within the timeout. Not an error — the normal state of a
/// feeder thread that is keeping up.
pub const FILL_TIMEOUT: i32 = -1;
/// The frame does not fit the supplied buffer; `meta[0]` holds the size needed.
pub const FILL_TOO_SMALL: i32 = -2;
/// The session has ended. Stop feeding.
pub const FILL_ENDED: i32 = -3;
/// The handle was invalid, or the arguments could not be read.
pub const FILL_BAD_HANDLE: i32 = -4;

// ── Return codes for echo_poll_audio ────────────────────────────────────────
// Positive means "a packet is in the buffer", so the sign alone separates the
// one case with a payload from the four without.
/// An encoded Opus packet is in the buffer; `meta[0]` holds its length.
pub const AUDIO_PACKET: i32 = 1;
/// A packet was lost in flight. Conceal one frame.
pub const AUDIO_CONCEAL: i32 = 0;
/// Nothing to play yet — the buffer is filling. Render one frame of silence.
pub const AUDIO_SILENCE: i32 = -1;
/// Nothing to play and nothing expected; the voice may idle.
pub const AUDIO_IDLE: i32 = -2;
/// The handle was invalid, or the arguments could not be read.
pub const AUDIO_BAD_HANDLE: i32 = -3;
/// The packet does not fit the supplied buffer; `meta[0]` holds the size needed.
pub const AUDIO_TOO_SMALL: i32 = -4;

/// ViGEm slots the host will plug a virtual pad into, matching `MAX_PADS` in
/// `nova-server/src/input.rs`. A snapshot for a slot at or beyond this is
/// dropped there without comment, so this side refuses it instead.
const MAX_GAMEPAD_SLOTS: i32 = 4;

/// How long [`echo_close`] waits for the session to tell the host it is done.
///
/// Long enough for one RUDP round trip including a retransmit, short enough that
/// quitting never feels stuck.
const CLOSE_GRACE: Duration = Duration::from_millis(1500);

/// Distinguishes a live handle from a stale or fabricated `uint64_t`. A double
/// close from C++ is an ordinary bug; without this it would be a use-after-free.
const MAGIC: u64 = 0xEC0_5E5_51_04;

/// The reason the last failing call failed. Process-global rather than
/// thread-local: the failures that matter come from a session task on a runtime
/// thread, not from the thread that asked.
static LAST_ERROR: Mutex<String> = Mutex::new(String::new());

fn set_last_error(message: impl Into<String>) {
    let mut slot = LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner());
    *slot = message.into();
}

/// Everything one session owns. Boxed, and its raw pointer is the `uint64_t`
/// handle C++ holds.
struct EchoHandle {
    magic: u64,
    /// `Option` so `close` can consume the runtime for an orderly shutdown while
    /// the handle itself stays valid enough to reject later calls.
    runtime: Option<tokio::runtime::Runtime>,
    events: Mutex<Receiver<String>>,
    frames: Arc<FrameQueue>,
    stop: tokio::sync::watch::Sender<bool>,
    /// Where C++ posts GameStream input packets, when a session carries them.
    input: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    /// Where C++ posts encoded microphone packets — one Opus packet each.
    ///
    /// Separate from [`Self::input`] rather than a shared uplink channel: they
    /// are produced by different threads at different rates, and merging them
    /// would put a 20 ms audio packet behind whatever input burst was queued.
    mic: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    /// Commands for the live control tunnel, one JSON envelope each.
    ///
    /// `None` on a pairing handle, which has no tunnel to carry them.
    control: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    /// Downstream game audio, scheduled for playout.
    ///
    /// Held here rather than behind a channel because C++ *pulls* from it on the
    /// audio device's clock: XAudio2 decides when the next 20 ms is needed, and
    /// the buffer answers. A channel would invert that and make the network the
    /// clock, which is how a jitter buffer stops being one.
    audio: Arc<echo_client::audio::AudioPlayout>,
    /// The session task, so `close` can wait for it to unwind.
    ///
    /// Load-bearing: `Runtime::shutdown_timeout` only waits for *blocking*
    /// tasks — async tasks are cancelled the instant the runtime drops. The
    /// session's final act is telling the host `stop_session`, so without this
    /// wait that call is killed mid-flight and the host holds the session until
    /// its idle sweep reclaims it up to a minute later.
    session: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl EchoHandle {
    /// Reconstitute a handle from C++. Returns `None` for zero or for anything
    /// that does not carry our magic.
    ///
    /// # Safety
    /// `handle` must be a value previously returned by [`echo_connect`] or
    /// [`echo_pair`] and not yet passed to [`echo_close`].
    unsafe fn from_raw<'a>(handle: u64) -> Option<&'a mut EchoHandle> {
        if handle == 0 {
            return None;
        }
        let candidate = &mut *(handle as *mut EchoHandle);
        (candidate.magic == MAGIC).then_some(candidate)
    }
}

/// Sends session events to whoever is polling, as JSON.
struct ChannelProgress(Sender<String>);

impl Progress for ChannelProgress {
    fn event(&mut self, event: Event) {
        // A closed receiver means C++ stopped polling; the session should keep
        // running regardless, so the error is deliberately ignored.
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

// ── Marshalling helpers ─────────────────────────────────────────────────────

/// Read a NUL-terminated UTF-8 argument.
///
/// # Safety
/// `ptr` must be null or point at a NUL-terminated string that outlives the
/// call. C++/WinRT callers pass `winrt::to_string(hstring).c_str()`.
unsafe fn read_str(ptr: *const c_char) -> Result<String, String> {
    if ptr.is_null() {
        return Err("null string argument".into());
    }
    CStr::from_ptr(ptr)
        .to_str()
        .map(str::to_owned)
        .map_err(|e| format!("argument is not valid UTF-8: {e}"))
}

/// Write `text` into the caller's buffer as NUL-terminated UTF-8.
///
/// Returns the byte count **excluding** the terminator, or [`ECHO_TOO_SMALL`]
/// when it will not fit — in which case nothing is written, so a caller that
/// grows and retries never sees a truncated half-string. A truncated JSON event
/// is worse than no event: it parses as garbage rather than failing.
///
/// # Safety
/// `out` must be null, or point at `cap` writable bytes.
unsafe fn write_str(out: *mut c_char, cap: i32, text: &str) -> i32 {
    if out.is_null() || cap <= 0 {
        return ECHO_BAD_ARG;
    }
    let bytes = text.as_bytes();
    if bytes.len() + 1 > cap as usize {
        return ECHO_TOO_SMALL;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), out as *mut u8, bytes.len());
    *out.add(bytes.len()) = 0;
    bytes.len() as i32
}

/// Collapse a `catch_unwind` result, recording the reason so [`echo_last_error`]
/// can report it.
///
/// A panic crossing this boundary would be undefined behaviour, so every entry
/// point funnels through here or through its own `catch_unwind`.
fn flatten<T>(result: std::thread::Result<Result<T, String>>) -> Result<T, String> {
    match result {
        Ok(inner) => {
            if let Err(message) = &inner {
                set_last_error(message.clone());
            }
            inner
        }
        Err(payload) => {
            let message = match payload.downcast_ref::<&str>() {
                Some(s) => format!("panic in native code: {s}"),
                None => match payload.downcast_ref::<String>() {
                    Some(s) => format!("panic in native code: {s}"),
                    None => "panic in native code".to_string(),
                },
            };
            set_last_error(message.clone());
            Err(message)
        }
    }
}

// ── Process setup and diagnostics ───────────────────────────────────────────

/// The reason the last failing call failed, or an empty string.
///
/// # Safety
/// `out` must point at `cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn echo_last_error(out: *mut c_char, cap: i32) -> i32 {
    let message = LAST_ERROR
        .lock()
        .map(|m| m.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    write_str(out, cap, &message)
}

/// Escalating self-test, for bringing the app up on a console the first time.
///
/// Each stage exercises one more layer of what the app container has to permit,
/// in the order those layers are most likely to be refused. Run them in sequence
/// and the first failure *names* the problem — which beats a streaming session
/// failing at an unknown depth with nothing on screen. This is the whole of
/// milestone 1: none of it is Echo, all of it is "does Rust work here".
///
/// | stage | proves | needs |
/// |---|---|---|
/// | 0 | the DLL loaded and the ABI lines up — no OS calls at all | — |
/// | 1 | the heap and the CRT work; `arg` is echoed back | — |
/// | 2 | filesystem: writes and re-reads a probe file under `arg` | LocalState path |
/// | 3 | sockets: binds UDP on a Tokio runtime, reports the local port | `internetClient` |
/// | 4 | crypto and entropy: generates an RSA-2048 identity under `arg` | LocalState path |
///
/// Stage 4 is slow (seconds) by nature — it is RSA key generation, and in a real
/// session it happens once per install.
///
/// # Safety
/// `arg` must be null or a NUL-terminated UTF-8 string; `out` must point at
/// `cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn echo_probe(stage: u32, arg: *const c_char, out: *mut c_char, cap: i32) -> i32 {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let arg = read_str(arg).unwrap_or_default();
        probe(stage, &arg)
    }));

    match flatten(result) {
        Ok(text) => write_str(out, cap, &text),
        Err(message) => {
            // Write the reason into the caller's buffer as well as recording it,
            // so a first-run probe needs exactly one call to be useful.
            let _ = write_str(out, cap, &format!("FAILED: {message}"));
            ECHO_FAILED
        }
    }
}

fn probe(stage: u32, arg: &str) -> Result<String, String> {
    match stage {
        0 => Ok(format!(
            "stage 0 ok — echo-xbox {} linked, ABI alive",
            env!("CARGO_PKG_VERSION")
        )),
        1 => Ok(format!(
            "stage 1 ok — heap and CRT: {}",
            if arg.is_empty() { "(no argument)" } else { arg }
        )),
        2 => {
            if arg.is_empty() {
                return Err("stage 2 needs a writable directory as its argument".into());
            }
            let path = std::path::Path::new(arg).join("echo-probe.txt");
            let payload = "echo probe";
            std::fs::write(&path, payload).map_err(|e| format!("write {}: {e}", path.display()))?;
            let back = std::fs::read_to_string(&path)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            std::fs::remove_file(&path).ok();
            if back != payload {
                return Err("the file read back with different contents".into());
            }
            Ok(format!("stage 2 ok — filesystem: {}", path.display()))
        }
        3 => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|e| format!("start runtime: {e}"))?;
            runtime.block_on(async {
                let socket = tokio::net::UdpSocket::bind("0.0.0.0:0")
                    .await
                    .map_err(|e| format!("bind UDP: {e}"))?;
                let local = socket.local_addr().map_err(|e| format!("local_addr: {e}"))?;
                Ok::<String, String>(format!("stage 3 ok — UDP bound on {local}"))
            })
        }
        4 => {
            if arg.is_empty() {
                return Err("stage 4 needs a writable directory as its argument".into());
            }
            let identity =
                Identity::load_or_create_rsa2048(std::path::Path::new(arg), "echo", "echo-xbox")?;
            Ok(format!("stage 4 ok — identity {}", identity.fingerprint))
        }
        other => Err(format!("no such probe stage: {other}")),
    }
}

/// This client's certificate fingerprint, generating the identity on first call.
///
/// `dir` must be app-private storage —
/// `ApplicationData::Current().LocalFolder().Path()`. It will hold a private
/// key, and Echo's identity *is* that key.
///
/// # Safety
/// `dir` must be a NUL-terminated UTF-8 string; `out` must point at `cap`
/// writable bytes. 65 is always enough.
#[no_mangle]
pub unsafe extern "C" fn echo_identity_fingerprint(dir: *const c_char, out: *mut c_char, cap: i32) -> i32 {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let dir = read_str(dir)?;
        let identity =
            Identity::load_or_create_rsa2048(std::path::Path::new(&dir), "echo", "echo-xbox")?;
        Ok::<String, String>(identity.fingerprint)
    }));

    match flatten(result) {
        Ok(fingerprint) => write_str(out, cap, &fingerprint),
        Err(_) => ECHO_FAILED,
    }
}

// ── Session lifecycle ───────────────────────────────────────────────────────

/// Start a session. Returns a handle immediately, or 0 — see [`echo_last_error`].
///
/// Failures after this point arrive as `{"type":"error"}` events rather than as
/// a zero handle, because by the time most of them can happen this call has
/// already returned. Only a malformed config or a runtime that will not start
/// fails here.
///
/// Config JSON — the CLI's and Android's own argument set, so all three
/// configure identically:
/// ```json
/// {
///   "identity_dir": "C:\\…\\LocalState",
///   "relay_url": "https://relay:8443/v1/signal",
///   "relay_pin": "<64 hex>",
///   "host_fingerprint": "<64 hex>",
///   "res": "1080p", "fps": 60, "codec": "hevc", "bitrate_kbps": 20000,
///   "app_id": 5,
///   "punch_secs": 8,
///   "lan_endpoint": "10.0.0.205:48011",
///   "wan_endpoint": "203.0.113.7:47998",
///   "lan_timeout_ms": 500
/// }
/// ```
///
/// The last three are the transport cascade and are all optional. Omitting
/// `lan_endpoint` skips the LAN attempt outright, which is what a caller with no
/// locally-observed address for this host should do — a wrong guess is paid for
/// on every connect and buys nothing.
///
/// # Safety
/// `config_json` must be a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn echo_connect(config_json: *const c_char) -> u64 {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let raw = read_str(config_json)?;
        start_session(&raw)
    }));
    flatten(result).unwrap_or(0)
}

/// Pair with a Nova host on the LAN. Returns a handle immediately; progress
/// arrives as events, exactly like [`echo_connect`].
///
/// Pairing is event-driven rather than blocking because its middle step is a
/// human: Rust generates the PIN and emits `awaiting_consent`, the app shows it
/// on the TV, and somebody walks over to the PC and types it into Nova's dialog.
/// A blocking call would give the UI nothing to display during the one part of
/// the flow that is entirely about display.
///
/// Config JSON:
/// ```json
/// { "identity_dir": "C:\\…\\LocalState", "host": "10.0.0.205",
///   "device_name": "Xbox Series X", "consent_secs": 180 }
/// ```
///
/// Must be on the LAN: this is Nova's unauthenticated HTTP port, which is
/// exactly why the handshake carries its own mutual proof. Streaming afterwards
/// works from anywhere.
///
/// # Safety
/// `config_json` must be a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn echo_pair(config_json: *const c_char) -> u64 {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let raw = read_str(config_json)?;
        start_pairing(&raw)
    }));
    flatten(result).unwrap_or(0)
}

/// The next control-plane event as JSON, or [`ECHO_OK`] with nothing written on
/// timeout.
///
/// Blocks up to `timeout_ms`, so it belongs on a worker thread. Treat
/// [`ECHO_OK`] as "nothing happened" and keep polling; a handle this no longer
/// recognises answers [`ECHO_BAD_HANDLE`], which is the end of the session.
///
/// # Safety
/// `out` must point at `cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn echo_poll_event(handle: u64, timeout_ms: i32, out: *mut c_char, cap: i32) -> i32 {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(session) = EchoHandle::from_raw(handle) else {
            return ECHO_BAD_HANDLE;
        };
        let events = session.events.lock().unwrap_or_else(|e| e.into_inner());
        match events.recv_timeout(Duration::from_millis(timeout_ms.max(0) as u64)) {
            Ok(json) => write_str(out, cap, &json),
            // The sender being gone means the session task finished. Not an
            // error — `ended` was already delivered before the channel dropped.
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => ECHO_OK,
        }
    }));
    result.unwrap_or(ECHO_BAD_HANDLE)
}

/// Copy the next frame into `dst`, waiting up to `timeout_ms`.
///
/// `dst` is in practice the pointer from `IMFMediaBuffer::Lock`, which is what
/// makes this a single copy into memory the decoder already owns.
///
/// Returns the number of bytes written, or one of the `FILL_*` codes. On success
/// `meta` receives `[size, flags, pts_100ns]`, where flags bit 0 marks a
/// keyframe — set `MFSampleExtension_CleanPoint` from it.
///
/// The presentation timestamp is derived from the wire frame index rather than a
/// clock, scaled to Media Foundation's 100-nanosecond units. It only has to be
/// monotonic for a decoder that is not doing presentation timing itself; the
/// frame index is exactly that, and it survives loss, whereas a receive-time
/// clock would encode our own jitter into the stream.
///
/// # Safety
/// `dst` must point at `cap` writable bytes and `meta` at 3 writable `int64_t`.
#[no_mangle]
pub unsafe extern "C" fn echo_fill_buffer(
    handle: u64,
    dst: *mut u8,
    cap: i32,
    meta: *mut i64,
    timeout_ms: i32,
) -> i32 {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(session) = EchoHandle::from_raw(handle) else {
            return FILL_BAD_HANDLE;
        };
        if dst.is_null() || meta.is_null() || cap <= 0 {
            return FILL_BAD_HANDLE;
        }
        let queue = session.frames.clone();

        let Some(frame) = queue.pop_timeout(Duration::from_millis(timeout_ms.max(0) as u64)) else {
            return if queue.is_closed() { FILL_ENDED } else { FILL_TIMEOUT };
        };

        let size = frame.data.len();
        if size > cap as usize {
            *meta = size as i64;
            return FILL_TOO_SMALL;
        }

        std::ptr::copy_nonoverlapping(frame.data.as_ptr(), dst, size);

        *meta = size as i64;
        *meta.add(1) = i64::from(frame.is_keyframe());
        // 100ns units at a nominal 60 fps. The decoder is told not to do
        // presentation timing (`MF_LOW_LATENCY`), so this only has to increase.
        *meta.add(2) = frame.index as i64 * 166_667;
        size as i32
    }));

    match result {
        Ok(code) => code,
        Err(_) => {
            set_last_error("panic while filling a frame buffer");
            FILL_BAD_HANDLE
        }
    }
}

/// One step of downstream audio playout, on the renderer's clock.
///
/// Returns a step code, and for [`AUDIO_PACKET`] copies the encoded Opus packet
/// into `dst` with its length in `meta[0]`.
///
/// | code | meaning | what to render |
/// |---|---|---|
/// | [`AUDIO_PACKET`] | a packet is in `dst` | decode it and submit the PCM |
/// | [`AUDIO_CONCEAL`] | a packet was lost | conceal one frame |
/// | [`AUDIO_SILENCE`] | buffer not ready | submit one frame of silence |
/// | [`AUDIO_IDLE`] | nothing expected | the voice may idle |
/// | [`AUDIO_BAD_HANDLE`] | closed or not a session | stop |
///
/// [`AUDIO_CONCEAL`] is distinct from [`AUDIO_SILENCE`] even where the caller
/// renders both as quiet: a concealed frame is a decoder's guess at audio that
/// existed, silence is the absence of any. Collapsing them makes packet loss and
/// an empty buffer sound identical and removes the only signal separating them.
///
/// # Safety
/// `dst` must point at `cap` writable bytes and `meta` at one writable `int64_t`.
#[no_mangle]
pub unsafe extern "C" fn echo_poll_audio(handle: u64, dst: *mut u8, cap: i32, meta: *mut i64) -> i32 {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(session) = EchoHandle::from_raw(handle) else {
            return AUDIO_BAD_HANDLE;
        };
        if dst.is_null() || meta.is_null() || cap <= 0 {
            return AUDIO_BAD_HANDLE;
        }
        match session.audio.next_step() {
            PlayoutStep::Packet(packet) => {
                let size = packet.len();
                if size > cap as usize {
                    *meta = size as i64;
                    return AUDIO_TOO_SMALL;
                }
                std::ptr::copy_nonoverlapping(packet.as_ptr(), dst, size);
                *meta = size as i64;
                AUDIO_PACKET
            }
            PlayoutStep::Conceal => AUDIO_CONCEAL,
            PlayoutStep::Silence => AUDIO_SILENCE,
            PlayoutStep::Idle => AUDIO_IDLE,
        }
    }));

    match result {
        Ok(code) => code,
        Err(_) => {
            set_last_error("panic while polling audio");
            AUDIO_BAD_HANDLE
        }
    }
}

/// Receive and queue statistics as JSON, for diagnostics and an on-screen
/// overlay.
///
/// # Safety
/// `out` must point at `cap` writable bytes; 1 KiB is comfortable.
#[no_mangle]
pub unsafe extern "C" fn echo_stats(handle: u64, out: *mut c_char, cap: i32) -> i32 {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(session) = EchoHandle::from_raw(handle) else {
            return ECHO_BAD_HANDLE;
        };
        let q = session.frames.stats();
        let (rtt_last, rtt_best) = session::rtt_stats();
        let mic = echo_client::mic::stats();
        // Both halves of the audio story, because either alone misattributes a
        // fault: the network counters say what arrived, the playout counters say
        // what was done with it. Popping with clean network counters is a
        // playout problem; the reverse is a path problem.
        let (play, net, highest) = session.audio.stats_or_zero();
        let json = serde_json::json!({
            // Playout side.
            "audio_rendered": play.rendered,
            "audio_concealed": play.concealed,
            // The split that matters: `underran` means the buffer ran dry while
            // the host was still sending — the fault. `paused` means the host
            // went quiet — normal, and usually the larger number.
            "audio_underran": play.underran,
            "audio_paused": play.paused,
            "audio_dropped": play.dropped_late,
            "audio_depth": play.depth,
            // Network side.
            "audio_accepted": net.accepted,
            "audio_lost": net.lost(highest),
            "audio_late": net.late,
            "mic_packets": mic.packets,
            "mic_bytes": mic.bytes,
            "mic_worst_gap_ms": mic.worst_gap_ms,
            "rtt_ms": rtt_last,
            "rtt_best_ms": rtt_best,
            "queue_depth": q.depth,
            "frames_delivered": q.delivered,
            "frames_dropped_overflow": q.dropped_overflow,
            "frames_dropped_waiting_keyframe": q.dropped_waiting_keyframe,
            "frame_age_ms": q.last_frame_age_ms,
            "worst_frame_age_ms": q.worst_frame_age_ms,
            "video_delay_ms": session.frames.delay_ms(),
        });
        write_str(out, cap, &json.to_string())
    }));
    result.unwrap_or(ECHO_BAD_HANDLE)
}

// ── Uplink ──────────────────────────────────────────────────────────────────

/// Post one input event to the host. Fire-and-forget.
///
/// `kind` selects the builder; the four integers are its arguments:
///
/// | kind | meaning | a | b | c | d |
/// |---|---|---|---|---|---|
/// | 1 | mouse move, relative | dx | dy | — | — |
/// | 2 | mouse move, absolute | x | y | width | height |
/// | 3 | mouse button | button code | 1 = down | — | — |
/// | 4 | scroll | clicks | — | — | — |
/// | 5 | key | keycode | 1 = down | modifiers | — |
/// | 6 | release everything held | — | — | — | — |
/// | 7 | touch | event | pointer id | x | y |
///
/// Returns `false` for a bad handle, a pairing handle, or an unknown kind —
/// never that the host refused it.
#[no_mangle]
pub unsafe extern "C" fn echo_send_input(handle: u64, kind: i32, a: i32, b: i32, c: i32, d: i32) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(echo) = EchoHandle::from_raw(handle) else {
            return false;
        };
        let Some(tx) = echo.input.as_ref() else {
            return false; // a pairing handle has no session to inject into
        };

        // Saturating rather than wrapping: a flick past i16 range should pin the
        // pointer's motion, not reverse it.
        let clamp = |v: i32| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;

        let packet = match kind {
            1 => input::mouse_move_relative(clamp(a), clamp(b)),
            2 => input::mouse_move_absolute(clamp(a), clamp(b), clamp(c), clamp(d)),
            3 => input::MouseButton::from_code(a as u8).map(|btn| input::mouse_button(btn, b == 1)),
            4 => input::scroll(clamp(a)),
            5 => Some(input::keyboard(a as u16, c as u8, b == 1)),
            7 => input::TouchEvent::from_code(a as u8)
                .map(|event| input::touch(b as u8, event, clamp(c), clamp(d))),
            // Queued like any other input so it cannot overtake a key-down
            // already in flight — arriving out of order would release a key
            // before it was pressed and leave it stuck, which is the exact
            // failure this exists to prevent.
            6 => return input::release_all().into_iter().all(|p| tx.send(p).is_ok()),
            _ => None,
        };

        match packet {
            Some(p) => tx.send(p).is_ok(),
            None => false,
        }
    }))
    .unwrap_or(false)
}

/// Post one controller state snapshot to the host.
///
/// Ten discrete arguments, deliberately — see `echo_client::input::gamepad`.
/// They would pack into four integers, and that packing is exactly the
/// silent-corruption class this codebase keeps paying for: a transposed shift
/// produces a controller that works *almost* right, which is far more expensive
/// to find than one that does not work at all.
///
/// `buttons` is **XInput's bit layout**, which GameStream's low 16 `buttonFlags`
/// are identical to. `Windows.Gaming.Input` does not use those bits, so the
/// translation belongs in the app — see `xbox/EchoXbox/GamepadInput.h`.
///
/// Stick axes cover the full `i16` range with **Y positive up**, matching both
/// XInput and `Windows.Gaming.Input`. Unlike Android, no negation is needed; a
/// sign flip here reads as inverted look — a preference someone forgot to expose
/// — rather than as a defect, which is what makes it worth stating.
///
/// `active_mask` is the plug, not the state: a set bit at `controller_number`
/// plugs a virtual Xbox 360 pad in host-side, a clear bit unplugs it. Slots
/// outside `0..=3` are refused, because the host drops them without comment and
/// a refusal here is visible where that is not.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn echo_send_gamepad(
    handle: u64,
    controller_number: i32,
    active_mask: i32,
    buttons: i32,
    left_trigger: i32,
    right_trigger: i32,
    left_stick_x: i32,
    left_stick_y: i32,
    right_stick_x: i32,
    right_stick_y: i32,
) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(echo) = EchoHandle::from_raw(handle) else {
            return false;
        };
        let Some(tx) = echo.input.as_ref() else {
            return false;
        };
        if !(0..MAX_GAMEPAD_SLOTS).contains(&controller_number) {
            return false;
        }

        // Saturating, never wrapping: a stick pushed past range must pin at the
        // rail, not reappear at the opposite one.
        let axis = |v: i32| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let trigger = |v: i32| v.clamp(0, u8::MAX as i32) as u8;

        let packet = input::gamepad(
            controller_number as u8,
            (active_mask & 0xFFFF) as u16,
            &input::GamepadState {
                buttons: (buttons & 0xFFFF) as u16,
                left_trigger: trigger(left_trigger),
                right_trigger: trigger(right_trigger),
                left_stick_x: axis(left_stick_x),
                left_stick_y: axis(left_stick_y),
                right_stick_x: axis(right_stick_x),
                right_stick_y: axis(right_stick_y),
            },
        );
        tx.send(packet).is_ok()
    }))
    .unwrap_or(false)
}

/// Post one encoded microphone packet — one Opus packet, no container.
///
/// Fire-and-forget for the same reason input is: a capture thread that blocks on
/// the network overruns the device's ring buffer, and audio lost at the source
/// cannot be recovered anywhere else.
///
/// **Send only raw Opus packets.** Windows ships no Opus *encoder*, so the Xbox
/// app cannot do what Android does and hand this a platform codec's output; it
/// encodes with a codec of its own. Whatever produces these must not prepend an
/// identification header, pre-skip, or any other container framing. That is not
/// hypothetical for this project — the AV1 rollout lost days to an SDK encoder
/// wrapping every frame in an IVF header while the decoder silently produced
/// nothing.
///
/// # Safety
/// `data` must point at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn echo_send_mic(handle: u64, data: *const u8, len: i32) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(session) = EchoHandle::from_raw(handle) else {
            return false;
        };
        let Some(tx) = session.mic.as_ref() else {
            return false; // a pairing handle carries no session
        };
        if data.is_null() || len <= 0 {
            return false;
        }
        let packet = std::slice::from_raw_parts(data, len as usize).to_vec();
        tx.send(packet).is_ok()
    }))
    .unwrap_or(false)
}

// ── Session controls ────────────────────────────────────────────────────────

/// Ask the host for a keyframe.
///
/// Only *flags* the request; the receive loop carries it. Under Nova's infinite
/// GOP there is no scheduled IDR, so a decoder that has lost its reference chain
/// and does not ask waits forever.
#[no_mangle]
pub unsafe extern "C" fn echo_request_idr(handle: u64) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(session) = EchoHandle::from_raw(handle) else {
            return false;
        };
        session.frames.request_keyframe();
        true
    }))
    .unwrap_or(false)
}

/// Ask the host to re-mode the display this session is watching, live.
///
/// This is what makes an Xbox behave like a monitor rather than like a fixed
/// stream: the host re-sizes the virtual display, rebinds capture and rebuilds
/// the encoder, and the new geometry arrives in the bitstream as fresh
/// VPS/SPS/PPS on the next IDR.
///
/// **`force` is sent unconditionally, and that is a claim about this client.**
/// `handle_set_display` refuses a mid-session change by default because a
/// Moonlight decoder is built once and a geometry change under it produces a
/// black frame with a green region (live 2026-08-10). Media Foundation is not
/// that decoder: an HEVC MFT answers a resolution change with
/// `MF_E_TRANSFORM_STREAM_CHANGE` and renegotiates its output type. The C++
/// side must actually handle that message — if it ever stops, this call has to
/// stop sending `force` with it.
///
/// Fire-and-forget. `false` means the handle is not a live streaming session
/// (a pairing handle has no tunnel), never that the host refused: the host's
/// answer is `accepted`, and what it does about it shows up as a format change.
#[no_mangle]
pub unsafe extern "C" fn echo_set_display(
    handle: u64,
    width: u32,
    height: u32,
    refresh_hz: u32,
    hdr: bool,
) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(session) = EchoHandle::from_raw(handle) else {
            return false;
        };
        let Some(tx) = session.control.as_ref() else {
            return false;
        };
        let envelope = serde_json::json!({
            "command": "set_display",
            "params": {
                "width": width,
                "height": height,
                "refresh_hz": refresh_hz,
                "hdr": hdr,
                "force": true,
            },
        });
        let Ok(bytes) = serde_json::to_vec(&envelope) else {
            return false;
        };
        tx.send(bytes).is_ok()
    }))
    .unwrap_or(false)
}

/// Hold video back by `delay_ms` to meet the audio device's latency. Returns the
/// delay actually applied, which is clamped.
///
/// Video is delayed to meet audio rather than audio hurried to meet video,
/// because most of the audio figure is a hardware floor and none of it is ours
/// to remove.
#[no_mangle]
pub unsafe extern "C" fn echo_set_video_delay(handle: u64, delay_ms: u32) -> u32 {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(session) = EchoHandle::from_raw(handle) else {
            return 0;
        };
        session.frames.set_delay_ms(delay_ms)
    }))
    .unwrap_or(0)
}

/// Tell the session its network moved, so the handover supervisor rebuilds the
/// path underneath a decoder that is never told about it.
///
/// On Xbox this is rarer than on a phone — a console does not walk out of Wi-Fi
/// range — but a resume from suspend, an Ethernet cable, or a router reboot all
/// produce it, and a resume is the common one. Returns the new epoch, or 0 for a
/// handle that is not a live session.
#[no_mangle]
pub unsafe extern "C" fn echo_network_changed(handle: u64) -> u64 {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        // The epoch is process-global, so this does not strictly need a handle.
        // It takes one anyway: an idle app resuming from suspend should not bump
        // an epoch no session is watching, and a `0` return says plainly that
        // nothing was signalled.
        let Some(_session) = EchoHandle::from_raw(handle) else {
            return 0;
        };
        handover::network_changed()
    }))
    .unwrap_or(0)
}

/// End the session and free the handle. Idempotent, and safe on 0.
///
/// Wakes every blocked caller, waits [`CLOSE_GRACE`] for the session's goodbye
/// to reach the host, then drops the runtime.
#[no_mangle]
pub unsafe extern "C" fn echo_close(handle: u64) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(session) = EchoHandle::from_raw(handle) else {
            return;
        };
        // Clear the magic first: from here on every other entry point answers
        // "not a session" rather than racing the teardown.
        session.magic = 0;
        let mut boxed = Box::from_raw(handle as *mut EchoHandle);

        let _ = boxed.stop.send(true);
        boxed.frames.close();

        // Wait for the session task to unwind so its `stop_session` reaches the
        // host. Dropping the runtime instead cancels it mid-flight, and the host
        // then holds the display until its idle sweep reclaims it.
        let task = boxed.session.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let (Some(task), Some(runtime)) = (task, boxed.runtime.as_ref()) {
            let _ = runtime.block_on(tokio::time::timeout(CLOSE_GRACE, task));
        }

        if let Some(runtime) = boxed.runtime.take() {
            // Bounded rather than indefinite: teardown must not be able to hang
            // the thread that called close().
            runtime.shutdown_timeout(Duration::from_secs(2));
        }
    }));
}

/// Leave the session **without telling the host it is over**. Idempotent, and
/// safe on 0.
///
/// This is [`echo_close`] with one thing deliberately left out, and the omission
/// is the entire feature. `close` waits [`CLOSE_GRACE`] for the session task to
/// unwind so its `stop_session` reaches the host, and the host answers that by
/// tearing the session down and handing back the display it was driving.
/// `detach` instead drops the runtime out from under the task, so no goodbye is
/// ever sent: the host sees the client go quiet, takes the
/// `detach_on_disconnect` path, and **holds the virtual display for its detach
/// grace period** so a reconnect walks back into the same desktop with the same
/// windows on it.
///
/// The client's `stop` watch channel is deliberately NOT signalled here. It
/// would be the tidy way to wake the task, and it is exactly wrong: signalling
/// asks the task to shut down gracefully, and a graceful shutdown is what sends
/// the goodbye. Whether it won the race with the runtime drop would decide
/// whether the user's monitor came back — so the race is removed rather than
/// tuned. Blocked callers are woken anyway: the frame queue is closed
/// explicitly, and the event channel's sender dies with the task.
///
/// The counterpart is [`echo_release`], which is how a detached session is
/// finally ended. The two exist as a pair on purpose — a client that can only
/// end a session can never leave one running, and a client that can only leave
/// can never let a monitor go.
///
/// # Safety
/// Same contract as [`echo_close`]: `handle` must be a value returned by
/// [`echo_connect`] or [`echo_pair`] and not yet closed or detached.
#[no_mangle]
pub unsafe extern "C" fn echo_detach(handle: u64) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(session) = EchoHandle::from_raw(handle) else {
            return;
        };
        // Clear the magic first, exactly as close does: from here on every other
        // entry point answers "not a session" rather than racing the teardown.
        session.magic = 0;
        let mut boxed = Box::from_raw(handle as *mut EchoHandle);

        // The one wake-up that is safe to perform: it unblocks C++'s feeder
        // thread and says nothing to the host.
        boxed.frames.close();

        // Dropped, never awaited. See the note above.
        drop(boxed.session.lock().unwrap_or_else(|e| e.into_inner()).take());

        if let Some(runtime) = boxed.runtime.take() {
            // Shorter than close's two seconds because nothing is being waited
            // FOR here — this only bounds the blocking tasks' own teardown.
            runtime.shutdown_timeout(Duration::from_millis(500));
        }
    }));
}

/// Ask the host to end whatever session it is holding for this device.
///
/// **Blocking**, unlike every other entry point here, and deliberately: it is a
/// single request-response with no ongoing state, so a handle and a poll loop
/// would be machinery around nothing. Call it from a worker thread and show the
/// returned string.
///
/// This exists because a session outlives the app that started it: an app the
/// console suspended, or that lost the network, never sent `stop_session`, so
/// the host detached and is holding the display. **Ending a session never needed
/// a session** — the version that started one and immediately stopped it raced
/// itself and took several presses to land.
///
/// Config JSON is the same shape [`echo_connect`] takes; the stream fields are
/// simply unused.
///
/// # Safety
/// `config_json` must be a NUL-terminated UTF-8 string; `out` must point at
/// `cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn echo_release(config_json: *const c_char, out: *mut c_char, cap: i32) -> i32 {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let raw = read_str(config_json)?;
        release_blocking(&raw)
    }));

    match flatten(result) {
        Ok(text) => write_str(out, cap, &text),
        Err(_) => ECHO_FAILED,
    }
}

// ── Plumbing ────────────────────────────────────────────────────────────────

/// Build the runtime, spawn the session, and hand back a boxed handle.
fn start_session(config_json: &str) -> Result<u64, String> {
    let cfg: serde_json::Value =
        serde_json::from_str(config_json).map_err(|e| format!("config is not valid JSON: {e}"))?;

    let identity_dir = str_field(&cfg, "identity_dir")?;
    let connect = connect_options(&cfg)?;

    // Unbounded, and that is a considered choice: the producer is a UI or input
    // thread, so any bound would mean either blocking it or silently discarding
    // input. The consumer only forwards to the control channel, and a session
    // that stops draining is ending anyway.
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    // Unbounded for the same reason, with one difference worth stating: the
    // microphone producer is a capture thread that must never block.
    let (mic_tx, mic_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    // Mid-stream control commands, as JSON envelopes. Unbounded like the other
    // two, but for a different reason: this one carries at most a few items per
    // session, and a bound would only add a failure mode to a channel that is
    // never under pressure.
    let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    let defaults = StreamOptions::default();
    let stream = StreamOptions {
        res: cfg.get("res").and_then(|v| v.as_str()).unwrap_or(&defaults.res).to_string(),
        fps: cfg.get("fps").and_then(|v| v.as_u64()).unwrap_or(defaults.fps as u64) as u32,
        codec: cfg.get("codec").and_then(|v| v.as_str()).unwrap_or(&defaults.codec).to_string(),
        bitrate_kbps: cfg
            .get("bitrate_kbps")
            .and_then(|v| v.as_u64())
            .unwrap_or(defaults.bitrate_kbps as u64) as u32,
        // Which app the session opens into. Absent means Desktop, matching the
        // host's own default.
        app_id: cfg.get("app_id").and_then(|v| v.as_u64()).unwrap_or(defaults.app_id as u64) as u32,
        // The LAN debug door is deliberately not exposed to the app: the host
        // refuses that port from non-private addresses, so offering it would
        // only produce confusing failures.
        control: None,
        // Owned by the handover supervisor, which raises it before it interrupts
        // an attempt so the exit is SILENT rather than a goodbye. A goodbye here
        // tells the host to tear the session down, which is the one thing a
        // reconnect needs not to have happened.
        detach_on_exit: Default::default(),
    };

    let identity =
        Identity::load_or_create_rsa2048(std::path::Path::new(&identity_dir), "echo", "echo-xbox")?;

    // Two worker threads: one for the receive loop, one for everything else (the
    // control tunnel's retransmission timers and the STUN keepalive). A console
    // has cores to spare, but the work is one socket and a handful of timers —
    // more threads would buy scheduler churn and nothing else.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("echo-net")
        .build()
        .map_err(|e| format!("start runtime: {e}"))?;

    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let queue = Arc::new(FrameQueue::new());
    let audio = Arc::new(echo_client::audio::AudioPlayout::new());

    let session = runtime.spawn({
        let queue = queue.clone();
        let audio = audio.clone();
        async move {
            let mut progress = ChannelProgress(event_tx);
            let uplink = Uplink {
                input: Some(input_rx),
                mic: Some(mic_rx),
                audio: Some(audio),
                control: Some(control_rx),
            };
            let outcome =
                run_session(&identity, connect, stream, &queue, &mut progress, stop_rx, uplink)
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
        mic: Some(mic_tx),
        control: Some(control_tx),
        audio,
        session: Mutex::new(Some(session)),
    });
    Ok(Box::into_raw(handle) as u64)
}

/// Build the runtime, run the pairing handshake, and hand back a boxed handle.
///
/// Shares [`EchoHandle`] with [`echo_connect`] so the app polls one event stream
/// and closes one kind of handle. The frame queue goes unused here, which costs
/// an empty `VecDeque` and buys one lifecycle to reason about.
fn start_pairing(config_json: &str) -> Result<u64, String> {
    let cfg: serde_json::Value =
        serde_json::from_str(config_json).map_err(|e| format!("config is not valid JSON: {e}"))?;

    let identity_dir = str_field(&cfg, "identity_dir")?;
    let host = str_field(&cfg, "host")?;
    let device_name = cfg
        .get("device_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Echo Xbox")
        .to_string();
    let consent =
        Duration::from_secs(cfg.get("consent_secs").and_then(|v| v.as_u64()).unwrap_or(180));
    // The PIN is generated here rather than accepted from the app: it must come
    // from the OS CSPRNG, and a caller-supplied one is exactly how that
    // guarantee gets quietly lost.
    let pin = pairing::generate_pin();

    let identity =
        Identity::load_or_create_rsa2048(std::path::Path::new(&identity_dir), "echo", "echo-xbox")?;

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
                    // `paired=1` means the handshake finished, not that the trust
                    // store entry works. Confirming over the port that actually
                    // enforces it turns "paired but unusable" into a distinct,
                    // diagnosable outcome.
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
        // Pairing accepts no input and no audio; `None` makes the uplink entry
        // points refuse rather than silently discard on a pairing handle.
        input: None,
        mic: None,
        control: None,
        // Never armed, so it answers `Idle` to every poll. An audio thread that
        // outlives a stream and lands on a pairing handle renders silence rather
        // than faulting.
        audio: Arc::new(echo_client::audio::AudioPlayout::new()),
        session: Mutex::new(Some(session)),
    });
    Ok(Box::into_raw(handle) as u64)
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
    uplink: Uplink,
) -> Result<(), String> {
    // The supervisor, not `open_path`+`stream` directly — same as Android. A
    // console's network is steadier than a phone's, but a suspend/resume moves it
    // just as surely, and the supervisor is what keeps the decoder alive across
    // that.
    let relay = handover::UplinkRelay::spawn(uplink);
    let mut sink = QueueSink(queue.clone());
    let outcome = handover::supervise(
        identity,
        &connect,
        &stream,
        &mut sink,
        progress,
        stop,
        || {
            // Runs once per attempt, which makes it the hook for "this decoder is
            // about to be fed a different session". The queue is not closed and
            // the gate is not closed — the picture on screen stays, and
            // `request_keyframe` only *flags*: a chain that survived costs one
            // redundant IDR, while a chain that did not and was never asked about
            // is a permanent freeze.
            queue.request_keyframe();
            relay.next()
        },
        handover::HandoverPolicy::default(),
    )
    .await;

    match outcome {
        handover::Outcome::Stopped(_) => Ok(()),
        // Reported as the *last* reason, not the first. The earlier ones are why
        // it kept trying; this one is why it stopped, and it is the only one a
        // user can act on.
        //
        // A refusal is not a failure to reach the host — it is the host, reached
        // and answering. Reporting it as "could not get back to the host"
        // describes the wrong problem and sends the user to look at their
        // network, which is the one thing that is working.
        handover::Outcome::GaveUp { last: last @ handover::Interruption::Refused { .. }, .. } => {
            Err(last.to_string())
        }
        handover::Outcome::GaveUp { last, attempts } => {
            Err(format!("could not get back to the host after {attempts} attempts: {last}"))
        }
    }
}

fn release_blocking(config_json: &str) -> Result<String, String> {
    let cfg: serde_json::Value =
        serde_json::from_str(config_json).map_err(|e| format!("config is not valid JSON: {e}"))?;

    let identity_dir = str_field(&cfg, "identity_dir")?;
    let host_fingerprint = str_field(&cfg, "host_fingerprint")?;
    // The same cascade as a streaming connect, and for the same reason: ending a
    // session held on a host two feet away should not need the internet.
    let connect = connect_options(&cfg)?;

    let identity =
        Identity::load_or_create_rsa2048(std::path::Path::new(&identity_dir), "echo", "echo-xbox")?;

    // Single-threaded: this does one punch and one round trip, then ends.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("start runtime: {e}"))?;

    // Progress goes nowhere: there is no poll loop on a blocking call, and the
    // outcome is the return value. Events are still generated so the shared path
    // stays identical to the streaming one.
    struct Discard;
    impl Progress for Discard {
        fn event(&mut self, _event: Event) {}
    }

    runtime.block_on(async move {
        let path = session::open_path(&identity, &connect, &mut Discard).await?;
        session::release(&identity, &host_fingerprint, path, &mut Discard).await?;
        Ok::<String, String>("the host released its session".to_string())
    })
}

fn str_field(value: &serde_json::Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("config is missing the string field `{key}`"))
}

fn connect_options(cfg: &serde_json::Value) -> Result<ConnectOptions, String> {
    let optional = |key: &str| {
        cfg.get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Ok(ConnectOptions {
        relay_url: str_field(cfg, "relay_url")?,
        relay_pin: str_field(cfg, "relay_pin")?,
        host_fingerprint: str_field(cfg, "host_fingerprint")?,
        punch_timeout: Duration::from_secs(
            cfg.get("punch_secs").and_then(|v| v.as_u64()).unwrap_or(8),
        ),
        lan_endpoint: optional("lan_endpoint"),
        wan_endpoint: optional("wan_endpoint"),
        lan_timeout: cfg
            .get("lan_timeout_ms")
            .and_then(|v| v.as_u64())
            .map(Duration::from_millis)
            .unwrap_or(session::DEFAULT_LAN_TIMEOUT),
    })
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
    fn malformed_json_is_reported_as_such_by_both_entry_points() {
        assert!(start_session("not json at all").unwrap_err().contains("not valid JSON"));
        assert!(start_pairing("nonsense").unwrap_err().contains("not valid JSON"));
    }

    #[test]
    fn a_pair_config_missing_the_host_is_refused_before_a_pin_is_shown() {
        // Showing a PIN and then failing would send someone to the PC for
        // nothing, so the config is validated before any event is emitted.
        let err = start_pairing(r#"{"identity_dir":"."}"#).unwrap_err();
        assert!(err.contains("host"), "unexpected error: {err}");
    }

    #[test]
    fn a_zero_or_fabricated_handle_is_refused_rather_than_dereferenced() {
        assert!(unsafe { EchoHandle::from_raw(0) }.is_none());
        // A non-zero value that never came from `echo_connect` must not be
        // trusted; the magic is what makes that check possible.
        let not_a_handle = Box::into_raw(Box::new(0u64)) as u64;
        assert!(unsafe { EchoHandle::from_raw(not_a_handle) }.is_none());
        unsafe { drop(Box::from_raw(not_a_handle as *mut u64)) };
    }

    #[test]
    fn every_entry_point_answers_a_bad_handle_instead_of_faulting() {
        // The contract the whole C++ side is written against. A stale handle is
        // routine during teardown, so none of these may fault.
        let mut meta = [0i64; 3];
        let mut buf = [0u8; 16];
        unsafe {
            assert_eq!(
                echo_fill_buffer(0, buf.as_mut_ptr(), 16, meta.as_mut_ptr(), 0),
                FILL_BAD_HANDLE
            );
            assert_eq!(echo_poll_audio(0, buf.as_mut_ptr(), 16, meta.as_mut_ptr()), AUDIO_BAD_HANDLE);
            assert!(!echo_send_input(0, 1, 0, 0, 0, 0));
            assert!(!echo_send_gamepad(0, 0, 1, 0, 0, 0, 0, 0, 0, 0));
            assert!(!echo_send_mic(0, buf.as_ptr(), 4));
            assert!(!echo_request_idr(0));
            assert_eq!(echo_set_video_delay(0, 40), 0);
            // C++ holds 0 whenever it is idle, and calls close() from a
            // destructor that cannot know whether connect ever succeeded.
            echo_close(0);
            // Same contract, and the same caller shape: the overlay's "leave the
            // stream" runs whether or not a session was ever open, and a second
            // press must be a no-op rather than a second free.
            echo_detach(0);
        }
    }

    #[test]
    fn a_gamepad_slot_the_host_would_silently_drop_is_refused_here() {
        // The host ignores slots at or beyond MAX_PADS without comment, so
        // sending one is input that vanishes. It must be a visible `false`.
        unsafe {
            assert!(!echo_send_gamepad(0, MAX_GAMEPAD_SLOTS, 1, 0, 0, 0, 0, 0, 0, 0));
        }
    }

    #[test]
    fn strings_are_written_into_the_callers_buffer_or_refused_whole() {
        let mut buf = [0 as c_char; 8];
        unsafe {
            assert_eq!(write_str(buf.as_mut_ptr(), 8, "abc"), 3);
            assert_eq!(buf[3], 0, "the terminator is ours to write, not the caller's");
            // Eight bytes plus a terminator does not fit in eight, and a
            // truncated JSON event parses as garbage rather than failing.
            assert_eq!(write_str(buf.as_mut_ptr(), 8, "12345678"), ECHO_TOO_SMALL);
            assert_eq!(write_str(std::ptr::null_mut(), 8, "x"), ECHO_BAD_ARG);
        }
    }

    #[test]
    fn the_probe_stages_that_touch_nothing_always_answer() {
        assert!(probe(0, "").unwrap().contains("stage 0 ok"));
        assert!(probe(1, "hello").unwrap().contains("hello"));
        assert!(probe(99, "").is_err());
        // Stage 2 needs somewhere to write, and saying so beats an io error
        // about a path that is the empty string.
        assert!(probe(2, "").unwrap_err().contains("directory"));
    }
}
