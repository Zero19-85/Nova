// GameStream audio path: WASAPI loopback capture (C++ shim) → 5ms Opus frames
// → optional AES-128-CBC encryption (rikey from /launch) → RTP on port 48000.
//
// Wire format matches Sunshine's audioBroadcastThread (stream.cpp:1595):
//   [12B RTP: 0x80, PT=97, seq BE u16, timestamp BE u32, ssrc=0] + opus payload
// timestamp advances by packetDuration (ms units) per packet; the IV for CBC
// is 16 zero bytes with BE(rikeyid + seq) in the first 4.
// Audio FEC shards (PT=127, RS(4,2)) are NOT sent yet — Moonlight treats them
// as optional; a lost packet is a 5ms dropout.
//
// ## Ownership model (Phase 15.1 — single owner)
//
// This module is the SOLE owner of:
//   1. the default render endpoint's session state (what was default before the
//      stream, and restoring it afterwards) — [`ORIGINAL_ENDPOINT`];
//   2. the sink swap (default → virtual sink for client-only audio) — `SinkGuard`;
//   3. the shim's process-global WASAPI capture state — guarded by
//      [`SHIM_CAPTURE_ACTIVE`] so `InitAudioCapture`/`CleanupAudio` can never
//      overlap across sessions.
//
// Previously `virtual_display.rs` ALSO cached and restored the default endpoint
// (`saved_audio_endpoint`), giving the device two independent owners. When the
// VDD's cache ran while the sink swap was engaged (the `/resume`
// "already active" path, or a zombie-session overlap), it captured the VIRTUAL
// SINK as the "real" endpoint and later restored the system to the sink — host
// stuck silent. That code is gone; `virtual_display`'s emergency path now calls
// [`emergency_restore_default_endpoint`] here instead.
//
// The reason the restore target must be captured BEFORE display activation (not
// at sink-swap time): when the VDD becomes primary, Windows can auto-flip the
// default endpoint to the HDMI audio device that appears with it. Capturing at
// swap time would arm the HDMI endpoint as the "original". Hence
// [`arm_endpoint_restore`] — idempotent, called by lib.rs before
// `activate_for_stream` and again (as a no-op fallback) by
// [`AudioCaptureManager::start_for_stream`].

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use aes::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
use audiopus::coder::Encoder as OpusEncoder;
use audiopus::{Application, Bitrate, Channels, SampleRate};

extern "C" {
    fn InitAudioCapture(
        device_id: *const u16,
        out_sample_rate: *mut u32,
        out_channels: *mut u16,
        out_bits_per_sample: *mut u16,
    ) -> i32;
    fn CaptureAudioFrame(out_buffer: *mut u8, max_bytes: i32, out_frames: *mut u32) -> i32;
    fn CleanupAudio();
    fn GetDefaultAudioDeviceId(out_id: *mut u16, cch: i32) -> i32;
    fn FindVirtualAudioSink(out_id: *mut u16, cch: i32) -> i32;
    fn FindRealAudioDevice(out_id: *mut u16, cch: i32) -> i32;
    fn SetDefaultAudioDevice(device_id: *const u16) -> i32;
}

