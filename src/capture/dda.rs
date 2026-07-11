//! DXGI Desktop Duplication (DDA) backend — **secure-desktop fallback**.
//!
//! ## Role
//!
//! Activated by [`super::DesktopManager`] ONLY while the secure desktop
//! (`WinSta0\Winlogon` — UAC prompts, logon screen, Ctrl+Alt+Del) is up. WGC is
//! bound to the interactive desktop and delivers nothing there; DDA duplicates
//! the *output* (monitor), which shows whatever desktop is active. WGC resumes
//! the moment the input desktop returns to `Default`.
//!
//! ## Why a dedicated capture thread (the load-bearing design point)
//!
//! Duplicating the secure desktop needs a thread that (a) is SYSTEM (the
//! Winlogon desktop's ACL admits only SYSTEM) and (b) has called
//! `SetThreadDesktop(Winlogon)`. Neither can be done on Nova's main capture
//! thread:
//!
//! - It runs as the interactive USER (the host must — WGC, Nova's primary
//!   backend, fails under SYSTEM). Impersonating SYSTEM per-call is possible…
//! - …but `SetThreadDesktop` fails with `ERROR_BUSY` on any thread that already
//!   has windows or hooks on its desktop, and the main thread does (COM/WGC put
//!   hidden message windows there). Confirmed live: `0x800700AA`.
//!
//! So the duplication runs on its own **fresh** thread (`nova-dda-secure`) that
//! has no windows: it `ImpersonateLoggedOnUser`s the SYSTEM-in-console-session
//! token the launcher service handed the host, `SetThreadDesktop(Winlogon)`
//! (now succeeds), creates the duplication, and copies each frame into a CPU
//! buffer. The main encode loop uploads that buffer to an encoder-device
//! texture and encodes it — so the two threads never share a D3D device
//! context, and the main thread's identity/desktop are never touched (WGC is
//! completely unaffected). When the thread ends, its impersonation and desktop
//! association die with it — no cleanup dance on the main thread.
//!
//! The SYSTEM token comes from `crate::service::system_impersonation_token()`
//! (the service duplicates its own LocalSystem token, retargets it to the
//! console session, and passes it to the host). Without it (task/manual launch)
//! the desktop attach fails and the manager stays on WGC — graceful, never
//! worse than a frozen frame.
//!
//! ## Frame path
//!
//! The capture thread always creates its **own** D3D11 device on the output's
//! adapter (never shares the encoder's device/context across threads) and
//! bounces frames through a CPU staging buffer. That is a system-RAM round-trip
//! per frame, but this backend only lives for the seconds a UAC prompt is up,
//! and it keeps the WGC hot path and the encoder device single-threaded.
//!
//! ## Cursor
//!
//! DDA delivers the cursor as separate pointer metadata rather than compositing
//! it into the frame. The capture thread reads the pointer position/visibility
//! from `DXGI_OUTDUPL_FRAME_INFO` + the shape from `GetFramePointerShape` and
//! blends it into each CPU frame (`blend_cursor`, all three shape types) so the
//! secure-desktop stream shows the mouse cursor.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::core::{Interface, HRESULT};
use windows::Win32::Foundation::{E_ACCESSDENIED, HMODULE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutput5,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTPUT_DESC,
};
use windows::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS,
};

/// `GENERIC_ALL` expressed as desktop access rights — what `SetThreadDesktop`
/// needs on the target desktop (Sunshine's `syncThreadDesktop` uses the same).
const DESKTOP_GENERIC_ALL: DESKTOP_ACCESS_FLAGS = DESKTOP_ACCESS_FLAGS(0x1000_0000);

/// A captured frame handed from the capture thread to the main encode loop as
/// CPU bytes (raw mapped rows, honoring `row_pitch`) plus its geometry/format.
struct CpuFrame {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    row_pitch: u32,
    format: DXGI_FORMAT,
}

/// Result the capture thread reports once, after it has (or hasn't) managed to
/// create the duplication: the captured geometry, or an error string.
type InitResult = Result<CaptureGeometry, String>;

#[derive(Clone, Copy)]
struct CaptureGeometry {
    width: u32,
    height: u32,
    origin_x: i32,
    origin_y: i32,
}

