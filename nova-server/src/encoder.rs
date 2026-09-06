use std::os::windows::ffi::OsStrExt;
use std::ffi::{c_void, CString};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

// ── Congestion-control state ─────────────────────────────────────────────────
// STREAM_BITRATE_KBPS: the active CBR target for the current session.
// Written by lib.rs on session start / reconfigure; read by control.rs when
// computing a congestion-triggered reduction.
static STREAM_BITRATE_KBPS: AtomicI32 = AtomicI32::new(0);

// CONGESTION_BITRATE_KBPS: pending bitrate set by the control thread on
// PT_LOSS_STATS. -1 = no pending change. compare_exchange consolidates rapid
// loss reports into a single reduction; the main loop calls take_congestion_bitrate()
// to claim it and then applies the reconfigure.
static CONGESTION_BITRATE_KBPS: AtomicI32 = AtomicI32::new(-1);

pub fn set_stream_bitrate_kbps(kbps: i32) {
    STREAM_BITRATE_KBPS.store(kbps, Ordering::Relaxed);
}

pub fn get_stream_bitrate_kbps() -> i32 {
    STREAM_BITRATE_KBPS.load(Ordering::Relaxed)
}

/// Called from the control thread on PT_LOSS_STATS. Signals a 20% bitrate cut;
/// compare_exchange ensures multiple rapid loss reports collapse to one signal.
pub fn signal_congestion_reduction() {
    let cur = STREAM_BITRATE_KBPS.load(Ordering::Relaxed);
    if cur > 0 {
        let reduced = (cur * 4 / 5).max(1000); // floor at 1 Mbps
        let _ = CONGESTION_BITRATE_KBPS.compare_exchange(
            -1, reduced, Ordering::Relaxed, Ordering::Relaxed,
        );
    }
}

/// Main loop: atomically claim and return any pending congestion bitrate (Kbps).
/// Returns None when no signal is pending; clears the signal on return.
pub fn take_congestion_bitrate() -> Option<u32> {
    let v = CONGESTION_BITRATE_KBPS.swap(-1, Ordering::Relaxed);
    if v >= 0 { Some(v as u32) } else { None }
}