/// Crash recovery: if Nova exited without restoring the default audio device
/// (process killed, console window closed, or any path that skips Rust
/// destructors), the virtual sink is left as the system default and the host
/// stays silent even with no client connected. Call once at startup — if the
/// default is currently the virtual sink, switch back to a real output device.
///
/// Also the live-query fallback for every restore path: idempotent and quiet
/// when the default is already a real device.
pub fn recover_stuck_sink() {
    let mut sink = [0u16; DEVICE_ID_CCH];
    if unsafe { FindVirtualAudioSink(sink.as_mut_ptr(), DEVICE_ID_CCH as i32) } != 0 {
        return; // no virtual sink installed — nothing to recover
    }
    let mut cur = [0u16; DEVICE_ID_CCH];
    if unsafe { GetDefaultAudioDeviceId(cur.as_mut_ptr(), DEVICE_ID_CCH as i32) } != 0 {
        return;
    }
    if wide_id(&cur) != wide_id(&sink) {
        return; // default is already a real device
    }

    let mut real = [0u16; DEVICE_ID_CCH];
    if unsafe { FindRealAudioDevice(real.as_mut_ptr(), DEVICE_ID_CCH as i32) } != 0 {
        eprintln!("⚠️  Audio: default output is the virtual sink (from a previous unclean exit) and no real device was found to restore — check Windows sound settings");
        return;
    }
    if unsafe { SetDefaultAudioDevice(wide_id(&real).as_ptr()) } == 0 {
        println!("🔊 Audio: recovered from a previous unclean exit — default output restored to host speakers");
    } else {
        eprintln!("⚠️  Audio: found a real output device but failed to restore it as default — check Windows sound settings");
    }
}

const DEVICE_ID_CCH: usize = 512;

/// Truncate a wide-string buffer at its NUL terminator.
fn wide_id(buf: &[u16]) -> Vec<u16> {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let mut v = buf[..len].to_vec();
    v.push(0);
    v
}

// ── Single-owner default-endpoint session state ───────────────────────────────

/// The default render endpoint as it was BEFORE the current streaming session
/// mutated any audio or display state (NUL-terminated UTF-16 device id).
///
/// Armed by [`arm_endpoint_restore`], claimed (take-once) by
/// [`restore_original_endpoint`]. Whichever restore path runs first — normal
/// session stop, manager drop, or the emergency shutdown handler — performs the
/// restore; every later caller finds `None` and falls back to the idempotent
/// live-query recovery. This is the ONLY place in the process that remembers
/// the pre-stream endpoint (see module docs for the dual-ownership bug this
/// replaces).
static ORIGINAL_ENDPOINT: Mutex<Option<Vec<u16>>> = Mutex::new(None);

/// Poison-proof lock: the mutex only guards a `take`/`store` of a small vec, so
/// on poisoning (a panic while held) the inner value is still coherent — and the
/// emergency shutdown path must never abort on a poisoned lock.
fn lock_original() -> MutexGuard<'static, Option<Vec<u16>>> {
    ORIGINAL_ENDPOINT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Captures the CURRENT default render endpoint as this session's restore
/// target. Idempotent — a no-op if a target is already armed, so the earliest
/// caller wins.
///
/// MUST be called before `VirtualDisplay::activate_for_stream`: when the VDD
/// becomes primary, Windows can auto-flip the default endpoint to the HDMI
/// audio device that appears with it, and arming after that would capture the
/// HDMI endpoint instead of the user's real speakers. lib.rs arms at both
/// activation sites; [`AudioCaptureManager::start_for_stream`] arms again as
/// the fallback for sessions that never activate a virtual display.
///
/// Refuses to arm the virtual sink itself (possible after an unclean previous
/// exit): restoring "back" to the sink would be the exact stuck-silent bug this
/// module exists to prevent. Left unarmed, the restore path falls back to
/// [`recover_stuck_sink`]'s live-query recovery instead.
pub fn arm_endpoint_restore() {
    let mut slot = lock_original();
    if slot.is_some() {
        return; // already armed for this session — earliest capture wins
    }

    let mut cur = [0u16; DEVICE_ID_CCH];
    if unsafe { GetDefaultAudioDeviceId(cur.as_mut_ptr(), DEVICE_ID_CCH as i32) } != 0 {
        eprintln!("⚠️  Audio: could not query the current default output — restore-on-stop will use live recovery");
        return;
    }
    let cur_id = wide_id(&cur);

    let mut sink = [0u16; DEVICE_ID_CCH];
    if unsafe { FindVirtualAudioSink(sink.as_mut_ptr(), DEVICE_ID_CCH as i32) } == 0
        && wide_id(&sink) == cur_id
    {
        println!("⚠️  Audio: default output is currently the virtual sink — not arming it as the restore target (restore will pick a real device)");
        return;
    }

    println!("🔊 Audio: armed endpoint restore (pre-stream default output captured)");
    *slot = Some(cur_id);
}

