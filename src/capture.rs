/// Windows Graphics Capture (WGC) backend — replaces `IDXGIOutputDuplication`.
///
/// ## Why WGC instead of DXGI Desktop Duplication
///
/// `IDXGIOutputDuplication` exposes the raw OS framebuffer at the DXGI layer.
/// Any display colour-mode transition — including the FP16 / Advanced Color
/// switch needed for HDR10 capture from the MttVDD virtual display — invalidates
/// the duplication handle (`DXGI_ERROR_ACCESS_LOST`) and the caller must
/// re-duplicate. On IddCx 1.2 (all shipping MttVDD versions) the FP16 mode
/// transition is NOT stable: it fires 50+ consecutive ACCESS_LOST events during
/// live streaming, exhausting Moonlight's 7-second connection timeout before any
/// HDR frame can be delivered.
///
/// WGC sits above the DXGI layer at the DWM composition level. Its
/// `Direct3D11CaptureFramePool` buffers frames on the caller's D3D11 device and
/// absorbs display mode transitions internally. The caller never sees
/// ACCESS_LOST — frames simply pause for a moment and then resume in the new
/// format (FP16 when Advanced Color is active, BGRA8 otherwise).
///
/// ## Zero-copy guarantee
///
/// The frame pool is created with the same `ID3D11Device` used by the NVENC
/// encoder. WGC delivers captured textures on that device, so every
/// `ID3D11Texture2D` returned by `try_get_frame` can be passed directly to the
/// shim's `ID3D11VideoContext` VP blt without any system-RAM round-trip.
///
/// ## HDR pipeline
///
/// When `is_hdr = true`, the pool is created with
/// `DirectXPixelFormat::R16G16B16A16Float`. After the VDD's Advanced Color mode
/// is enabled via `DisplayConfigSetDeviceInfo(SET_ADVANCED_COLOR_STATE)`, WGC
/// delivers true FP16 scRGB frames that the shim converts to P010 BT.2020 PQ
/// for HEVC Main10 encoding.

use windows::core::{Interface, Result, IInspectable};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool,
    GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{BOOL, HMODULE, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint,
    HDC, HMONITOR, MONITORINFO, MONITOR_FROM_FLAGS,
};
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

/// MONITORINFOEXW is not reliably exported by windows-rs across patch versions,
/// so we define the ABI layout manually. Win32 spec: `MONITORINFO` (40 bytes)
/// followed by `szDevice[CCHDEVICENAME]` where CCHDEVICENAME = 32 UTF-16 units.
#[repr(C)]
struct MonitorInfoEx {
    base:   MONITORINFO,
    device: [u16; 32],
}

// ── Module-level EnumDisplayMonitors callbacks ────────────────────────────────
// These must be free functions (not closures) because their address is passed
// as a raw function pointer through the Win32 MONITORENUMPROC type alias.

struct FindByName { target: String, result: Option<HMONITOR> }

unsafe extern "system" fn enum_find_by_name(
    hmon: HMONITOR, _: HDC, _: *mut RECT, lparam: LPARAM,
) -> BOOL {
    let s = &mut *(lparam.0 as *mut FindByName);
    let mut info = MonitorInfoEx {
        base:   MONITORINFO { cbSize: std::mem::size_of::<MonitorInfoEx>() as u32,
                              ..Default::default() },
        device: [0u16; 32],
    };
    if GetMonitorInfoW(hmon, &mut info.base).as_bool() {
        let name = String::from_utf16_lossy(&info.device);
        if name.trim_end_matches('\0').eq_ignore_ascii_case(&s.target) {
            s.result = Some(hmon);
            return BOOL(0); // stop enumeration
        }
    }
    BOOL(1)
}

struct FindExcluding { exclude: String, result: Option<HMONITOR> }

