use std::ffi::{c_void, CString};
use std::sync::atomic::{AtomicI32, Ordering};
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
    ) -> i32;
    fn CleanupEncoder(encoder: *mut c_void) -> i32;
    fn RequestIdrFrame(encoder: *mut c_void);
    fn ReconfigureBitrate(bitrate_kbps: i32, fps: i32) -> i32;

}

/// Pass the log file path (UTF-16, null-terminated) to the C++ shim so that
/// `ShimLog()` writes to the same `nova.log` as the Rust side.  Call this
/// immediately after `debug::init_debug_logger()`, before `Encoder::new()`.
pub fn init_shim_log(log_path_wide: *const u16) {
    unsafe { InitShimLog(log_path_wide); }
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
    /// Returns the number of encoded bytes written into `out`, or a negative
    /// NVENC error code.  The shim also mirrors each packet to `test.h264`.
    pub fn encode_frame<T>(&self, texture: &T, out: &mut [u8]) -> i32 {
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