extern "C" {
    /// Tell the C++ shim where to write its log output.  Must be called before
    /// any other shim function so that D3D11/NVENC init errors are captured.
    fn InitShimLog(log_path: *const u16);

    /// Override the HDR10 HEVC SEI luminance parameters (type 137 MDCV + type 144
    /// CLL/FALL).  Call once after loading nova.toml, before the first
    /// InitEncoder.  BT.2020 primaries are always the standard constants.
    fn SetHdrMetadata(max_luminance_nits: u32, max_cll_nits: u32, max_fall_nits: u32);
    fn SetDeepDpbAuthorized(authorized: i32);

    /// Set the SDR white level (scRGB units, i.e. nits / 80) the SDR↔HDR
    /// conversion shaders use. See `set_sdr_white_level`.
    fn SetSdrWhiteLevel(scrgb_units: f32);

    fn OpenNvEncSession(d3d11_device: *mut c_void, out_encoder: *mut *mut c_void) -> i32;
    fn InitEncoder(
        encoder: *mut c_void,
        width: i32,
        height: i32,
        codec: *const std::ffi::c_char,
        bitrate_kbps: i32,
        fps: i32,
        is_hdr: i32,
    ) -> i32;
    fn InitColorConversion(device: *mut c_void, width: i32, height: i32, is_hdr: i32, fps: i32) -> i32;
    fn EncodeFrame(
        encoder: *mut c_void,
        d3d11_texture: *mut c_void,
        width: i32,
        height: i32,
        out_buffer: *mut u8,
        max_size: i32,
        frame_index: u64,
    ) -> i32;
    fn CleanupEncoder(encoder: *mut c_void) -> i32;
    fn RequestIdrFrame(encoder: *mut c_void);
    fn ReconfigureBitrate(bitrate_kbps: i32, fps: i32) -> i32;

    /// RFI: invalidate the client's `[first,last]` frame-index range in NVENC's
    /// DPB so the next P-frame recovers without an IDR. Returns 1 on success,
    /// 0 when the range can't be honoured (caller must force an IDR).
    fn InvalidateRefFrames(first_frame: u64, last_frame: u64) -> i32;
    /// RFI: whether this GPU/codec supports reference-picture invalidation
    /// (probed at InitEncoder). 1 = supported.
    fn RfiSupported() -> i32;
    /// RFI: whether the most recently encoded frame was a recovery frame (its
    /// reference was re-pointed by an invalidation). 1 = mark it wire type 5.
    fn LastFrameWasRfiRecovery() -> i32;

    /// LTR: arm the next encoded frame to reference a long-term reference
    /// instead of the frame before it. Returns 1 when one was armed, 0 when no
    /// usable long-term reference exists (caller must force an IDR).
    fn ArmLtrRecovery() -> i32;
    /// LTR: report the newest wire frame index the client confirmed decoding,
    /// so recovery only ever references a picture it provably holds.
    fn NotifyLtrAcked(frame_index: u64);
    /// LTR: whether long-term references are live for this session.
    fn LtrActive() -> i32;
    /// LTR: whether the frame just encoded was an LTR recovery frame.
    fn LastFrameWasLtrRecovery() -> i32;

    // ── Bug-reporter frame capture (shim/snapshot.cpp) ──────────────────────
    //
    // Nothing here touches NVENC. The snapshot is taken from the composite
    // texture the encoder consumes, one step before colour conversion, so it is
    // a picture of what was streamed rather than a second render of it.
    fn ArmFrameSnapshot();
    fn TakeFrameSnapshotPng(path: *const u16) -> i32;
    fn ResetFrameSnapshot();

    // ── Local recording (shim/recorder.cpp) ─────────────────────────────────
    //
    // Also NVENC-free: the recorder receives the bytes NVENC already produced
    // and writes a container around them. No second encoder session exists, and
    // adding one is the thing this design is avoiding.
    fn RecorderStart(
        path: *const u16,
        codec: i32,
        width: i32,
        height: i32,
        fps: i32,
        is_hdr: i32,
    ) -> i32;
    fn RecorderStop() -> i32;
    fn RecorderIsActive() -> i32;
    fn RecorderDroppedFrames() -> u64;
}

/// Ask the encode loop to keep its next frame for the bug reporter.
///
/// Returns immediately; the frame appears when the encoder next runs. A host
/// that is not streaming never calls `EncodeFrame`, so this can legitimately
/// never be satisfied — [`take_frame_snapshot_png`] reports that as "nothing
/// captured" rather than blocking.
pub fn arm_frame_snapshot() {
    unsafe { ArmFrameSnapshot() }
}

/// Write the armed frame to `path` as a PNG.
///
/// `Ok(false)` means nothing had been captured — armed but no frame encoded
/// since, or never armed. That is a normal outcome for an idle host and is not
/// an error.
pub fn take_frame_snapshot_png(path: &std::path::Path) -> Result<bool, i32> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    match unsafe { TakeFrameSnapshotPng(wide.as_ptr()) } {
        0 => Ok(true),
        -1 => Ok(false),
        code => Err(code),
    }
}

/// Drop any held frame and free the readback texture.
///
/// Called at session end: a snapshot is a picture of somebody's desktop, and it
/// has no business outliving the stream it came from just because nobody pressed
/// the report button.
pub fn reset_frame_snapshot() {
    unsafe { ResetFrameSnapshot() }
}

/// Begin recording the encoded stream to `path`. See `shim/recorder.h` for the
/// return codes.
pub fn recorder_start(
    path: &std::path::Path,
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    is_hdr: bool,
) -> i32 {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // The shim's own codec numbering, which `Codec` already mirrors — kept as an
    // explicit match rather than a cast so a new variant is a compile error here
    // instead of a file that records as the wrong codec.
    let codec_id = match codec {
        Codec::H264 => 0,
        Codec::Hevc => 1,
        Codec::Av1 => 2,
    };
    unsafe {
        RecorderStart(wide.as_ptr(), codec_id, width as i32, height as i32, fps as i32, is_hdr as i32)
    }
}