/// Shared between the capture thread and the main loop.
struct DdaShared {
    /// Latest captured frame; `take`n by the main loop each iteration. `None`
    /// means "no new frame since last read" (static secure desktop).
    frame: Mutex<Option<CpuFrame>>,
    /// Set by the main loop to ask the capture thread to stop.
    stop: AtomicBool,
    /// Set by the capture thread on `DXGI_ERROR_ACCESS_LOST`.
    access_lost: AtomicBool,
}

/// DXGI Desktop Duplication capturer. Owns the dedicated capture thread; the
/// public surface matches the WGC backend so [`super::DesktopManager`] can treat
/// them interchangeably.
pub struct DdaCapturer {
    /// The NVENC-shared device — the cache texture returned to the caller lives
    /// here (the zero-copy-into-NVENC contract; the CPU→GPU upload happens on
    /// the main thread, never cross-thread).
    encoder_device: ID3D11Device,
    shared: Arc<DdaShared>,
    thread: Option<JoinHandle<()>>,
    /// Stable cache on `encoder_device` — what callers receive; re-uploaded from
    /// each new CPU frame on the main thread.
    last_frame: Option<ID3D11Texture2D>,
    cache_dims: Option<(u32, u32, DXGI_FORMAT)>,
    width: u32,
    height: u32,
    origin_x: i32,
    origin_y: i32,
    is_hdr: bool,
    target: Option<String>,
}