/// Claim-once restore of the armed pre-stream endpoint.
///
/// If nothing is armed (never captured, or an earlier path already claimed it),
/// falls back to [`recover_stuck_sink`] — which heals the "virtual sink is
/// still the default" state and is a quiet no-op otherwise. Every stop path
/// funnels here, so a session can never end with the sink left as default.
fn restore_original_endpoint() {
    match lock_original().take() {
        Some(id) => {
            if unsafe { SetDefaultAudioDevice(id.as_ptr()) } == 0 {
                println!("🔊 Audio: default output restored to the pre-stream endpoint");
            } else {
                eprintln!("⚠️  Audio: failed to restore the pre-stream endpoint — attempting live recovery");
                recover_stuck_sink();
            }
        }
        None => recover_stuck_sink(),
    }
}

/// Synchronous endpoint restore for process-death paths (console close, logoff,
/// shutdown, WM_ENDSESSION). Called by
/// `virtual_display::emergency_restore_for_shutdown` — same claim-once
/// semantics as the normal stop, so whichever runs first wins and the other
/// no-ops. Safe to call with no session active.
pub fn emergency_restore_default_endpoint() {
    restore_original_endpoint();
}

// ── Sink routing ──────────────────────────────────────────────────────────────

/// Client-only audio routing (Sunshine's approach — no muting involved):
/// switch the default render device to a virtual sink (Steam Streaming
/// Speakers / VB-CABLE) so application audio routes into it instead of the
/// physical speakers, loopback-capture that sink, and restore the pre-stream
/// endpoint on drop (including panics and every send-loop exit path). The
/// host endpoint's volume/mute state is never touched.
///
/// The guard does NOT remember what to restore — that is [`ORIGINAL_ENDPOINT`]'s
/// job (armed before any display/audio mutation). The guard's drop simply
/// triggers the claim-once restore.
struct SinkGuard {
    /// Device to loopback-capture from (None = current default endpoint).
    capture_id: Option<Vec<u16>>,
}

impl SinkGuard {
    fn engage(host_audio: bool) -> Self {
        if host_audio {
            println!("🔊 Audio: localAudioPlayMode=1 — playing on host speakers and streaming to client");
            return Self { capture_id: None };
        }

        let mut sink = [0u16; DEVICE_ID_CCH];
        let found = unsafe { FindVirtualAudioSink(sink.as_mut_ptr(), DEVICE_ID_CCH as i32) };
        if found != 0 {
            eprintln!("⚠️  Audio: no virtual sink found (install Steam Streaming Speakers or VB-CABLE) — audio will also play on host speakers");
            return Self { capture_id: None };
        }
        let sink_id = wide_id(&sink);

        let mut cur = [0u16; DEVICE_ID_CCH];
        let have_cur = unsafe { GetDefaultAudioDeviceId(cur.as_mut_ptr(), DEVICE_ID_CCH as i32) } == 0;

        // Virtual sink is already the default (e.g. previous run died before
        // restoring) — capture it directly; the restore path will move the
        // default to a real device via recover_stuck_sink at session end.
        if have_cur && wide_id(&cur) == sink_id {
            println!("🎧 Audio: client-only — virtual sink is already the default output");
            return Self { capture_id: Some(sink_id) };
        }

        let ret = unsafe { SetDefaultAudioDevice(sink_id.as_ptr()) };
        if ret != 0 {
            eprintln!("⚠️  Audio: could not switch default output to virtual sink (code {}) — audio will also play on host speakers", ret);
            return Self { capture_id: None };
        }

        // Verify the switch actually landed — diagnoses "plays on both" cases.
        let mut now = [0u16; DEVICE_ID_CCH];
        if unsafe { GetDefaultAudioDeviceId(now.as_mut_ptr(), DEVICE_ID_CCH as i32) } == 0
            && wide_id(&now) != sink_id
        {
            eprintln!("⚠️  Audio: default-device readback does not match virtual sink — host speakers may still play");
        }

        println!("🎧 Audio: client-only — default output switched to virtual sink (host speakers silent, not muted)");
        Self { capture_id: Some(sink_id) }
    }
}