/// Finish and close the recording. Safe when nothing is recording.
pub fn recorder_stop() -> i32 {
    unsafe { RecorderStop() }
}

/// Whether a recording is open.
pub fn recorder_is_active() -> bool {
    unsafe { RecorderIsActive() == 1 }
}

/// Frames the writer could not keep up with. Nonzero means the recording has
/// gaps; the live stream is unaffected by construction.
pub fn recorder_dropped_frames() -> u64 {
    unsafe { RecorderDroppedFrames() }
}

/// Master feature flag for reference-frame invalidation. While `false`, Nova
/// never advertises RFI to the client (so the client keeps requesting IDRs for
/// loss) and never calls invalidation — the entire path is inert, and loss
/// recovery uses on-demand IDRs exactly as before. Flip to `true` only after
/// live validation under real packet loss: a bug here corrupts the stream
/// rather than degrading it (the frame-index bijection must hold exactly).
pub const RFI_ENABLED: bool = true;

/// Attempt reference-frame invalidation for the client-reported lost range.
/// `true` = NVENC will recover with a P-frame; `false` = the caller must fall
/// back to forcing an IDR. See the shim's `InvalidateRefFrames`.
pub fn invalidate_ref_frames(first_frame: u64, last_frame: u64) -> bool {
    unsafe { InvalidateRefFrames(first_frame, last_frame) == 1 }
}

/// Whether NVENC on this host advertises reference-picture invalidation
/// support. Only meaningful after the first `Encoder::new()`. Gates whether
/// Nova advertises RFI to the client (see rtsp.rs).
pub fn rfi_supported() -> bool {
    unsafe { RfiSupported() == 1 }
}

/// Whether the frame just returned by `encode_frame` is an RFI recovery frame,
/// which must go on the wire as frame type 5 so the client decodes its
/// re-pointed reference correctly. Call immediately after `encode_frame`.
pub fn last_frame_was_rfi_recovery() -> bool {
    unsafe { LastFrameWasRfiRecovery() == 1 }
}

/// Attempt a keyframeless repair: point the next P-frame at a long-term
/// reference the client still holds. `true` = armed, `false` = no usable
/// long-term reference and the caller must force an IDR.
///
/// **Ordering matters at the call sites.** This is the second choice, not the
/// first: reference-frame invalidation is cheaper still (it re-points within
/// the short-term window and needs no marked frame), so the repair ladder is
/// RFI, then LTR, then an IDR. Each rung costs more than the one above it and
/// each is strictly better than the rung below.
pub fn arm_ltr_recovery() -> bool {
    unsafe { ArmLtrRecovery() == 1 }
}

/// Tell the encoder the client has decoded everything up to `frame_index`.
///
/// Only Echo can supply this -- GameStream has no message for it -- and it is
/// what turns LTR recovery from a conservative guess into a provable one: with
/// an acknowledgement the shim references the NEWEST confirmed frame (cheapest
/// repair), without one it falls back to the OLDEST it holds (most likely to
/// have survived). Monotonic in the shim, so a reordered report cannot retire a
/// reference the client demonstrably has.
pub fn notify_ltr_acked(frame_index: u64) {
    unsafe { NotifyLtrAcked(frame_index) }
}

/// Whether long-term references are live for this session. False on AV1, on a
/// GPU that reports too few LTR slots, or when `kEnableLtr` is off in the shim.
pub fn ltr_active() -> bool {
    unsafe { LtrActive() == 1 }
}

/// Whether the frame just returned by `encode_frame` repaired the stream by
/// referencing a long-term reference. Like its RFI twin, it must go on the wire
/// as frame type 5: the client is being handed a P-frame whose reference is not
/// the frame before it, and type 5 is how that is signalled.
pub fn last_frame_was_ltr_recovery() -> bool {
    unsafe { LastFrameWasLtrRecovery() == 1 }
}

