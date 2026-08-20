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
//
// ## Mid-session routing watchdog (Phase 15.6 — the "ghost" duty)
//
// WASAPI loopback binds ONE device at init and never follows default changes.
// A 1 Hz check inside the send loop keeps routing correct for the whole
// session: in client-only mode the sink is RE-ASSERTED as default if anything
// (late device arrival, sound-settings fiddling) moves it; when capturing the
// default (host_audio, or no sink available) a default change triggers a full
// capture REBIND onto the new endpoint — healing the "VDD HDMI audio appears
// seconds after activation, apps migrate, stream goes silent" failure. The
// streaming sink itself is selectable via nova.toml `[audio]
// endpoint_override` ([`set_sink_override`]) on top of the shim's built-in
// known-sink list.

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

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
    fn FindAudioDeviceByName(needle: *const u16, out_id: *mut u16, cch: i32) -> i32;
    fn SetDefaultAudioDevice(device_id: *const u16) -> i32;
    /// Flow-aware twin of [`GetDefaultAudioDeviceId`]. `is_capture != 0` asks
    /// about the recording default.
    fn GetDefaultEndpointId(is_capture: i32, out_id: *mut u16, cch: i32) -> i32;
    /// Flow-aware endpoint search by friendly-name substring or exact id.
    fn FindEndpointByName(needle: *const u16, is_capture: i32, out_id: *mut u16, cch: i32) -> i32;
}

// ── Streaming-sink selection ──────────────────────────────────────────────────

/// Operator-designated streaming sink (nova.toml `[audio] endpoint_override`),
/// stored as NUL-terminated UTF-16 at startup. When set, sink resolution tries
/// it FIRST — matched case-insensitively as a substring of endpoint friendly
/// names or exactly as an endpoint id — so ANY active render device (the VDD's
/// HDMI audio, VoiceMeeter, a vendor virtual output) can serve as the ghost
/// sink without extending the shim's built-in list.
static SINK_OVERRIDE: Mutex<Option<Vec<u16>>> = Mutex::new(None);

/// Record the nova.toml override. Called once from `run()` right after config
/// load, BEFORE `recover_stuck_sink` — crash recovery must recognise a custom
/// sink as "the sink" too. Empty/whitespace = no override.
pub fn set_sink_override(name: &str) {
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    *SINK_OVERRIDE.lock().unwrap_or_else(|p| p.into_inner()) = Some(wide);
    println!("🎧 Audio: endpoint_override set — \"{name}\" is the preferred streaming sink");
}