impl DdaCapturer {
    /// Start duplicating the output currently showing `gdi_device_name` (or the
    /// primary output when `None`) on a dedicated SYSTEM-impersonating thread.
    /// Blocks up to 3 s for the thread to report the duplication is live; on
    /// failure the thread is joined and the error returned (the manager then
    /// stays on WGC and retries on a cooldown).
    pub fn new(
        encoder_device: ID3D11Device,
        gdi_device_name: Option<&str>,
        is_hdr: bool,
    ) -> Result<Self, String> {
        let shared = Arc::new(DdaShared {
            frame: Mutex::new(None),
            stop: AtomicBool::new(false),
            access_lost: AtomicBool::new(false),
        });
        let target = gdi_device_name.map(str::to_owned);

        let (init_tx, init_rx) = mpsc::channel::<InitResult>();
        let thread = {
            let shared = shared.clone();
            let target = target.clone();
            thread::Builder::new()
                .name("nova-dda-secure".into())
                .spawn(move || capture_thread_main(shared, target, is_hdr, init_tx))
                .map_err(|e| format!("failed to spawn DDA capture thread: {e}"))?
        };

        match init_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(geo)) => Ok(Self {
                encoder_device,
                shared,
                thread: Some(thread),
                last_frame: None,
                cache_dims: None,
                width: geo.width,
                height: geo.height,
                origin_x: geo.origin_x,
                origin_y: geo.origin_y,
                is_hdr,
                target,
            }),
            Ok(Err(e)) => {
                shared.stop.store(true, Ordering::Release);
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                shared.stop.store(true, Ordering::Release);
                let _ = thread.join();
                Err("DDA capture thread did not initialize within 3 s".to_string())
            }
        }
    }

    /// Stop the capture thread (signals stop + joins). Idempotent; also runs on
    /// drop. The thread's impersonation + secure-desktop association are released
    /// automatically when it exits.
    pub fn release(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }

    /// True after the capture thread saw `DXGI_ERROR_ACCESS_LOST` (desktop
    /// switch / mode change). The manager either restores or swaps back to WGC.
    pub fn access_lost(&self) -> bool {
        self.shared.access_lost.load(Ordering::Acquire)
    }

    /// Rebuild the duplication after ACCESS_LOST (still on the secure desktop).
    pub fn try_restore(&mut self) -> Result<(), String> {
        self.release();
        let rebuilt = Self::new(self.encoder_device.clone(), self.target.as_deref(), self.is_hdr)?;
        *self = rebuilt;
        Ok(())
    }

    /// Poll for the next duplicated frame — same contract as the WGC backend.
    /// Uploads the latest CPU frame (if any) to the encoder-device cache on THIS
    /// (main) thread and returns it; `None` when no new frame is available.
    pub fn try_get_frame(&mut self) -> Option<ID3D11Texture2D> {
        let frame = self.shared.frame.lock().ok()?.take()?;
        self.upload_to_cache(&frame).ok()?;
        self.last_frame.clone()
    }

    pub fn cached_texture(&self) -> Option<&ID3D11Texture2D> {
        self.last_frame.as_ref()
    }

    pub fn has_frame(&self) -> bool {
        self.last_frame.is_some()
    }

    /// Re-target/re-format the duplication. `Ok(true)` = resolution changed.
    pub fn rebind(
        &mut self,
        gdi_device_name: Option<&str>,
        is_hdr: bool,
        _expected_size: Option<(u32, u32)>,
    ) -> windows::core::Result<bool> {
        let (old_w, old_h) = (self.width, self.height);
        self.release();
        match Self::new(self.encoder_device.clone(), gdi_device_name, is_hdr) {
            Ok(new) => {
                let resized = new.width != old_w || new.height != old_h;
                *self = new;
                Ok(resized)
            }
            Err(e) => {
                println!("⚠️  DDA rebind failed: {e}");
                Err(windows::core::Error::from(E_ACCESSDENIED))
            }
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn origin(&self) -> (i32, i32) {
        (self.origin_x, self.origin_y)
    }
    pub fn device(&self) -> &ID3D11Device {
        &self.encoder_device
    }

    /// Upload a CPU frame into the stable encoder-device cache texture
    /// (recreated when the geometry/format changes). Runs on the main thread —
    /// the only thread that touches the encoder device context.
    fn upload_to_cache(&mut self, frame: &CpuFrame) -> windows::core::Result<()> {
        unsafe {
            let dims = (frame.width, frame.height, frame.format);
            if self.cache_dims != Some(dims) {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: frame.width,
                    Height: frame.height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: frame.format,
                    SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut cache = None;
                self.encoder_device.CreateTexture2D(&desc, None, Some(&mut cache))?;
                self.last_frame = cache;
                self.cache_dims = Some(dims);
            }
            if let Some(cache) = self.last_frame.as_ref() {
                let ctx = self.encoder_device.GetImmediateContext()?;
                ctx.UpdateSubresource(
                    cache,
                    0,
                    None,
                    frame.bytes.as_ptr() as *const _,
                    frame.row_pitch,
                    0,
                );
                ctx.Flush();
            }
        }
        Ok(())
    }
}

impl Drop for DdaCapturer {
    fn drop(&mut self) {
        self.release();
    }
}

// ── Capture thread ────────────────────────────────────────────────────────────

/// Body of the `nova-dda-secure` thread: assume the SYSTEM token, attach to the
/// secure desktop, create the duplication, then loop acquiring frames into the
/// shared slot. Reports the outcome of setup once via `init_tx`.
fn capture_thread_main(
    shared: Arc<DdaShared>,
    target: Option<String>,
    is_hdr: bool,
    init_tx: mpsc::Sender<InitResult>,
) {
    unsafe {
        // 1. Assume the SYSTEM-in-console-session token so the secure desktop's
        //    ACL admits us. Fresh thread ⇒ no windows ⇒ SetThreadDesktop works.
        let impersonating = match crate::service::system_impersonation_token() {
            Some(tok) => ImpersonateLoggedOnUser(tok).is_ok(),
            None => false,
        };

        // 2. Attach THIS thread to the input (secure) desktop.
        let attached = match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_GENERIC_ALL) {
            Ok(hdesk) => match SetThreadDesktop(hdesk) {
                Ok(()) => true,
                Err(e) => {
                    println!("   ↳ DDA thread: SetThreadDesktop failed: {e:?}");
                    let _ = CloseDesktop(hdesk);
                    false
                }
            },
            Err(e) => {
                println!("   ↳ DDA thread: OpenInputDesktop failed: {e:?}");
                false
            }
        };
        println!(
            "🔐 DDA capture thread: impersonating SYSTEM={impersonating}, attached to input desktop={attached}"
        );

        // 3. Build the duplication (own device on the output's adapter).
        let session = match setup_duplication(target.as_deref(), is_hdr) {
            Ok(s) => s,
            Err(e) => {
                let _ = init_tx.send(Err(e));
                if impersonating {
                    let _ = RevertToSelf();
                }
                return;
            }
        };
        let _ = init_tx.send(Ok(session.geometry));

        // 4. Acquire loop.
        run_acquire_loop(&shared, &session);

        if impersonating {
            let _ = RevertToSelf();
        }
        // Thread exit releases the secure-desktop association automatically.
    }
}