/// Pass the log file path (UTF-16, null-terminated) to the C++ shim so that
/// `ShimLog()` writes to the same `nova.log` as the Rust side.  Call this
/// immediately after `debug::init_debug_logger()`, before `Encoder::new()`.
pub fn init_shim_log(log_path_wide: *const u16) {
    unsafe { InitShimLog(log_path_wide); }
    // The shim's handle is a CRT descriptor, which `SetStdHandle` does not
    // reach — so after a log rotation it would keep writing into the file that
    // was renamed away. Registering here means rotation re-points it too.
    crate::debug::set_log_reopen_hook(reopen_shim_log);
}

/// Re-point the shim at the (freshly rotated) log. Must be a bare `fn` so it
/// can be stored as a callback; it recomputes the path rather than capturing.
fn reopen_shim_log() {
    let wide = crate::debug::log_path_wide();
    unsafe { InitShimLog(wide.as_ptr()); }
}

/// Push nova.toml [hdr] luminance parameters to the shim before the first
/// InitEncoder call.  BT.2020 primaries are standard constants and are not
/// configurable; only the panel-specific luminance/CLL/FALL values vary.
/// Authorize the 16-frame DPB tier at 1440p and below. Call BEFORE the first
/// `Encoder::new`, like [`set_hdr_metadata`] -- `InitEncoder` reads it once
/// when it sizes the DPB.
///
/// `false` (the default) keeps every session at a DPB the weakest Moonlight
/// client can decode. See `[stream] allow_level6_dpb` for why this is an
/// operator assertion rather than something negotiated.
pub fn set_deep_dpb_authorized(authorized: bool) {
    unsafe { SetDeepDpbAuthorized(authorized as i32) }
}

pub fn set_hdr_metadata(max_luminance_nits: u16, max_cll_nits: u16, max_fall_nits: u16) {
    unsafe {
        SetHdrMetadata(
            max_luminance_nits as u32,
            max_cll_nits       as u32,
            max_fall_nits      as u32,
        );
    }
}

/// Tell the shim where SDR white sits inside the HDR container, in scRGB units
/// (nits / 80), as reported by `VirtualDisplay::query_sdr_white_level`.
///
/// Only the SDR↔HDR cross-conversion shaders use it — an FP16 capture arrives
/// already composited by Windows at this level, so its path needs no scaling.
/// Getting the two to agree is what stops the picture changing brightness when
/// the capture path switches (a UAC prompt swapping WGC/FP16 for DDA/BGRA8).
/// Applied to the next encoded frame; safe to call from any thread.
pub fn set_sdr_white_level(scrgb_units: f32) {
    SDR_WHITE_SCRGB.store(scrgb_units.to_bits(), Ordering::Relaxed);
    unsafe { SetSdrWhiteLevel(scrgb_units) }
}

/// Rust-side mirror of the shim's `g_sdrWhiteScRGB`, so CPU-side compositing can
/// use the same white level as the conversion shaders. The DDA cursor blend is
/// the one such consumer: it writes 8-bit sRGB cursor pixels straight into an
/// FP16 scRGB frame, and hardcoding 1.0 there (80 nits) drew the cursor at half
/// the brightness of the 160-nit desktop behind it.
static SDR_WHITE_SCRGB: AtomicU32 = AtomicU32::new(0);

/// SDR white in scRGB units (nits / 80), or BT.2408 reference white (203 nits)
/// until `set_sdr_white_level` has run — matching the shim's own default.
pub fn sdr_white_level() -> f32 {
    match SDR_WHITE_SCRGB.load(Ordering::Relaxed) {
        0 => 203.0 / 80.0,
        bits => f32::from_bits(bits),
    }
}

/// Thread-safe IDR trigger callable from any thread (e.g. the control-stream
/// thread when Moonlight requests a keyframe). The shim's `g_force_idr` is a
/// C++ `std::atomic<bool>` and `RequestIdrFrame` ignores its argument, so no
/// handle is needed.
pub fn request_idr_global() {
    unsafe { RequestIdrFrame(std::ptr::null_mut()) };
}