/// Resolve the streaming sink's endpoint id: the nova.toml override first
/// (warn-and-fall-through when it matches no ACTIVE endpoint — e.g. the VDD's
/// audio device before the display activates), then the shim's ordered built-in
/// list (`kGhostSinkNames`: Steam Streaming Speakers, then NVIDIA Virtual
/// Audio). `None` = no sink available; client-only routing degrades to
/// capturing the default device.
///
/// VB-CABLE is NOT in that list, and must not be added: the Echo microphone
/// renders into it, so a ghost sink there would feed host audio back to the
/// remote user as their own microphone. `mic::collides_with_ghost_sink` refuses
/// the `endpoint_override` route to the same place.
fn find_virtual_sink_id() -> Option<Vec<u16>> {
    let needle = SINK_OVERRIDE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    if let Some(needle) = needle {
        let mut id = [0u16; DEVICE_ID_CCH];
        if unsafe { FindAudioDeviceByName(needle.as_ptr(), id.as_mut_ptr(), DEVICE_ID_CCH as i32) } == 0 {
            return Some(wide_id(&id));
        }
        println!("⚠️  Audio: endpoint_override matched no active render endpoint — trying built-in virtual sinks");
    }
    let mut sink = [0u16; DEVICE_ID_CCH];
    if unsafe { FindVirtualAudioSink(sink.as_mut_ptr(), DEVICE_ID_CCH as i32) } == 0 {
        return Some(wide_id(&sink));
    }
    None
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
    let Some(sink_id) = find_virtual_sink_id() else {
        return; // no streaming sink present — nothing to recover
    };
    let mut cur = [0u16; DEVICE_ID_CCH];
    if unsafe { GetDefaultAudioDeviceId(cur.as_mut_ptr(), DEVICE_ID_CCH as i32) } != 0 {
        return;
    }
    if wide_id(&cur) != sink_id {
        return; // default is already a real device
    }

    let mut real = [0u16; DEVICE_ID_CCH];
    if unsafe { FindRealAudioDevice(real.as_mut_ptr(), DEVICE_ID_CCH as i32) } != 0 {
        eprintln!("⚠️  Audio: default output is the streaming sink (from a previous unclean exit) and no real device was found to restore — check Windows sound settings");
        return;
    }
    let real_id = wide_id(&real);
    // With an endpoint_override, the "first non-built-in-sink device" can BE
    // the override device itself — restoring the sink onto itself would fake
    // success while the host stays silent.
    if real_id == sink_id {
        eprintln!("⚠️  Audio: the streaming sink is the only active output — cannot auto-restore a real device");
        return;
    }
    if unsafe { SetDefaultAudioDevice(real_id.as_ptr()) } == 0 {
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

    if find_virtual_sink_id().is_some_and(|sink_id| sink_id == cur_id) {
        println!("⚠️  Audio: default output is currently the streaming sink — not arming it as the restore target (restore will pick a real device)");
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
    // The microphone default is armed on the same kind of process-death path
    // and must come back with it. Claim-once, so a normal stop that already ran
    // makes this a no-op.
    restore_default_capture();
}

// ── Microphone routing (eCapture) ────────────────────────────────────────────
//
// The mirror of the sink routing above, for the other direction. Echo's client
// microphone is decoded on the host and rendered into VB-CABLE's *input*; an
// application that wants to hear it has to record from the cable's *output*.
// Windows will not do that by itself, so until this existed the passthrough
// worked perfectly and nothing on the host could hear it unless the operator
// had already selected "CABLE Output" as their recording device by hand.
//
// **This deliberately reverses an earlier decision**, and the reason it is safe
// now is the reason it was refused then. The old rule was "Nova never chooses
// anyone's microphone", written when nothing here had a restore path: an
// override with no way back is a setting the operator has to repair themselves,
// which is worse than doing nothing. What makes it correct is the same
// claim-once arm/restore the render side has used since Phase 15.1 — the
// original device is captured before the swap and put back on every ending,
// including the ones nobody plans for.
//
// Two further guards keep it honest: it engages only when the client is
// actually SENDING microphone audio (not merely when a session exists), and it
// refuses to arm the cable itself as a restore target.

/// The capture endpoint that pairs with [`crate::mic`]'s render target.
///
/// VB-CABLE presents two halves: audio rendered into "CABLE Input" comes out of
/// "CABLE Output". `mic.rs` renders into the former, so this is the one an
/// application must record from. Matched as a case-insensitive substring, so
/// the full name ("CABLE Output (VB-Audio Virtual Cable)") matches.
const MIC_CAPTURE_DEVICE: &str = "CABLE Output";

/// The operator's real recording device, remembered across a session.
///
/// Same claim-once discipline as [`ORIGINAL_ENDPOINT`] and for the same reason:
/// several paths can end a session and exactly one of them must perform the
/// restore, with the rest finding `None` and doing nothing.
static ORIGINAL_CAPTURE: Mutex<Option<Vec<u16>>> = Mutex::new(None);

fn lock_original_capture() -> MutexGuard<'static, Option<Vec<u16>>> {
    ORIGINAL_CAPTURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Point Windows' default recording device at the virtual cable.
///
/// Call when the client's microphone audio actually starts arriving — not when
/// a session begins. A session whose user never switched the microphone on must
/// not have their recording device changed, and "packets are arriving" is the
/// only evidence on this side of the wire that the switch is on.
///
/// Idempotent: a second call while already engaged does nothing.
pub fn engage_default_capture() {
    let mut slot = lock_original_capture();
    if slot.is_some() {
        return; // already engaged this session
    }

    let Some(cable) = find_capture_id(MIC_CAPTURE_DEVICE) else {
        println!(
            "🎤 Audio: no \"{MIC_CAPTURE_DEVICE}\" recording endpoint — leaving the default \
             microphone alone (install VB-CABLE for host-side microphone passthrough)"
        );
        return;
    };

    let mut cur = [0u16; DEVICE_ID_CCH];
    if unsafe { GetDefaultEndpointId(1, cur.as_mut_ptr(), DEVICE_ID_CCH as i32) } != 0 {
        eprintln!("⚠️  Audio: could not query the current default microphone — not switching it");
        return;
    }
    let cur_id = wide_id(&cur);

    // Already the cable — from a previous unclean exit, or because the operator
    // chose it themselves. Arming it would make "restore" mean "put the cable
    // back", which is the stuck-silent bug the render side documents.
    if cur_id == cable {
        println!("🎤 Audio: the default microphone is already \"{MIC_CAPTURE_DEVICE}\" — nothing to switch");
        return;
    }

    if unsafe { SetDefaultAudioDevice(cable.as_ptr()) } != 0 {
        eprintln!("⚠️  Audio: could not make \"{MIC_CAPTURE_DEVICE}\" the default microphone");
        return;
    }
    // Stored only AFTER the swap succeeded. Arming first would leave a restore
    // target for a change that never happened, and the restore would then move
    // the operator's default for no reason.
    //
    // Written to disk as well as memory, so a Master that is terminated rather
    // than stopped still has a way back. Best-effort: failing to write must not
    // abort a working microphone, it only costs the cross-process safety net.
    if let Err(e) = std::fs::write(mic_restore_file(), String::from_utf16_lossy(strip_nul(&cur_id)))
    {
        eprintln!("⚠️  Audio: could not record the microphone restore target ({e}) — a hard kill \
                   would leave the default on the cable");
    }
    *slot = Some(cur_id);
    println!(
        "🎤 Audio: default microphone switched to \"{MIC_CAPTURE_DEVICE}\" — the host now hears \
         the client (original device will be restored)"
    );
}

/// Claim-once restore of the operator's recording device.
///
/// Unlike the render side there is no live-query fallback, and that is
/// deliberate: with nothing armed we do not know which microphone was theirs,
/// and guessing — picking "some real capture device" — could hand their default
/// to a webcam they never use. Nothing armed means we never changed it, so the
/// right action is none.
pub fn restore_default_capture() {
    let Some(id) = lock_original_capture().take() else {
        return;
    };
    if unsafe { SetDefaultAudioDevice(id.as_ptr()) } == 0 {
        println!("🎤 Audio: default microphone restored to the pre-stream device");
    } else {
        eprintln!("⚠️  Audio: failed to restore the pre-stream microphone — check Windows sound settings");
    }
    // The cross-process net is only needed while the swap is in effect. Removed
    // even when the restore failed: the file's job is to describe an outstanding
    // swap, and re-applying a device Windows just refused would not help.
    let _ = std::fs::remove_file(mic_restore_file());
}

/// A NUL-terminated wide id without its terminator, for writing as text.
fn strip_nul(id: &[u16]) -> &[u16] {
    match id.iter().position(|&c| c == 0) {
        Some(end) => &id[..end],
        None => id,
    }
}

/// Where the armed microphone is remembered ACROSS processes.
///
/// The in-memory arm covers every ending this process gets to run code for.
/// It does not cover the one that matters most for a LocalSystem service: the
/// Master being terminated — an installer upgrade, `sc stop` landing mid-call,
/// a crash — at which point a detached render thread never reaches its tail and
/// the operator is left recording from a silent cable with no idea why.
///
/// A file makes the recovery exact rather than a guess. This is the same
/// reasoning as `virtual_display`'s `nova_display_baseline.txt`, and it is why
/// there is no "pick any real microphone" fallback: we either know which device
/// was theirs or we leave it alone.
const MIC_RESTORE_PATH: &str = "nova_mic_restore.txt";

/// Next to the exe, never CWD-relative — the SCM sets CWD to System32.
fn mic_restore_file() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(MIC_RESTORE_PATH)))
        .unwrap_or_else(|| std::path::PathBuf::from(MIC_RESTORE_PATH))
}