/// Everything the acquire loop needs, all owned by the capture thread.
struct DuplicationSession {
    dup: IDXGIOutputDuplication,
    device: ID3D11Device,
    geometry: CaptureGeometry,
}

unsafe fn setup_duplication(target: Option<&str>, is_hdr: bool) -> Result<DuplicationSession, String> {
    let (output, out_desc) = find_output(target)?;
    let out_adapter: IDXGIAdapter1 = output
        .GetParent()
        .map_err(|e| format!("IDXGIOutput::GetParent failed: {e:?}"))?;

    // Always a private device on the output's adapter — the capture thread owns
    // it exclusively, so the encoder device context is never touched off-thread.
    let device = create_device_on_adapter(&out_adapter)?;

    let requested_format: DXGI_FORMAT = if is_hdr {
        DXGI_FORMAT_R16G16B16A16_FLOAT
    } else {
        DXGI_FORMAT_B8G8R8A8_UNORM
    };
    let dup = duplicate_output(&output, &device, requested_format, is_hdr)?;

    let r = out_desc.DesktopCoordinates;
    let geometry = CaptureGeometry {
        width: (r.right - r.left) as u32,
        height: (r.bottom - r.top) as u32,
        origin_x: r.left,
        origin_y: r.top,
    };
    println!(
        "✅ DDA duplication active on {} ({}x{} {})",
        device_name_of(&out_desc),
        geometry.width,
        geometry.height,
        if is_hdr { "FP16/HDR" } else { "BGRA8/SDR" },
    );

    Ok(DuplicationSession { dup, device, geometry })
}

/// Cached hardware-cursor shape from `GetFramePointerShape` (DDA delivers the
/// cursor as separate pointer metadata, not composited into the frame).
struct CursorShape {
    bytes: Vec<u8>,
    shape_type: u32,
    width: u32,
    height: u32,
    pitch: u32,
}

unsafe fn run_acquire_loop(shared: &DdaShared, session: &DuplicationSession) {
    // Staging texture is reused across frames, recreated only if the size/format
    // changes — kept local to this (capture) thread.
    let mut staging: Option<ID3D11Texture2D> = None;
    let mut staging_dims: Option<(u32, u32, DXGI_FORMAT)> = None;

    // Cursor state, updated from the frame info / pointer-shape API and blended
    // into every CPU frame so the secure-desktop stream shows the mouse cursor.
    let mut cursor: Option<CursorShape> = None;
    let mut cursor_x = 0i32;
    let mut cursor_y = 0i32;
    let mut cursor_visible = false;

    while !shared.stop.load(Ordering::Acquire) {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match session.dup.AcquireNextFrame(100, &mut info, &mut resource) {
            Ok(()) => {}
            Err(e) if e.code() == HRESULT::from(DXGI_ERROR_WAIT_TIMEOUT) => continue,
            Err(e) if e.code() == HRESULT::from(DXGI_ERROR_ACCESS_LOST) => {
                shared.access_lost.store(true, Ordering::Release);
                break;
            }
            Err(e) => {
                println!("⚠️  DDA AcquireNextFrame: {e:?}");
                break;
            }
        }

        // Position/visibility update (non-zero timestamp = the pointer moved).
        if info.LastMouseUpdateTime != 0 {
            cursor_visible = info.PointerPosition.Visible.as_bool();
            cursor_x = info.PointerPosition.Position.x;
            cursor_y = info.PointerPosition.Position.y;
        }
        // New cursor shape available — fetch and cache it.
        if info.PointerShapeBufferSize > 0 {
            let mut buf = vec![0u8; info.PointerShapeBufferSize as usize];
            let mut required = 0u32;
            let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            if session
                .dup
                .GetFramePointerShape(
                    info.PointerShapeBufferSize,
                    buf.as_mut_ptr() as *mut _,
                    &mut required,
                    &mut shape_info,
                )
                .is_ok()
            {
                cursor = Some(CursorShape {
                    bytes: buf,
                    shape_type: shape_info.Type,
                    width: shape_info.Width,
                    height: shape_info.Height,
                    pitch: shape_info.Pitch,
                });
            }
        }

        if let Some(res) = resource.as_ref() {
            if let Ok(tex) = res.cast::<ID3D11Texture2D>() {
                if let Some(mut frame) =
                    copy_frame_to_cpu(&session.device, &tex, &mut staging, &mut staging_dims)
                {
                    if cursor_visible {
                        if let Some(c) = &cursor {
                            blend_cursor(&mut frame, c, cursor_x, cursor_y);
                        }
                    }
                    if let Ok(mut slot) = shared.frame.lock() {
                        *slot = Some(frame);
                    }
                }
            }
        }
        let _ = session.dup.ReleaseFrame();
    }
}