/// Retarget NVENC's CBR rate control to the bitrate the client negotiated in
/// its RTSP ANNOUNCE. Must be called when a client connects: the encoder is
/// created at startup with the CLI default, and CBR holds that rate
/// constantly — exceeding what the client asked for makes Moonlight abort
/// with "lower your bitrate" warnings. Pass fps <= 0 to keep the current rate.
pub fn reconfigure_bitrate(bitrate_kbps: u32, fps: u32) {
    let ret = unsafe { ReconfigureBitrate(bitrate_kbps as i32, fps as i32) };
    if ret < 0 {
        eprintln!("❌ ReconfigureBitrate({} Kbps) failed: {}", bitrate_kbps, ret);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Av1  => "av1",
        }
    }

    /// GameStream ServerCodecModeSupport bitmask contribution.
    /// H264=bit0, HEVC=bit1, AV1=bit8 (matches Sunshine's bitmask).
    #[allow(dead_code)]
    pub fn mode_bit(self) -> u32 {
        match self {
            Codec::H264 => 1,
            Codec::Hevc => 2,
            Codec::Av1  => 256,
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "hevc" | "h265" => Codec::Hevc,
            "av1"           => Codec::Av1,
            _               => Codec::H264,
        }
    }

    /// Derive codec from the `/launch` `videoFormat` bitmask that Limelight
    /// computes from (client `supportedVideoFormats` ∩ server
    /// `ServerCodecModeSupport`).
    ///
    /// Bit layout (moonlight-common-c Limelight.h):
    ///   0x0001 = H264, 0x0002 = HEVC Main, 0x0102 = HEVC Main10,
    ///   0x1000 = AV1 Main8, 0x1100 = AV1 Main10.
    pub fn from_video_format(vf: u32) -> Self {
        if vf & 0x1000 != 0 { Codec::Av1 }
        else if vf & 0x0002 != 0 { Codec::Hevc }
        else { Codec::H264 }
    }
}

pub struct EncoderConfig {
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    pub bitrate_kbps: i32,
    pub codec: Codec,
    pub is_hdr: bool,
}

/// Safe handle around the NVENC C++ shim.
///
/// Owns the encoder session lifetime — `Drop` calls `CleanupEncoder` which also
/// flushes the trailing frames and closes `test.h264`.
pub struct Encoder {
    handle: *mut c_void,
    device_ptr: *mut c_void,
    pub config: EncoderConfig,
}