unsafe extern "system" fn enum_find_excluding(
    hmon: HMONITOR, _: HDC, _: *mut RECT, lparam: LPARAM,
) -> BOOL {
    let s = &mut *(lparam.0 as *mut FindExcluding);
    let mut info = MonitorInfoEx {
        base:   MONITORINFO { cbSize: std::mem::size_of::<MonitorInfoEx>() as u32,
                              ..Default::default() },
        device: [0u16; 32],
    };
    if GetMonitorInfoW(hmon, &mut info.base).as_bool() {
        let name = String::from_utf16_lossy(&info.device);
        if !name.trim_end_matches('\0').eq_ignore_ascii_case(&s.exclude) {
            s.result = Some(hmon);
            return BOOL(0);
        }
    }
    BOOL(1)
}

// ─────────────────────────────────────────────────────────────────────────────

pub struct WgcCapturer {
    /// D3D11 device used by both the WGC frame pool and the NVENC encoder.
    /// Always created on the primary hardware GPU (adapter 0 / NVIDIA).
    /// WGC handles any required cross-adapter copy internally so this device
    /// can capture from any monitor regardless of which DXGI adapter it lives on.
    pub device:   ID3D11Device,
    pub width:    u32,
    pub height:   u32,
    /// Desktop-coordinate origin of the captured monitor — used by `input.rs`
    /// to map Moonlight's client-relative mouse coordinates correctly.
    pub origin_x: i32,
    pub origin_y: i32,

    wrt_device:  windows::Graphics::DirectX::Direct3D11::IDirect3DDevice,
    item:        GraphicsCaptureItem,   // kept alive for session lifetime
    frame_pool:  Direct3D11CaptureFramePool,
    session:     GraphicsCaptureSession,
    is_hdr:      bool,
    /// Last successfully captured frame, copied to a stable `USAGE_DEFAULT`
    /// texture so the WGC pool buffer can be freed immediately. Re-submitted
    /// to the encoder when `try_get_frame` returns `None` (desktop unchanged)
    /// to keep the stream alive on a static desktop. Cleared on `rebind`.
    last_frame:  Option<ID3D11Texture2D>,
}

impl WgcCapturer {
    // ── Construction ──────────────────────────────────────────────────────────

    /// Starts a WGC capture session on the primary monitor, excluding the
    /// virtual display device named `exclude` (same semantics as the old
    /// `DesktopCapturer::new_excluding`). Always starts in SDR mode; the
    /// caller upgrades to HDR via `rebind(..., is_hdr=true, ...)` once the
    /// client negotiates HEVC Main10.
    pub fn new_excluding(exclude: Option<&str>) -> Result<Self> {
        unsafe { let _ = RoInitialize(RO_INIT_MULTITHREADED); }

        let hmonitor = match exclude {
            Some(ex) => Self::first_monitor_excluding(ex)
                            .unwrap_or_else(Self::primary_hmonitor),
            None     => Self::primary_hmonitor(),
        };

        let device     = Self::create_d3d11_device()?;
        let wrt_device = Self::wrap_d3d11_device(&device)?;
        Self::open_session(device, wrt_device, hmonitor, false)
    }

    /// Re-targets capture to `gdi_device_name` (or the physical primary when
    /// `None`) and recreates the frame pool with the correct pixel format for
    /// `is_hdr`. Returns `Ok(true)` when the encoder must be recreated
    /// (resolution changed) — same contract as the old DXGI `rebind`.
    ///
    /// `expected_size`: when `Some((w, h))`, polls `GetMonitorInfoW` for up to
    /// 3 seconds until the VDD reports that size before opening the WGC session.
    /// This closes the race where `SetDisplayConfig`/`force_resolution` is still
    /// settling when `rebind` fires, causing WGC to latch the transitional size.
    /// The HMONITOR is re-resolved each iteration because CCD topology changes
    /// can reassign handle values.
    pub fn rebind(
        &mut self,
        gdi_device_name: Option<&str>,
        is_hdr: bool,
        expected_size: Option<(u32, u32)>,
    ) -> Result<bool> {
        unsafe { let _ = RoInitialize(RO_INIT_MULTITHREADED); }

        let mut hmonitor = gdi_device_name
            .and_then(Self::hmonitor_from_device_name)
            .unwrap_or_else(Self::primary_hmonitor);

        if let Some((exp_w, exp_h)) = expected_size {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                let (w, h) = unsafe {
                    let mut info = MONITORINFO {
                        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                        ..Default::default()
                    };
                    let _ = GetMonitorInfoW(hmonitor, &mut info);
                    let r = info.rcMonitor;
                    ((r.right - r.left) as u32, (r.bottom - r.top) as u32)
                };
                if w == exp_w && h == exp_h {
                    println!("✅ VDD settled at {w}×{h} — proceeding with WGC bind");
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    println!("⚠️  VDD still {w}×{h} after 3 s (expected {exp_w}×{exp_h}) — binding anyway");
                    break;
                }
                println!("   rebind: VDD at {w}×{h}, waiting for {exp_w}×{exp_h}...");
                std::thread::sleep(std::time::Duration::from_millis(100));
                // Re-resolve — HMONITOR handles can be reassigned after CCD topology changes.
                hmonitor = gdi_device_name
                    .and_then(Self::hmonitor_from_device_name)
                    .unwrap_or_else(Self::primary_hmonitor);
            }
        }