/// Put back a microphone a previous process swapped and never restored.
///
/// Called once at Master startup. Does nothing unless a restore file exists AND
/// the default really is the cable — so an operator who chose "CABLE Output"
/// deliberately after the crash keeps their choice, and a stale file from a
/// clean run is simply cleared.
pub fn heal_capture_endpoint_at_startup() {
    let path = mic_restore_file();
    let Ok(saved) = std::fs::read_to_string(&path) else {
        return;
    };
    let saved = saved.trim();
    if saved.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }

    let mut cur = [0u16; DEVICE_ID_CCH];
    let current_is_cable = unsafe { GetDefaultEndpointId(1, cur.as_mut_ptr(), DEVICE_ID_CCH as i32) }
        == 0
        && find_capture_id(MIC_CAPTURE_DEVICE).is_some_and(|cable| cable == wide_id(&cur));

    if current_is_cable {
        let id: Vec<u16> = saved.encode_utf16().chain(std::iter::once(0)).collect();
        if unsafe { SetDefaultAudioDevice(id.as_ptr()) } == 0 {
            println!(
                "🎤 Audio: restored the microphone a previous run left switched to \
                 \"{MIC_CAPTURE_DEVICE}\""
            );
        } else {
            eprintln!("⚠️  Audio: could not restore the microphone recorded in {MIC_RESTORE_PATH}");
        }
    }
    // Cleared either way: a file that outlives its usefulness would re-apply an
    // ever-staler device at every future startup.
    let _ = std::fs::remove_file(&path);
}