// The encoder is only driven from the single capture loop thread.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(device: &ID3D11Device, config: EncoderConfig) -> Result<Self, String> {
        unsafe {
            // ID3D11Device is #[repr(transparent)] over a single COM raw pointer;
            // transmute_copy gives us that pointer without consuming the smart wrapper.
            let device_ptr = std::mem::transmute_copy::<ID3D11Device, *mut c_void>(device);
            let mut handle: *mut c_void = std::ptr::null_mut();

            let ret = OpenNvEncSession(device_ptr, &mut handle);
            if ret != 0 {
                return Err(format!("OpenNvEncSession returned {}", ret));
            }

            let codec_cstr = CString::new(config.codec.as_str())
                .expect("codec name is ASCII");

            let ret = InitEncoder(
                handle,
                config.width,
                config.height,
                codec_cstr.as_ptr(),
                config.bitrate_kbps,
                config.fps,
                config.is_hdr as i32,
            );
            if ret != 0 {
                return Err(format!("InitEncoder returned {}", ret));
            }

            let ret = InitColorConversion(device_ptr, config.width, config.height, config.is_hdr as i32, config.fps);
            if ret != 0 {
                return Err(format!("InitColorConversion returned {}", ret));
            }

            Ok(Self { handle, device_ptr, config })
        }
    }

    /// Feed one captured D3D11 texture through the VP→NVENC pipeline.
    ///
    /// `frame_index` is the client-facing wire frame index this frame will be
    /// sent as (see rtp.rs). It becomes NVENC's `inputTimeStamp`, which is what
    /// reference-frame invalidation targets — so it MUST match the index the
    /// client references, and must advance in lockstep with the wire index
    /// (including for frames dropped after encode; see the capture loop).
    ///
    /// Returns the number of encoded bytes written into `out`, or a negative
    /// NVENC error code.
    pub fn encode_frame<T>(&self, texture: &T, out: &mut [u8], frame_index: u64) -> i32 {
        unsafe {
            // T is ID3D11Texture2D — same repr(transparent) COM wrapper trick.
            let tex_ptr = std::mem::transmute_copy::<T, *mut c_void>(texture);
            EncodeFrame(
                self.handle,
                tex_ptr,
                self.config.width,
                self.config.height,
                out.as_mut_ptr(),
                out.len() as i32,
                frame_index,
            )
        }
    }

    /// Raw device pointer — available for future FFI that needs the D3D11 device.
    #[allow(dead_code)]
    pub fn device_ptr(&self) -> *mut c_void {
        self.device_ptr
    }

    /// Force the next encoded frame to be an IDR keyframe with inline SPS/PPS.
    /// Call this when a Moonlight client connects so it doesn't have to wait
    /// up to `idrPeriod` frames for a decodable frame.
    pub fn request_idr(&self) {
        unsafe { RequestIdrFrame(self.handle) };
    }

    /// Tears down this encoder's shim-global NVENC/D3D state
    /// (g_nvEncoder/g_device/g_context etc. in shim.cpp) and marks `self` so
    /// `Drop` becomes a no-op. Idempotent.
    ///
    /// Must be called *before* constructing a replacement `Encoder` on a
    /// capture rebind: `CleanupEncoder` tears down whatever is currently in
    /// those globals, not specifically what this `Encoder` created. If the
    /// replacement's `Encoder::new()` ran first, it would overwrite the
    /// globals with the new encoder's state, and this encoder's `Drop` would
    /// then destroy the brand-new encoder instead of the old one — leaving
    /// `g_nvEncoder`/`g_device` null and every subsequent `EncodeFrame`/
    /// `ReconfigureBitrate` call failing.
    pub fn cleanup(&mut self) {
        if !self.handle.is_null() {
            unsafe { CleanupEncoder(self.handle); }
            self.handle = std::ptr::null_mut();
            self.device_ptr = std::ptr::null_mut();
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks the congestion-signal arithmetic. Every assertion lives in ONE
    /// test on purpose: `STREAM_BITRATE_KBPS`/`CONGESTION_BITRATE_KBPS` are
    /// process-global atomics, and `cargo test` runs test fns in parallel
    /// threads — split across two tests these would race each other.
    ///
    /// No FFI is touched here (no `reconfigure_bitrate`), so this runs without
    /// a GPU, an encoder session, or nova_shim.dll being loadable.
    #[test]
    fn congestion_signal_cuts_20_percent_and_collapses_bursts() {
        // No session (bitrate 0) ⇒ signalling must be a no-op. This is exactly
        // the state the Worker was stuck in before it published its bitrate,
        // which silently disabled dynamic QoS for the whole split deployment.
        set_stream_bitrate_kbps(0);
        let _ = take_congestion_bitrate(); // clear any leftover
        signal_congestion_reduction();
        assert_eq!(take_congestion_bitrate(), None, "no session ⇒ no reduction");

        // Active session: a loss report cuts to 80% of the current target.
        set_stream_bitrate_kbps(90_400);
        signal_congestion_reduction();
        // A burst of further reports must collapse into the SAME pending
        // reduction rather than stacking down toward the floor.
        signal_congestion_reduction();
        signal_congestion_reduction();
        assert_eq!(take_congestion_bitrate(), Some(72_320));

        // Claiming clears the signal — the ramp-back branch relies on this.
        assert_eq!(take_congestion_bitrate(), None);

        // The cut is floored so congestion can never collapse the stream to
        // an unusable bitrate.
        set_stream_bitrate_kbps(1_100);
        signal_congestion_reduction();
        assert_eq!(take_congestion_bitrate(), Some(1_000));

        set_stream_bitrate_kbps(0); // leave the global clean for other tests
    }
}