        // Build the new session — keep the same D3D11 device throughout the
        // process lifetime (always adapter 0 / primary GPU).
        let new = Self::open_session(
            self.device.clone(),
            self.wrt_device.clone(),
            hmonitor,
            is_hdr,
        )?;

        let resized = new.width != self.width || new.height != self.height;

        // Swap in the new session (old session/pool dropped → capture stops).
        self.item       = new.item;
        self.frame_pool = new.frame_pool;
        self.session    = new.session;
        self.width      = new.width;
        self.height     = new.height;
        self.origin_x   = new.origin_x;
        self.origin_y   = new.origin_y;
        self.is_hdr     = is_hdr;
        self.last_frame = None;

        Ok(resized)
    }

    // ── Per-frame API ─────────────────────────────────────────────────────────

    /// Polls the WGC frame pool for the next available frame.
    ///
    /// Returns `Some(texture)` where `texture` is a **stable D3D11_USAGE_DEFAULT
    /// copy** owned by this capturer — NOT the ephemeral WGC pool surface.
    ///
    /// WGC pool surfaces use cross-process / keyed-mutex shared memory that DWM
    /// reclaims the instant `Direct3D11CaptureFrame` drops, which can invalidate
    /// the GPU mapping before `encode_frame` reads it. By copying to our own
    /// texture and calling `Flush()` before releasing the frame, we guarantee the
    /// encoder always reads from a stable, pool-independent buffer.
    ///
    /// Returns `None` when no new frame is available (desktop unchanged), or when
    /// a resolution change is detected (pool recreated; updated `self.width`/
    /// `self.height` tell the caller to rebind the encoder).
    pub fn try_get_frame(&mut self) -> Option<ID3D11Texture2D> {
        let frame = self.frame_pool.TryGetNextFrame().ok()?;

        let size = frame.ContentSize().ok()?;
        if size.Width as u32 != self.width || size.Height as u32 != self.height {
            self.width  = size.Width  as u32;
            self.height = size.Height as u32;
            let _ = self.frame_pool.Recreate(
                &self.wrt_device,
                Self::pixel_format(self.is_hdr),
                2,
                SizeInt32 { Width: size.Width, Height: size.Height },
            );
            return None;
        }

        // Pool surface → ID3D11Texture2D (keyed-mutex / cross-process shared).
        let surface = frame.Surface().ok()?;
        let access  = surface.cast::<IDirect3DDxgiInterfaceAccess>().ok()?;
        let pool_tex: ID3D11Texture2D = unsafe { access.GetInterface().ok()? };

        // Copy pool surface → stable D3D11_USAGE_DEFAULT cache, then Flush so
        // the CopyResource command is dispatched to the GPU before we drop the
        // WGC frame below. The driver holds a GPU-side reference after Flush, so
        // the copy completes correctly even after DWM reclaims the pool buffer.
        self.cache_frame(&pool_tex).ok()?;
        unsafe { if let Ok(ctx) = self.device.GetImmediateContext() { ctx.Flush(); } }

        // frame and pool_tex drop here → WGC pool buffer returned to DWM.
        // Return a clone of our stable cached copy — fully independent of pool.
        self.last_frame.clone()
    }

    /// The last successfully captured frame — re-submitted to the encoder when
    /// `try_get_frame` returns `None` (desktop unchanged) to keep the stream
    /// alive on a static desktop.
    pub fn cached_texture(&self) -> Option<&ID3D11Texture2D> {
        self.last_frame.as_ref()
    }

    /// True once this WGC session has delivered at least one frame.
    /// Resets to false on every `rebind` (new session, new device name, or
    /// HDR format change). Used by the capture loop to gate:
    ///   (a) cached-frame re-submission — never encode a stale texture from a
    ///       previous session whose format/size may not match the current encoder.
    ///   (b) the damage-generator jiggle — only jiggle while the VDD is still
    ///       completely empty; stop once real frames are flowing.
    pub fn has_frame(&self) -> bool {
        self.last_frame.is_some()
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Creates the hardware D3D11 device (adapter 0 / primary GPU) that is
    /// shared between the WGC frame pool and the NVENC encoder for the entire
    /// process lifetime.
    fn create_d3d11_device() -> Result<ID3D11Device> {
        unsafe {
            let mut device = None;
            D3D11CreateDevice(
                None,                           // let the system pick the primary GPU
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            ).map_err(|e| {
                println!(
                    "[Capture] ❌ D3D11CreateDevice FAILED: HRESULT=0x{:08X}  {:?}\n\
                     [Capture]    Common causes in a Windows Service:\n\
                     [Capture]      0x887A0004 = DXGI_ERROR_DEVICE_REMOVED (GPU reset/TDR)\n\
                     [Capture]      0x80004005 = E_FAIL (no GPU visible from Session 0)\n\
                     [Capture]      0x80070005 = E_ACCESSDENIED (Session 0 GPU isolation)\n\
                     [Capture]    Nova must run as the logged-on user, not as a SYSTEM service.",
                    e.code().0 as u32, e
                );
                e
            })?;
            Ok(device.unwrap())
        }
    }

    /// Wraps an `ID3D11Device` in a WinRT `IDirect3DDevice` (required by the
    /// WGC `Direct3D11CaptureFramePool` APIs).
    fn wrap_d3d11_device(
        device: &ID3D11Device,
    ) -> Result<windows::Graphics::DirectX::Direct3D11::IDirect3DDevice> {
        unsafe {
            let dxgi: IDXGIDevice = device.cast()?;
            let inspectable: IInspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?;
            inspectable.cast()
        }
    }

    /// Builds a complete WGC pipeline (item → pool → session) for `hmonitor`.
    fn open_session(
        device:     ID3D11Device,
        wrt_device: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice,
        hmonitor:   HMONITOR,
        is_hdr:     bool,
    ) -> Result<Self> {
        // Monitor geometry — origin + dimensions
        let (origin_x, origin_y, width, height) = unsafe {
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(hmonitor, &mut info);
            let r = info.rcMonitor;
            (r.left, r.top, (r.right - r.left) as u32, (r.bottom - r.top) as u32)
        };

        // Capture item from HMONITOR via the Win32 interop interface.
        // During a CCD topology transition the HMONITOR can be momentarily
        // invalid, causing CreateForMonitor to return E_INVALIDARG (0x80070057).
        // Retry up to 10 × 100 ms so the VDD has time to fully settle.
        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        const E_INVALIDARG: windows::core::HRESULT = windows::core::HRESULT(0x80070057_u32 as i32);
        let item: GraphicsCaptureItem = {
            let mut last_err: Option<windows::core::Error> = None;
            let mut found: Option<GraphicsCaptureItem> = None;
            for attempt in 0..10u32 {
                match unsafe { interop.CreateForMonitor(hmonitor) } {
                    Ok(i) => { found = Some(i); break; }
                    Err(e) if e.code() == E_INVALIDARG => {
                        println!("⚠️  CreateForMonitor E_INVALIDARG (attempt {}/10) — VDD still transitioning, retrying...", attempt + 1);
                        last_err = Some(e);
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    Err(e) => return Err(e),
                }
            }
            match found {
                Some(i) => i,
                None => return Err(last_err.unwrap()),
            }
        };

        // Frame pool — same D3D11 device as NVENC; WGC handles cross-adapter copy.
        let size = SizeInt32 { Width: width as i32, Height: height as i32 };
        let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &wrt_device,
            Self::pixel_format(is_hdr),
            2,    // two-frame pool: one being encoded, one being filled
            size,
        )?;

        let session = frame_pool.CreateCaptureSession(&item)?;
        // WGC composites the system cursor directly into the captured frame in
        // the display's native colour space (FP16 in HDR mode, BGRA8 in SDR).
        // The shim cursor-compositing pipeline is left idle (no update_cursor_*
        // calls from the WGC loop), avoiding double compositing.
        session.SetIsCursorCaptureEnabled(true)?;
        session.StartCapture()?;

        println!("✅ WGC capture session started ({}x{} {})",
            width, height, if is_hdr { "FP16/HDR" } else { "BGRA8/SDR" });

        Ok(Self {
            device, wrt_device, item, frame_pool, session,
            width, height, origin_x, origin_y,
            is_hdr, last_frame: None,
        })
    }

    fn pixel_format(is_hdr: bool) -> DirectXPixelFormat {
        if is_hdr {
            // WGC only supports two frame-pool formats: BGRA8 (SDR) and FP16 (HDR).
            // R10G10B10A2 is NOT a valid WGC pool format — requesting it causes
            // CreateFreeThreaded to fail with E_INVALIDARG immediately.
            // WGC delivers the FP16 linear-scRGB surface that DWM composites to,
            // tagged DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709. The shim's D3D11
            // Video Processor converts FP16 scRGB → P010 BT.2020 PQ for NVENC.
            DirectXPixelFormat::R16G16B16A16Float
        } else {
            DirectXPixelFormat::B8G8R8A8UIntNormalized
        }
    }

    /// Copies `texture` to a stable `D3D11_USAGE_DEFAULT` local texture so the
    /// WGC pool buffer can be freed (by dropping `Direct3D11CaptureFrame`)
    /// independently of when the encoder consumes the data.
    fn cache_frame(&mut self, texture: &ID3D11Texture2D) -> Result<()> {
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);

            if self.last_frame.is_none() {
                let cached_desc = D3D11_TEXTURE2D_DESC {
                    Usage:          D3D11_USAGE_DEFAULT,
                    BindFlags:      D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags:      0,
                    ..desc
                };
                let mut cache = None;
                self.device.CreateTexture2D(&cached_desc, None, Some(&mut cache))?;
                self.last_frame = cache;
            }

            let ctx = self.device.GetImmediateContext()?;
            ctx.CopyResource(self.last_frame.as_ref().unwrap(), texture);
        }
        Ok(())
    }

    // ── HMONITOR resolution helpers ───────────────────────────────────────────

    fn primary_hmonitor() -> HMONITOR {
        unsafe {
            MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_FROM_FLAGS(1)) // MONITOR_DEFAULTTOPRIMARY
        }
    }

    fn hmonitor_from_device_name(target: &str) -> Option<HMONITOR> {
        let mut s = FindByName { target: target.to_string(), result: None };
        unsafe {
            let _ = EnumDisplayMonitors(
                HDC(std::ptr::null_mut()), None,
                Some(enum_find_by_name),
                LPARAM(&mut s as *mut _ as isize),
            );
        }
        s.result
    }

    fn first_monitor_excluding(exclude: &str) -> Option<HMONITOR> {
        let mut s = FindExcluding { exclude: exclude.to_string(), result: None };
        unsafe {
            let _ = EnumDisplayMonitors(
                HDC(std::ptr::null_mut()), None,
                Some(enum_find_excluding),
                LPARAM(&mut s as *mut _ as isize),
            );
        }
        s.result
    }
}