/// Alpha-blend the hardware cursor into a captured frame at `(px, py)`. Handles
/// the three DXGI pointer-shape types for both BGRA8 (SDR) and FP16 scRGB (HDR)
/// frames — the HDR path is why the cursor was previously missing with HDR10.
fn blend_cursor(frame: &mut CpuFrame, c: &CursorShape, px: i32, py: i32) {
    if frame.format == DXGI_FORMAT_R16G16B16A16_FLOAT {
        blend_cursor_fp16(frame, c, px, py);
        return;
    }
    if frame.format != DXGI_FORMAT_B8G8R8A8_UNORM {
        return;
    }
    const MONOCHROME: u32 = 1;
    const COLOR: u32 = 2;
    const MASKED_COLOR: u32 = 4;

    let (fw, fh) = (frame.width as i32, frame.height as i32);
    let rp = frame.row_pitch as usize;
    let cp = c.pitch as usize;

    match c.shape_type {
        COLOR | MASKED_COLOR => {
            let (cw, ch) = (c.width as i32, c.height as i32);
            for cy in 0..ch {
                let dy = py + cy;
                if dy < 0 || dy >= fh {
                    continue;
                }
                for cx in 0..cw {
                    let dx = px + cx;
                    if dx < 0 || dx >= fw {
                        continue;
                    }
                    let s = cy as usize * cp + cx as usize * 4;
                    let d = dy as usize * rp + dx as usize * 4;
                    if s + 4 > c.bytes.len() || d + 4 > frame.bytes.len() {
                        continue;
                    }
                    let (sb, sg, sr, sa) =
                        (c.bytes[s], c.bytes[s + 1], c.bytes[s + 2], c.bytes[s + 3]);
                    if c.shape_type == COLOR {
                        let a = sa as u32;
                        let blend = |dst: u8, src: u8| {
                            ((src as u32 * a + dst as u32 * (255 - a)) / 255) as u8
                        };
                        frame.bytes[d] = blend(frame.bytes[d], sb);
                        frame.bytes[d + 1] = blend(frame.bytes[d + 1], sg);
                        frame.bytes[d + 2] = blend(frame.bytes[d + 2], sr);
                    } else if sa == 0 {
                        // MASKED_COLOR: alpha 0 ⇒ opaque copy.
                        frame.bytes[d] = sb;
                        frame.bytes[d + 1] = sg;
                        frame.bytes[d + 2] = sr;
                    } else {
                        // alpha 0xFF ⇒ XOR with the destination.
                        frame.bytes[d] ^= sb;
                        frame.bytes[d + 1] ^= sg;
                        frame.bytes[d + 2] ^= sr;
                    }
                }
            }
        }
        MONOCHROME => {
            // 1bpp; height is 2× actual: top = AND mask, bottom = XOR mask.
            let ch = (c.height / 2) as i32;
            let cw = c.width as i32;
            for cy in 0..ch {
                let dy = py + cy;
                if dy < 0 || dy >= fh {
                    continue;
                }
                for cx in 0..cw {
                    let dx = px + cx;
                    if dx < 0 || dx >= fw {
                        continue;
                    }
                    let and_i = cy as usize * cp + cx as usize / 8;
                    let xor_i = (cy + ch) as usize * cp + cx as usize / 8;
                    if xor_i >= c.bytes.len() {
                        continue;
                    }
                    let bit = 7 - (cx % 8);
                    let and_bit = (c.bytes[and_i] >> bit) & 1;
                    let xor_bit = (c.bytes[xor_i] >> bit) & 1;
                    let d = dy as usize * rp + dx as usize * 4;
                    if d + 4 > frame.bytes.len() {
                        continue;
                    }
                    match (and_bit, xor_bit) {
                        (0, 0) => {
                            frame.bytes[d] = 0;
                            frame.bytes[d + 1] = 0;
                            frame.bytes[d + 2] = 0;
                        }
                        (0, 1) => {
                            frame.bytes[d] = 255;
                            frame.bytes[d + 1] = 255;
                            frame.bytes[d + 2] = 255;
                        }
                        (1, 0) => {} // transparent — leave the desktop pixel
                        _ => {
                            frame.bytes[d] ^= 0xFF;
                            frame.bytes[d + 1] ^= 0xFF;
                            frame.bytes[d + 2] ^= 0xFF;
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// FP16 (scRGB, HDR) variant of [`blend_cursor`]. The cursor shape is 8-bit
/// sRGB; convert it to linear and blend into the half-float channels. SDR white
/// (sRGB 255) maps to scRGB 1.0, matching how DWM composites the SDR secure
/// desktop, so the cursor sits at the same brightness as the screen behind it.
fn blend_cursor_fp16(frame: &mut CpuFrame, c: &CursorShape, px: i32, py: i32) {
    const MONOCHROME: u32 = 1;
    const COLOR: u32 = 2;
    const MASKED_COLOR: u32 = 4;

    let (fw, fh) = (frame.width as i32, frame.height as i32);
    let rp = frame.row_pitch as usize;
    let cp = c.pitch as usize;

    match c.shape_type {
        COLOR | MASKED_COLOR => {
            let (cw, ch) = (c.width as i32, c.height as i32);
            for cy in 0..ch {
                let dy = py + cy;
                if dy < 0 || dy >= fh {
                    continue;
                }
                for cx in 0..cw {
                    let dx = px + cx;
                    if dx < 0 || dx >= fw {
                        continue;
                    }
                    let s = cy as usize * cp + cx as usize * 4;
                    let d = dy as usize * rp + dx as usize * 8;
                    if s + 4 > c.bytes.len() || d + 8 > frame.bytes.len() {
                        continue;
                    }
                    let (sb, sg, sr, sa) =
                        (c.bytes[s], c.bytes[s + 1], c.bytes[s + 2], c.bytes[s + 3]);
                    let (lr, lg, lb) =
                        (srgb8_to_linear(sr), srgb8_to_linear(sg), srgb8_to_linear(sb));
                    if c.shape_type == COLOR {
                        blend_px_fp16(&mut frame.bytes, d, lr, lg, lb, sa as f32 / 255.0);
                    } else if sa == 0 {
                        blend_px_fp16(&mut frame.bytes, d, lr, lg, lb, 1.0);
                    } else {
                        invert_px_fp16(&mut frame.bytes, d);
                    }
                }
            }
        }
        MONOCHROME => {
            let ch = (c.height / 2) as i32;
            let cw = c.width as i32;
            for cy in 0..ch {
                let dy = py + cy;
                if dy < 0 || dy >= fh {
                    continue;
                }
                for cx in 0..cw {
                    let dx = px + cx;
                    if dx < 0 || dx >= fw {
                        continue;
                    }
                    let and_i = cy as usize * cp + cx as usize / 8;
                    let xor_i = (cy + ch) as usize * cp + cx as usize / 8;
                    if xor_i >= c.bytes.len() {
                        continue;
                    }
                    let bit = 7 - (cx % 8);
                    let and_bit = (c.bytes[and_i] >> bit) & 1;
                    let xor_bit = (c.bytes[xor_i] >> bit) & 1;
                    let d = dy as usize * rp + dx as usize * 8;
                    if d + 8 > frame.bytes.len() {
                        continue;
                    }
                    match (and_bit, xor_bit) {
                        (0, 0) => blend_px_fp16(&mut frame.bytes, d, 0.0, 0.0, 0.0, 1.0),
                        (0, 1) => blend_px_fp16(&mut frame.bytes, d, 1.0, 1.0, 1.0, 1.0),
                        (1, 0) => {} // transparent
                        _ => invert_px_fp16(&mut frame.bytes, d),
                    }
                }
            }
        }
        _ => {}
    }
}

/// Alpha-blend a linear RGB colour into an FP16 pixel at byte offset `d`.
fn blend_px_fp16(bytes: &mut [u8], d: usize, r: f32, g: f32, b: f32, a: f32) {
    let dr = f16_to_f32(u16::from_le_bytes([bytes[d], bytes[d + 1]]));
    let dg = f16_to_f32(u16::from_le_bytes([bytes[d + 2], bytes[d + 3]]));
    let db = f16_to_f32(u16::from_le_bytes([bytes[d + 4], bytes[d + 5]]));
    let nr = f32_to_f16(r * a + dr * (1.0 - a));
    let ng = f32_to_f16(g * a + dg * (1.0 - a));
    let nb = f32_to_f16(b * a + db * (1.0 - a));
    bytes[d..d + 2].copy_from_slice(&nr.to_le_bytes());
    bytes[d + 2..d + 4].copy_from_slice(&ng.to_le_bytes());
    bytes[d + 4..d + 6].copy_from_slice(&nb.to_le_bytes());
}

/// Invert an FP16 pixel (approximates the GDI XOR/invert cursor operation).
fn invert_px_fp16(bytes: &mut [u8], d: usize) {
    for ch in 0..3 {
        let o = d + ch * 2;
        let v = f16_to_f32(u16::from_le_bytes([bytes[o], bytes[o + 1]]));
        let iv = f32_to_f16((1.0 - v).clamp(0.0, 1.0));
        bytes[o..o + 2].copy_from_slice(&iv.to_le_bytes());
    }
}

/// sRGB 8-bit → linear float (scRGB uses a linear transfer function).
fn srgb8_to_linear(c: u8) -> f32 {
    let s = c as f32 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// Minimal IEEE half → f32 (finite values; NaN/Inf collapse to finite here).
fn f16_to_f32(h: u16) -> f32 {
    let sign = (h >> 15) as u32 & 1;
    let exp = (h >> 10) as u32 & 0x1f;
    let mant = (h & 0x3ff) as u32;
    let bits = if exp == 0 {
        if mant == 0 {
            sign << 31
        } else {
            // subnormal — normalize into a float32 exponent
            let mut e = exp as i32;
            let mut m = mant;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            let exp32 = (e + (127 - 15)) as u32;
            (sign << 31) | (exp32 << 23) | ((m & 0x3ff) << 13)
        }
    } else if exp == 0x1f {
        (sign << 31) | (0xff << 23) // treat Inf/NaN as large; clamped on write
    } else {
        ((sign << 31) | ((exp + (127 - 15)) << 23)) | (mant << 13)
    };
    f32::from_bits(bits)
}

/// Minimal f32 → IEEE half for the [0, 1]-ish range a cursor blend produces.
/// Values are clamped to the representable half range; tiny values flush to 0.
fn f32_to_f16(f: f32) -> u16 {
    let f = f.clamp(0.0, 65504.0);
    if f == 0.0 {
        return 0;
    }
    let bits = f.to_bits();
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mant = bits & 0x7f_ffff;
    if exp >= 0x1f {
        0x7bff // max finite half
    } else if exp <= 0 {
        0 // below half's normal range — negligible for a cursor
    } else {
        ((exp as u16) << 10) | ((mant >> 13) as u16)
    }
}

/// Copy a duplicated frame into a CPU buffer via a staging texture on the
/// capture thread's own device.
unsafe fn copy_frame_to_cpu(
    device: &ID3D11Device,
    tex: &ID3D11Texture2D,
    staging: &mut Option<ID3D11Texture2D>,
    staging_dims: &mut Option<(u32, u32, DXGI_FORMAT)>,
) -> Option<CpuFrame> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    tex.GetDesc(&mut desc);
    let dims = (desc.Width, desc.Height, desc.Format);

    if *staging_dims != Some(dims) {
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..desc
        };
        let mut new = None;
        device.CreateTexture2D(&staging_desc, None, Some(&mut new)).ok()?;
        *staging = new;
        *staging_dims = Some(dims);
    }
    let staging = staging.as_ref()?;

    let ctx = device.GetImmediateContext().ok()?;
    ctx.CopyResource(staging, tex);

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ok()?;

    let row_pitch = mapped.RowPitch;
    let total = row_pitch as usize * desc.Height as usize;
    let mut bytes = vec![0u8; total];
    std::ptr::copy_nonoverlapping(mapped.pData as *const u8, bytes.as_mut_ptr(), total);
    ctx.Unmap(staging, 0);

    Some(CpuFrame {
        bytes,
        width: desc.Width,
        height: desc.Height,
        row_pitch,
        format: desc.Format,
    })
}

// ── DXGI plumbing helpers ─────────────────────────────────────────────────────

fn device_name_of(desc: &DXGI_OUTPUT_DESC) -> String {
    let len = desc
        .DeviceName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.DeviceName.len());
    String::from_utf16_lossy(&desc.DeviceName[..len])
}

/// Enumerate all adapters/outputs; return the output whose GDI `DeviceName`
/// matches `target`, else the desktop-primary output (origin 0,0), else the
/// first attached output.
fn find_output(target: Option<&str>) -> Result<(IDXGIOutput, DXGI_OUTPUT_DESC), String> {
    unsafe {
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|e| format!("CreateDXGIFactory1 failed: {e:?}"))?;

        let mut primary: Option<(IDXGIOutput, DXGI_OUTPUT_DESC)> = None;
        let mut first: Option<(IDXGIOutput, DXGI_OUTPUT_DESC)> = None;

        let mut ai = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            ai += 1;
            let mut oi = 0u32;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                oi += 1;
                let desc = match output.GetDesc() {
                    Ok(d) if d.AttachedToDesktop.as_bool() => d,
                    _ => continue,
                };
                if let Some(t) = target {
                    if device_name_of(&desc).eq_ignore_ascii_case(t) {
                        return Ok((output, desc));
                    }
                }
                if desc.DesktopCoordinates.left == 0 && desc.DesktopCoordinates.top == 0 {
                    primary.get_or_insert((output.clone(), desc));
                }
                first.get_or_insert((output, desc));
            }
        }

        if let Some(t) = target {
            println!("⚠️  DDA: output \"{t}\" not found — falling back to the primary output");
        }
        primary
            .or(first)
            .ok_or_else(|| "no DXGI output is attached to the desktop".to_string())
    }
}

fn create_device_on_adapter(adapter: &IDXGIAdapter1) -> Result<ID3D11Device, String> {
    unsafe {
        let mut device = None;
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN, // must be UNKNOWN when an adapter is supplied
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .map_err(|e| format!("D3D11CreateDevice on output adapter failed: {e:?}"))?;
        device.ok_or_else(|| "D3D11CreateDevice returned no device".to_string())
    }
}

/// `IDXGIOutput5::DuplicateOutput1` (format-explicit; FP16 for HDR), falling
/// back to `IDXGIOutput1::DuplicateOutput`. `E_ACCESSDENIED` here means the
/// thread lacks secure-desktop access (no SYSTEM token / not attached).
fn duplicate_output(
    output: &IDXGIOutput,
    device: &ID3D11Device,
    format: DXGI_FORMAT,
    is_hdr: bool,
) -> Result<IDXGIOutputDuplication, String> {
    let describe = |e: &windows::core::Error| -> String {
        if e.code() == E_ACCESSDENIED {
            "E_ACCESSDENIED — the capture thread is not SYSTEM / not attached to the secure \
             desktop (needs the service's SYSTEM token)"
                .to_string()
        } else {
            format!("{e:?}")
        }
    };

    unsafe {
        if let Ok(out5) = output.cast::<IDXGIOutput5>() {
            match out5.DuplicateOutput1(device, 0, &[format]) {
                Ok(dup) => return Ok(dup),
                Err(e) => {
                    if is_hdr {
                        println!(
                            "⚠️  DDA: FP16 DuplicateOutput1 failed ({}) — retrying BGRA8",
                            describe(&e)
                        );
                    } else if e.code() == E_ACCESSDENIED {
                        return Err(format!("DuplicateOutput1 failed: {}", describe(&e)));
                    }
                }
            }
        }

        let out1: IDXGIOutput1 = output
            .cast()
            .map_err(|e| format!("IDXGIOutput1 unavailable: {e:?}"))?;
        out1.DuplicateOutput(device)
            .map_err(|e| format!("DuplicateOutput failed: {}", describe(&e)))
    }
}
