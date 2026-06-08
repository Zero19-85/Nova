use std::ffi::{c_void, CString};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

extern "C" {
    fn OpenNvEncSession(d3d11_device: *mut c_void, out_encoder: *mut *mut c_void) -> i32;
    fn InitEncoder(
        encoder: *mut c_void,
        width: i32,
        height: i32,
        codec: *const std::ffi::c_char,
        bitrate_kbps: i32,
        fps: i32,
    ) -> i32;
    fn InitColorConversion(device: *mut c_void, width: i32, height: i32) -> i32;
    fn EncodeFrame(
        encoder: *mut c_void,
        d3d11_texture: *mut c_void,
        width: i32,
        height: i32,
        out_buffer: *mut u8,
        max_size: i32,
    ) -> i32;
    fn CleanupEncoder(encoder: *mut c_void) -> i32;
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
}

pub struct EncoderConfig {
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    pub bitrate_kbps: i32,
    pub codec: Codec,
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
            );
            if ret != 0 {
                return Err(format!("InitEncoder returned {}", ret));
            }

            let ret = InitColorConversion(device_ptr, config.width, config.height);
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

    /// Raw device pointer — needed by InitColorConversion and passed back to C++.
    pub fn device_ptr(&self) -> *mut c_void {
        self.device_ptr
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            CleanupEncoder(self.handle);
        }
    }
}