impl Drop for SinkGuard {
    fn drop(&mut self) {
        // Claim-once: restores the armed pre-stream endpoint, or falls back to
        // live recovery if another path (emergency shutdown) already claimed it.
        restore_original_endpoint();
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
}

// ── WASAPI capture (shim) lifecycle ───────────────────────────────────────────

/// Overlap guard for the shim's process-GLOBAL WASAPI capture state.
///
/// `InitAudioCapture`/`CleanupAudio` manipulate one set of global COM objects in
/// `audio_shim.cpp`. If a new session's init ran while a zombie session's
/// capture thread had not yet finished its cleanup (the `/resume`-over-zombie
/// overlap), the old thread's `CleanupAudio` nulled the NEW session's capture
/// client mid-stream — the "audio doesn't reliably start" symptom.
///
/// `AudioCaptureManager` already serializes sessions (start joins the previous
/// session's threads first), so this flag is the enforcement of that invariant:
/// init refuses to proceed while a previous capture is still live, rather than
/// silently corrupting it.
static SHIM_CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Initialise WASAPI loopback (on `device_id`, or the default render device
/// when None) and return a channel that yields raw PCM chunks (interleaved,
/// format described by the returned AudioFormat).
fn start_capture_thread(
    stop: Arc<AtomicBool>,
    device_id: Option<&[u16]>,
) -> Result<(mpsc::Receiver<Vec<u8>>, AudioFormat, thread::JoinHandle<()>), String> {
    // Acquire the process-global capture slot. The manager joins the previous
    // session before starting a new one, so under normal operation this
    // succeeds first try; the short retry only covers a capture thread that is
    // mid-CleanupAudio at this instant.
    let mut acquired = false;
    for _ in 0..20u32 {
        if SHIM_CAPTURE_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            acquired = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    if !acquired {
        return Err(
            "a previous session's WASAPI capture never released (Init/Cleanup overlap guard)"
                .to_string(),
        );
    }

    let mut sample_rate: u32 = 0;
    let mut channels: u16 = 0;
    let mut bps: u16 = 0;

    let fmt = unsafe {
        let id_ptr = device_id.map_or(std::ptr::null(), |v| v.as_ptr());
        let ret = InitAudioCapture(id_ptr, &mut sample_rate, &mut channels, &mut bps);
        if ret != 0 {
            // Nothing was initialised — free the slot for the next attempt.
            SHIM_CAPTURE_ACTIVE.store(false, Ordering::Release);
            return Err(format!("InitAudioCapture failed (code {})", ret));
        }
        AudioFormat { sample_rate, channels, bits_per_sample: bps }
    };

    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(16);

    let handle = thread::spawn(move || {
        unsafe {
            use windows::Win32::System::Threading::{
                GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
            };
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);

            // Register with the Windows Multimedia Class Scheduler Service
            // "Pro Audio" task — matches Apollo/Sunshine (avrt.h).  MMCSS
            // elevates scheduler quantum and protects the thread from
            // background-priority preemption without REALTIME privilege.
            // Declared locally: windows-rs 0.58 exposes AvSetMmThreadCharacteristicsW
            // in Win32_Media_Audio but the sub-binding isn't in scope here.
            extern "system" {
                fn AvSetMmThreadCharacteristicsW(
                    task_name: *const u16,
                    task_index: *mut u32,
                ) -> *mut std::ffi::c_void;
            }
            let task_name: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
            let mut task_index: u32 = 0;
            let mmcss = AvSetMmThreadCharacteristicsW(task_name.as_ptr(), &mut task_index);
            if mmcss.is_null() {
                eprintln!("⚠️  Audio: MMCSS Pro Audio registration failed (avrt.dll missing?)");
            }
        }
        // ~1 s of audio at worst-case 48 kHz, 2 ch, 32-bit float
        let mut buf = vec![0u8; 48_000 * 2 * 4];
        while !stop.load(Ordering::Relaxed) {
            let mut frames: u32 = 0;
            let bytes = unsafe {
                CaptureAudioFrame(buf.as_mut_ptr(), buf.len() as i32, &mut frames)
            };
            if bytes > 0 {
                match tx.try_send(buf[..bytes as usize].to_vec()) {
                    // Receiver gone — streaming stopped.
                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                    // Channel full — drop this chunk instead of killing the
                    // session; the consumer will catch up.
                    _ => {}
                }
            } else if bytes < 0 {
                eprintln!("❌ CaptureAudioFrame error: {}", bytes);
                break;
            } else {
                thread::sleep(Duration::from_millis(2));
            }
        }
        unsafe { CleanupAudio(); }
        // Cleanup complete — the next session may now InitAudioCapture.
        SHIM_CAPTURE_ACTIVE.store(false, Ordering::Release);
    });

    Ok((rx, fmt, handle))
}

/// AES-128-CBC with PKCS7 padding — GameStream audio encryption
/// (Sunshine crypto::cipher::cbc_t with padding=true).
fn aes_cbc_encrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = aes::Aes128::new(GenericArray::from_slice(key));
    let pad = 16 - (data.len() % 16); // PKCS7: 1..=16 bytes, always present
    let mut buf = data.to_vec();
    buf.extend(std::iter::repeat(pad as u8).take(pad));
    let mut prev = *iv;
    for chunk in buf.chunks_mut(16) {
        for i in 0..16 {
            chunk[i] ^= prev[i];
        }
        cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
        prev.copy_from_slice(chunk);
    }
    buf
}

// ── Session lifecycle ─────────────────────────────────────────────────────────

/// A live audio session: the send thread (which owns the SinkGuard and the
/// capture thread) plus its stop signal.
struct AudioSession {
    stop: Arc<AtomicBool>,
    handle: thread::JoinHandle<()>,
}

/// Sole owner of the streaming audio lifecycle.
///
/// One instance lives in `run()` for the process lifetime. `start_for_stream`
/// / `stop_and_release` are tied to the streaming session state machine —
/// audio starts when a session reaches PLAY and is fully released (WASAPI
/// cleanup, sink restore) when the session ends, on every path including
/// zombie-session replacement:
///
/// - `start_for_stream` FIRST stops and releases any previous session
///   (blocking join), so a new session can never overlap a zombie's teardown.
///   The old `Option<AudioStreamer>` pattern in lib.rs evaluated
///   `AudioStreamer::start(..)` BEFORE dropping the previous streamer — the new
///   session's `InitAudioCapture` could run before the zombie's `CleanupAudio`,
///   which then nulled the new session's WASAPI state.
/// - `stop_and_release` restores the pre-stream default endpoint even when no
///   audio thread ever started (e.g. `/cancel` before PLAY, after the VDD flip
///   already armed a restore target).
/// - `Drop` = `stop_and_release`, covering the graceful-shutdown paths.
pub struct AudioCaptureManager {
    session: Option<AudioSession>,
}

impl AudioCaptureManager {
    pub const fn new() -> Self {
        Self { session: None }
    }