/// Resolve a capture endpoint by friendly-name substring.
fn find_capture_id(name: &str) -> Option<Vec<u16>> {
    let needle: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut out = [0u16; DEVICE_ID_CCH];
    if unsafe { FindEndpointByName(needle.as_ptr(), 1, out.as_mut_ptr(), DEVICE_ID_CCH as i32) } == 0
    {
        Some(wide_id(&out))
    } else {
        None
    }
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

        let Some(sink_id) = find_virtual_sink_id() else {
            eprintln!("⚠️  Audio: no streaming sink found (set [audio] endpoint_override in nova.toml, or install Steam Streaming Speakers / VB-CABLE) — audio will also play on host speakers");
            return Self { capture_id: None };
        };

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

/// A running WASAPI loopback capture: PCM channel, format, and its OWN stop
/// flag — separate from the session stop so a mid-session capture REBIND
/// (default endpoint moved) can tear down and rebuild capture without ending
/// the audio session.
struct CaptureSession {
    rx: mpsc::Receiver<Vec<u8>>,
    fmt: AudioFormat,
    stop: Arc<AtomicBool>,
    handle: thread::JoinHandle<()>,
}

impl CaptureSession {
    /// Stop the capture thread and block until `CleanupAudio` has released the
    /// shim's global WASAPI state (`SHIM_CAPTURE_ACTIVE` freed) — after this a
    /// new `start_capture_thread` may run immediately.
    fn shutdown(self) {
        self.stop.store(true, Ordering::Relaxed);
        drop(self.rx); // also unblocks the capture thread's channel sends
        let _ = self.handle.join();
    }
}

/// Initialise WASAPI loopback (on `device_id`, or the default render device
/// when None) and return the running capture session.
fn start_capture_thread(device_id: Option<&[u16]>) -> Result<CaptureSession, String> {
    let stop = Arc::new(AtomicBool::new(false));
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

    let stop_flag = stop.clone();
    let handle = thread::spawn(move || {
        let stop = stop_flag;
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

    Ok(CaptureSession { rx, fmt, stop, handle })
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

// ── Network-send relay (Phase 3 — Master-side, reused by the monolithic
// path too) ──────────────────────────────────────────────────────────────
//
// Owns exactly what `send_pcm_loop` used to build in-process: the RTP-audio
// header, AES-128-CBC encryption, seq/timestamp continuity, and the actual
// UDP send — the network-facing half of the audio pipeline, as opposed to
// the WASAPI-capture/Opus-encode half above, which stays wherever the
// capture thread runs (the Worker, under the split architecture; still
// in-process for the monolithic `run()`). Relocating ONLY this half mirrors
// exactly how `rtp.rs`'s `RtpSender` already owns video's network-send
// independently of wherever encoding happens.
//
// A fresh instance's `target`/`seq`/`timestamp` all start at their "no
// session yet" defaults; `reconfigure` (called once per new session, before
// any frames arrive) resets them again so a reconnect never inherits a
// stale session's sequence numbers or a stale learned address — the same
// reasoning `RtpSender::reset()` follows for video's `frame_index`.
pub struct AudioTxState {
    socket: UdpSocket,
    target: Option<SocketAddr>,
    seq: u16,
    timestamp: u32,
    rikey: [u8; 16],
    rikeyid: u32,
    encrypt: bool,
    packet_duration_ms: u32,
}

impl AudioTxState {
    pub fn new(socket: UdpSocket) -> Self {
        Self {
            socket,
            target: None,
            seq: 0,
            timestamp: 0,
            rikey: [0; 16],
            rikeyid: 0,
            encrypt: false,
            packet_duration_ms: 5,
        }
    }

    /// Call once per new session (mirrors the old fused `send_pcm_loop`
    /// building a brand new `AudioTxState` per `audio_send_loop` call).
    pub fn reconfigure(&mut self, rikey: [u8; 16], rikeyid: u32, encrypt: bool, packet_duration_ms: u32) {
        self.rikey = rikey;
        self.rikeyid = rikeyid;
        self.encrypt = encrypt;
        self.packet_duration_ms = packet_duration_ms.max(1);
        self.seq = 0;
        self.timestamp = 0;
        self.target = None;
    }

    /// Drains any pending client pings, learning/refreshing the target
    /// address — mirrors `rtp::RtpSender::try_learn_target`. Called from
    /// `send_frame` itself (below) rather than on a separate timer: audio
    /// frames arrive every 5-20ms, far more often than any tick would fire,
    /// so piggybacking here is both simpler and more responsive.
    fn learn_target(&mut self) {
        let mut buf = [0u8; 64];
        while let Ok((_n, addr)) = self.socket.recv_from(&mut buf) {
            if self.target != Some(addr) {
                println!("🎵 Learned client audio address: {addr}");
                self.target = Some(addr);
            }
        }
    }

    /// Encrypts (if configured) and sends one Opus-encoded audio frame as an
    /// RTP audio packet — the exact wire format the old fused
    /// `send_pcm_loop` built in-process, just relocated here. A no-op until
    /// the target is learned (mirrors the old `if let Some(t) = tx_state.target`
    /// gate).
    pub fn send_frame(&mut self, opus_payload: &[u8]) {
        self.learn_target();
        let Some(target) = self.target else { return };

        // IV = 16 zero bytes, first 4 = BE(rikeyid + seq) — Sunshine
        // stream.cpp:1630 / moonlight-common-c AudioStream.c.
        let payload: Vec<u8> = if self.encrypt {
            let mut iv = [0u8; 16];
            iv[..4].copy_from_slice(&self.rikeyid.wrapping_add(self.seq as u32).to_be_bytes());
            aes_cbc_encrypt(&self.rikey, &iv, opus_payload)
        } else {
            opus_payload.to_vec()
        };

        let mut pkt = Vec::with_capacity(12 + payload.len());
        pkt.push(0x80); // V=2
        pkt.push(97);   // packetType: opus audio
        pkt.extend_from_slice(&self.seq.to_be_bytes());
        pkt.extend_from_slice(&self.timestamp.to_be_bytes());
        pkt.extend_from_slice(&[0u8; 4]); // ssrc
        pkt.extend_from_slice(&payload);
        let _ = self.socket.send_to(&pkt, target);

        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(self.packet_duration_ms);
    }
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

    /// Start the audio pipeline for a streaming session (WASAPI loopback →
    /// Opus). The encoded frames go out over `frame_tx` — raw Opus bytes,
    /// pre-encryption — to whoever is forwarding them to Master over IPC
    /// (the Worker's main loop); this module no longer touches a network
    /// socket at all. Master's `AudioTxState` (below) owns the RTP-audio
    /// header, AES-CBC encryption, and the actual UDP send — see this
    /// file's module doc for why that ownership moved.
    pub fn start_for_stream(
        &mut self,
        frame_tx: mpsc::Sender<Vec<u8>>,
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
            audio_send_loop(frame_tx, packet_duration_ms, host_audio, stop_flag);
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

/// Why `send_pcm_loop` returned.
enum SendExit {
    /// Session stop (client disconnect / manager stop) or a fatal pipeline
    /// error — the audio session ends.
    SessionStopped,
    /// The default render endpoint changed while capture was FOLLOWING the
    /// default (host_audio mode, or no sink available). WASAPI loopback is
    /// bound to one device at init and does NOT follow default changes — the
    /// classic case is the VDD's HDMI audio endpoint appearing a few seconds
    /// after display activation: Windows flips the default, every app stream
    /// migrates, and the old device we're capturing goes silent ("audio
    /// missed by the capture loop"). The caller rebinds capture to the NEW
    /// default and continues the same session.
    DeviceChanged,
}

/// Where this capture is bound, for the once-per-second routing watchdog.
enum CaptureRoute {
    /// Client-only mode: capture is pinned to the sink by id, and the sink
    /// must REMAIN the system default for the whole stream. If the default
    /// drifts (late device arrival, user fiddling with sound settings), the
    /// watchdog re-asserts the sink — the "ghost orchestrator" duty. The
    /// armed pre-stream endpoint still restores at session end.
    PinnedSink(Vec<u16>),
    /// Capturing the default endpoint: remember WHICH device that was; when
    /// the default moves, exit with `DeviceChanged` so capture rebinds to
    /// where the application audio actually went. Empty id = the initial
    /// query failed; the watchdog stays inert.
    FollowDefault(Vec<u16>),
}

fn audio_send_loop(
    frame_tx: mpsc::Sender<Vec<u8>>,
    packet_duration_ms: u32,
    host_audio: bool,
    stop: Arc<AtomicBool>,
) {
    // Routing must be decided before capture starts: client-only mode captures
    // the virtual sink by id, so the loopback never touches the host speakers.
    // The guard triggers the claim-once endpoint restore on every exit path.
    // Engaged ONCE for the session — capture rebinds below reuse it.
    let sink_guard = SinkGuard::engage(host_audio);

    loop {
        let cap = match start_capture_thread(sink_guard.capture_id.as_deref()) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("🎵 Audio disabled: {}", e);
                break;
            }
        };

        let route = match &sink_guard.capture_id {
            Some(id) => CaptureRoute::PinnedSink(id.clone()),
            None => {
                let mut cur = [0u16; DEVICE_ID_CCH];
                if unsafe { GetDefaultAudioDeviceId(cur.as_mut_ptr(), DEVICE_ID_CCH as i32) } == 0 {
                    CaptureRoute::FollowDefault(wide_id(&cur))
                } else {
                    CaptureRoute::FollowDefault(Vec::new()) // unknown — watchdog inert
                }
            }
        };

        let exit = send_pcm_loop(
            packet_duration_ms, &stop, &cap.rx, cap.fmt, &route, &frame_tx,
        );

        // Tear capture down COMPLETELY before continuing: CleanupAudio releases
        // the shim's GLOBAL WASAPI state, so the next InitAudioCapture (rebind
        // or a future session) must never overlap it (SHIM_CAPTURE_ACTIVE
        // enforces this; joining here means the guard is already free when the
        // manager's stop_and_release returns).
        cap.shutdown();

        match exit {
            SendExit::SessionStopped => break,
            SendExit::DeviceChanged => {
                println!("🎵 Audio: default output changed mid-stream — rebinding loopback capture to the new endpoint");
                // Let the endpoint flip settle before re-querying/binding.
                thread::sleep(Duration::from_millis(300));
                if stop.load(Ordering::Relaxed) {
                    break;
                }
            }
        }
    }

    // Joining above guarantees capture teardown precedes the SinkGuard drop
    // here that restores the default output device.
    drop(sink_guard);
}

fn send_pcm_loop(
    packet_duration_ms: u32,
    stop: &AtomicBool,
    rx: &mpsc::Receiver<Vec<u8>>,
    fmt: AudioFormat,
    route: &CaptureRoute,
    frame_tx: &mpsc::Sender<Vec<u8>>,
) -> SendExit {
    // Opus only accepts 48/24/16/12/8 kHz; the Windows shared-mode mix format
    // is essentially always 48 kHz. Resampling is out of scope for v1.
    if fmt.sample_rate != 48_000 {
        eprintln!("🎵 Audio disabled: mix format is {} Hz (need 48000)", fmt.sample_rate);
        return SendExit::SessionStopped;
    }
    if fmt.bits_per_sample != 32 && fmt.bits_per_sample != 16 {
        eprintln!("🎵 Audio disabled: unsupported sample format ({}-bit)", fmt.bits_per_sample);
        return SendExit::SessionStopped;
    }

    let mut encoder = match OpusEncoder::new(SampleRate::Hz48000, Channels::Stereo, Application::LowDelay) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("🎵 Audio disabled: Opus encoder init failed: {:?}", e);
            return SendExit::SessionStopped;
        }
    };
    let _ = encoder.set_bitrate(Bitrate::BitsPerSecond(128_000));

    let samples_per_packet = (48_000 * packet_duration_ms as usize) / 1000; // per channel
    println!("🎵 Audio stream started: Opus 48kHz stereo 128kbps, {}ms packets", packet_duration_ms);

    let src_channels = fmt.channels.max(1) as usize;
    let mut pcm: VecDeque<f32> = VecDeque::with_capacity(48_000);
    let mut frame_buf: Vec<f32> = Vec::with_capacity(samples_per_packet * 2);
    let mut opus_buf = [0u8; 1400];
    // Routing watchdog bookkeeping — checked once per second.
    let mut last_route_check = Instant::now();
    let mut reasserts: u32 = 0;

    while !stop.load(Ordering::Relaxed) {
        // ── Routing watchdog (1 Hz) — the "ghost" enforcement ────────────────
        if last_route_check.elapsed() >= Duration::from_secs(1) {
            last_route_check = Instant::now();
            let mut cur = [0u16; DEVICE_ID_CCH];
            let have_cur =
                unsafe { GetDefaultAudioDeviceId(cur.as_mut_ptr(), DEVICE_ID_CCH as i32) } == 0;
            if have_cur {
                let cur_id = wide_id(&cur);
                match route {
                    CaptureRoute::PinnedSink(sink_id) if cur_id != *sink_id => {
                        // Default drifted off the sink mid-stream (late device
                        // arrival, sound-settings fiddling) — application audio
                        // would fall back to host speakers and vanish from the
                        // stream. Re-assert the sink; capture (pinned by id)
                        // stays valid throughout.
                        reasserts += 1;
                        if unsafe { SetDefaultAudioDevice(sink_id.as_ptr()) } == 0 {
                            if reasserts == 1 || reasserts % 10 == 0 {
                                println!("🎧 Audio: default output drifted off the streaming sink — re-asserted ({} time(s))", reasserts);
                            }
                        } else if reasserts == 1 {
                            eprintln!("⚠️  Audio: default output drifted off the streaming sink and re-assert failed — host speakers may play");
                        }
                    }
                    CaptureRoute::FollowDefault(initial)
                        if !initial.is_empty() && cur_id != *initial =>
                    {
                        // The device we're loopback-capturing is no longer the
                        // default — app audio migrated. Rebind capture.
                        return SendExit::DeviceChanged;
                    }
                    _ => {}
                }
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

            // Raw Opus bytes only — Master's AudioTxState (below) owns the
            // RTP-audio header, AES-CBC encryption, seq/timestamp, and the
            // actual UDP send. A disconnected receiver means the Worker's
            // main loop (and thus the whole IPC link) is already gone —
            // this session is ending anyway, so just stop trying.
            if frame_tx.send(opus_buf[..n].to_vec()).is_err() {
                return SendExit::SessionStopped;
            }
        }
    }

    SendExit::SessionStopped
}