    /// Start the audio pipeline for a streaming session
    /// (WASAPI loopback → Opus → RTP on `socket`).
    #[allow(clippy::too_many_arguments)]
    pub fn start_for_stream(
        &mut self,
        socket: UdpSocket,
        rikey: [u8; 16],
        rikeyid: u32,
        encrypt: bool,
        packet_duration_ms: u32,
        host_audio: bool,
    ) {
        // Serialize sessions: fully tear down any previous one (blocking) so
        // Init/Cleanup can never interleave and the endpoint state is clean.
        self.stop_and_release();

        // Fallback arm — lib.rs normally armed before display activation, in
        // which case this is a no-op; sessions without a VDD activation (or a
        // PLAY that arrived before pre-activation) arm here instead.
        arm_endpoint_restore();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let handle = thread::spawn(move || {
            unsafe {
                use windows::Win32::System::Threading::{
                    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
                };
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
            }
            audio_send_loop(socket, rikey, rikeyid, encrypt, packet_duration_ms, host_audio, stop_flag);
        });
        self.session = Some(AudioSession { stop, handle });
    }

    /// Stop the audio pipeline and release every resource it holds: joins the
    /// send thread (which joins the capture thread → `CleanupAudio` releases
    /// the shim's global WASAPI state) and restores the pre-stream default
    /// endpoint. Blocking; idempotent; safe to call with no session running —
    /// the endpoint restore still runs (claim-once, quiet no-op if nothing is
    /// armed), covering sessions cancelled before audio ever started.
    pub fn stop_and_release(&mut self) {
        if let Some(s) = self.session.take() {
            s.stop.store(true, Ordering::Relaxed);
            // Joining guarantees the send thread's SinkGuard has dropped (the
            // endpoint restore ran) and the capture thread has completed
            // CleanupAudio before this returns.
            let _ = s.handle.join();
            println!("🎵 Audio stream stopped — capture released, default output restored");
        }
        // No session thread (or the thread exited early before engaging the
        // sink): claim + restore whatever is still armed. Covers /cancel
        // before PLAY when the VDD activation already armed a restore target.
        restore_original_endpoint();
    }
}

/// Graceful-shutdown path: dropping the manager stops the session and restores
/// the pre-stream default endpoint.
impl Drop for AudioCaptureManager {
    fn drop(&mut self) {
        self.stop_and_release();
    }
}

fn audio_send_loop(
    socket: UdpSocket,
    rikey: [u8; 16],
    rikeyid: u32,
    encrypt: bool,
    packet_duration_ms: u32,
    host_audio: bool,
    stop: Arc<AtomicBool>,
) {
    // Routing must be decided before capture starts: client-only mode captures
    // the virtual sink by id, so the loopback never touches the host speakers.
    // The guard triggers the claim-once endpoint restore on every exit path.
    let sink_guard = SinkGuard::engage(host_audio);

    let (rx, fmt, cap_handle) = match start_capture_thread(stop.clone(), sink_guard.capture_id.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("🎵 Audio disabled: {}", e);
            return;
        }
    };

    send_pcm_loop(&socket, rikey, rikeyid, encrypt, packet_duration_ms, &stop, &rx, fmt);

    // Tear capture down COMPLETELY before this function returns: CleanupAudio
    // releases the shim's GLOBAL WASAPI state, so a reconnect's InitAudioCapture
    // must never overlap it — a lingering capture thread from the old session
    // would otherwise null out the new session's capture client mid-stream
    // (SHIM_CAPTURE_ACTIVE enforces this; joining here means the guard is
    // already free when the manager's stop_and_release returns).
    // Joining also guarantees teardown precedes the SinkGuard drop below that
    // restores the default output device.
    stop.store(true, Ordering::Relaxed);
    drop(rx); // also unblocks the capture thread's channel sends
    let _ = cap_handle.join();
    drop(sink_guard);
}

#[allow(clippy::too_many_arguments)]
fn send_pcm_loop(
    socket: &UdpSocket,
    rikey: [u8; 16],
    rikeyid: u32,
    encrypt: bool,
    packet_duration_ms: u32,
    stop: &AtomicBool,
    rx: &mpsc::Receiver<Vec<u8>>,
    fmt: AudioFormat,
) {
    // Opus only accepts 48/24/16/12/8 kHz; the Windows shared-mode mix format
    // is essentially always 48 kHz. Resampling is out of scope for v1.
    if fmt.sample_rate != 48_000 {
        eprintln!("🎵 Audio disabled: mix format is {} Hz (need 48000)", fmt.sample_rate);
        return;
    }
    if fmt.bits_per_sample != 32 && fmt.bits_per_sample != 16 {
        eprintln!("🎵 Audio disabled: unsupported sample format ({}-bit)", fmt.bits_per_sample);
        return;
    }

    let mut encoder = match OpusEncoder::new(SampleRate::Hz48000, Channels::Stereo, Application::LowDelay) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("🎵 Audio disabled: Opus encoder init failed: {:?}", e);
            return;
        }
    };
    let _ = encoder.set_bitrate(Bitrate::BitsPerSecond(128_000));

    let samples_per_packet = (48_000 * packet_duration_ms as usize) / 1000; // per channel
    println!("🎵 Audio stream started: Opus 48kHz stereo 128kbps, {}ms packets, encrypted={}",
        packet_duration_ms, encrypt);

    let src_channels = fmt.channels.max(1) as usize;
    let mut pcm: VecDeque<f32> = VecDeque::with_capacity(48_000);
    let mut frame_buf: Vec<f32> = Vec::with_capacity(samples_per_packet * 2);
    let mut opus_buf = [0u8; 1400];
    let mut target: Option<SocketAddr> = None;
    let mut seq: u16 = 0;
    let mut timestamp: u32 = 0;
    let mut ping = [0u8; 64];

    while !stop.load(Ordering::Relaxed) {
        // Learn (and keep refreshed) the client's audio address from its pings.
        while let Ok((_n, addr)) = socket.recv_from(&mut ping) {
            if target != Some(addr) {
                println!("🎵 Learned client audio address: {}", addr);
                target = Some(addr);
            }
        }

        // Pull captured PCM, downmix to stereo f32 interleaved.
        match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(chunk) => {
                let bytes_per_sample = (fmt.bits_per_sample / 8) as usize;
                let frame_stride = bytes_per_sample * src_channels;
                for frame in chunk.chunks_exact(frame_stride) {
                    for ch in 0..2 {
                        // Mono source: duplicate channel 0 into both.
                        let idx = if src_channels == 1 { 0 } else { ch };
                        let off = idx * bytes_per_sample;
                        let s = if fmt.bits_per_sample == 32 {
                            f32::from_le_bytes([frame[off], frame[off + 1], frame[off + 2], frame[off + 3]])
                        } else {
                            i16::from_le_bytes([frame[off], frame[off + 1]]) as f32 / 32768.0
                        };
                        pcm.push_back(s);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Emit as many full Opus frames as we have samples for.
        while pcm.len() >= samples_per_packet * 2 {
            frame_buf.clear();
            frame_buf.extend(pcm.drain(..samples_per_packet * 2));

            let n = match encoder.encode_float(&frame_buf, &mut opus_buf) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("🎵 Opus encode error: {:?}", e);
                    continue;
                }
            };

            if let Some(t) = target {
                // IV = 16 zero bytes, first 4 = BE(rikeyid + seq) — Sunshine
                // stream.cpp:1630 / moonlight-common-c AudioStream.c.
                let payload: Vec<u8> = if encrypt {
                    let mut iv = [0u8; 16];
                    iv[..4].copy_from_slice(&rikeyid.wrapping_add(seq as u32).to_be_bytes());
                    aes_cbc_encrypt(&rikey, &iv, &opus_buf[..n])
                } else {
                    opus_buf[..n].to_vec()
                };

                let mut pkt = Vec::with_capacity(12 + payload.len());
                pkt.push(0x80); // V=2
                pkt.push(97);   // packetType: opus audio
                pkt.extend_from_slice(&seq.to_be_bytes());
                pkt.extend_from_slice(&timestamp.to_be_bytes());
                pkt.extend_from_slice(&[0u8; 4]); // ssrc
                pkt.extend_from_slice(&payload);
                let _ = socket.send_to(&pkt, t);
            }

            seq = seq.wrapping_add(1);
            timestamp = timestamp.wrapping_add(packet_duration_ms);
        }
    }
}
