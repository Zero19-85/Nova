//! Desktop capture abstraction layer.
//!
//! ## Why an abstraction at all
//!
//! Nova captures the desktop with **Windows.Graphics.Capture (WGC)** — it sits
//! above the DXGI layer, survives HDR/Advanced-Color mode transitions, and
//! composites the cursor for us (see [`wgc`] for the full rationale). WGC is and
//! remains the default backend for every ordinary streaming session.
//!
//! WGC has exactly one blind spot: the **secure desktop**. When Windows shows a
//! UAC elevation prompt (or the Winlogon / lock screen), the input desktop
//! switches from `WinSta0\Default` to `WinSta0\Winlogon`. WGC — being a DWM
//! composition consumer bound to the interactive desktop — delivers black frames
//! for the duration. A remote administrator driving Nova then sees nothing at the
//! exact moment they need to click "Yes".
//!
//! **DXGI Desktop Duplication (DDA)** does not have this limitation: given a
//! thread that has called `SetThreadDesktop(Winlogon)` (which requires the
//! elevated/SYSTEM-derived token the launcher service provides), an
//! `IDXGIOutputDuplication` on the physical output keeps producing frames across
//! the secure-desktop switch. So the shipping architecture is **WGC primary, DDA
//! fallback**, swapped live on desktop-switch detection.
//!
//! ## The seam
//!
//! [`DesktopCapture`] is the per-frame contract both backends satisfy.
//! [`CaptureBackend`] is the concrete either/or, and [`DesktopManager`] owns the
//! currently-active backend and (from Phase 2 on) performs the swap. The encoder,
//! RTP, VDD, and input paths only ever talk to [`DesktopManager`] — none of them
//! know or care which backend is live.
//!
//! ## Phase status
//!
//! - **Phase 0:** seam defined (this trait/enum/manager), WGC-only.
//! - **Phase 1b:** [`desktop_switch::DesktopSwitchMonitor`] detects
//!   interactive↔secure desktop switches (event hook + poll fallback).
//! - **Phase 2a/b (current):** [`DdaCapturer`] is a real `IDXGIOutputDuplication`
//!   backend; [`DesktopManager::maybe_swap_backend`] moves WGC→DDA on secure
//!   desktop entry and back on exit; `lib.rs` drives the manager, not the
//!   concrete type. Until the Phase 2c SYSTEM launcher exists, DDA activation
//!   is expected to fail (`E_ACCESSDENIED`) on stock systems — the manager
//!   stays on WGC and the stream freezes on the last frame for the interlude,
//!   which is exactly the pre-Phase-2 behavior, never worse.
//! - **Phase 2c (next):** thin SYSTEM launcher service + token spawn, at which
//!   point the same DDA path starts succeeding unchanged.

mod dda;
pub mod desktop_switch;
mod wgc;

pub use dda::DdaCapturer;
pub use wgc::WgcCapturer;

use windows::core::Result;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

/// Which concrete backend is producing frames. Cheap to copy; used for logging
/// and for the desktop-switch state machine to decide whether a swap is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// Windows.Graphics.Capture — the default for all ordinary sessions.
    Wgc,
    /// DXGI Desktop Duplication — activated only while the secure desktop is up.
    Dda,
}

/// The per-frame capture contract shared by every backend.
///
/// Deliberately narrow: it exposes exactly what the [`lib.rs`](crate) capture
/// loop consumes today from `WgcCapturer`, expressed as methods so a trait object
/// or enum can stand in for the concrete type. Field access in `lib.rs`
/// (`capturer.width`, `capturer.origin_x`, …) maps onto the accessor methods
/// here when the loop migrates to [`DesktopManager`] in Phase 2.
///
/// ### Zero-copy contract
///
/// `try_get_frame` returns a texture created on **the same `ID3D11Device`** the
/// encoder uses ([`device`](DesktopCapture::device)), so the returned
/// `ID3D11Texture2D` can be handed straight to the shim's video-processor blt
/// with no system-RAM round-trip — identical to the current WGC guarantee.
pub trait DesktopCapture {
    /// Poll for the next frame. `Some(texture)` is a stable, backend-independent
    /// copy the caller may hold past the next call; `None` means "no new frame"
    /// (static desktop) or "resolution changed, rebind" — exactly the current
    /// `WgcCapturer::try_get_frame` semantics.
    fn try_get_frame(&mut self) -> Option<ID3D11Texture2D>;

    /// The last successfully captured frame, re-submitted to the encoder on a
    /// static desktop to keep the stream alive.
    fn cached_texture(&self) -> Option<&ID3D11Texture2D>;

    /// True once this session has delivered at least one frame. Resets on rebind.
    fn has_frame(&self) -> bool;

    /// Re-target capture to `gdi_device_name` (or the physical primary when
    /// `None`) and recreate the frame path for `is_hdr`. `Ok(true)` means the
    /// resolution changed and the encoder must be recreated.
    fn rebind(
        &mut self,
        gdi_device_name: Option<&str>,
        is_hdr: bool,
        expected_size: Option<(u32, u32)>,
    ) -> Result<bool>;

    /// Current captured width in physical pixels.
    fn width(&self) -> u32;
    /// Current captured height in physical pixels.
    fn height(&self) -> u32;
    /// Desktop-coordinate origin `(x, y)` of the captured surface — used by
    /// `input.rs` to map client-relative mouse coordinates.
    fn origin(&self) -> (i32, i32);
    /// The shared D3D11 device backing both this capturer and the NVENC encoder.
    fn device(&self) -> &ID3D11Device;
    /// Which backend this is — for logging and the switch state machine.
    fn kind(&self) -> BackendKind;
}

impl DesktopCapture for WgcCapturer {
    fn try_get_frame(&mut self) -> Option<ID3D11Texture2D> {
        WgcCapturer::try_get_frame(self)
    }
    fn cached_texture(&self) -> Option<&ID3D11Texture2D> {
        WgcCapturer::cached_texture(self)
    }
    fn has_frame(&self) -> bool {
        WgcCapturer::has_frame(self)
    }
    fn rebind(
        &mut self,
        gdi_device_name: Option<&str>,
        is_hdr: bool,
        expected_size: Option<(u32, u32)>,
    ) -> Result<bool> {
        WgcCapturer::rebind(self, gdi_device_name, is_hdr, expected_size)
    }
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn origin(&self) -> (i32, i32) {
        (self.origin_x, self.origin_y)
    }
    fn device(&self) -> &ID3D11Device {
        &self.device
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Wgc
    }
}

/// The concrete either/or of the two backends.
///
/// An enum rather than `Box<dyn DesktopCapture>` so the per-frame hot path stays
/// a static, inlinable dispatch (zero-cost) — the trait exists for the contract,
/// not for runtime polymorphism in the frame loop.
pub enum CaptureBackend {
    Wgc(WgcCapturer),
    Dda(DdaCapturer),
}

impl DesktopCapture for CaptureBackend {
    fn try_get_frame(&mut self) -> Option<ID3D11Texture2D> {
        match self {
            CaptureBackend::Wgc(c) => c.try_get_frame(),
            CaptureBackend::Dda(c) => c.try_get_frame(),
        }
    }
    fn cached_texture(&self) -> Option<&ID3D11Texture2D> {
        match self {
            CaptureBackend::Wgc(c) => c.cached_texture(),
            CaptureBackend::Dda(c) => c.cached_texture(),
        }
    }
    fn has_frame(&self) -> bool {
        match self {
            CaptureBackend::Wgc(c) => c.has_frame(),
            CaptureBackend::Dda(c) => c.has_frame(),
        }
    }
    fn rebind(
        &mut self,
        gdi_device_name: Option<&str>,
        is_hdr: bool,
        expected_size: Option<(u32, u32)>,
    ) -> Result<bool> {
        match self {
            CaptureBackend::Wgc(c) => c.rebind(gdi_device_name, is_hdr, expected_size),
            CaptureBackend::Dda(c) => c.rebind(gdi_device_name, is_hdr, expected_size),
        }
    }
    fn width(&self) -> u32 {
        match self {
            CaptureBackend::Wgc(c) => c.width(),
            CaptureBackend::Dda(c) => c.width(),
        }
    }
    fn height(&self) -> u32 {
        match self {
            CaptureBackend::Wgc(c) => c.height(),
            CaptureBackend::Dda(c) => c.height(),
        }
    }
    fn origin(&self) -> (i32, i32) {
        match self {
            CaptureBackend::Wgc(c) => c.origin(),
            CaptureBackend::Dda(c) => c.origin(),
        }
    }
    fn device(&self) -> &ID3D11Device {
        match self {
            CaptureBackend::Wgc(c) => c.device(),
            CaptureBackend::Dda(c) => c.device(),
        }
    }
    fn kind(&self) -> BackendKind {
        match self {
            CaptureBackend::Wgc(_) => BackendKind::Wgc,
            CaptureBackend::Dda(_) => BackendKind::Dda,
        }
    }
}

/// Owns the currently-active capture backend, mediates all frame acquisition,
/// and performs the WGC↔DDA swap on secure-desktop transitions (Phase 15.2).
///
/// The encoder, RTP, VDD, and input paths only ever talk to this object.
/// Invariants:
///
/// - **One D3D11 device for the process lifetime** (`self.device`, shared with
///   NVENC). Every backend the manager creates — including WGC sessions rebuilt
///   after a DDA interlude — binds to it, so the shim never sees a
///   foreign-device texture.
/// - **Sessions run on WGC.** DDA exists only while the input desktop is
///   `Secure`; [`Self::maybe_swap_backend`] (called once per capture-loop
///   iteration — two atomic loads in steady state) moves to DDA on
///   Default→Secure and back on Secure→Default. A session-level
///   [`DesktopCapture::rebind`] while the desktop is normal always lands on WGC.
/// - **Failure is survivable.** DDA activation is *expected* to fail with
///   `E_ACCESSDENIED` until the Phase 2c SYSTEM launcher exists; the manager
///   logs it, stays on WGC (client sees the last frame frozen), and retries on
///   a cooldown. Nothing panics, nothing tears the stream down.
pub struct DesktopManager {
    backend: CaptureBackend,
    /// The NVENC-shared device — cloned into every backend this manager builds.
    device: ID3D11Device,
    /// Session target (GDI device name) recorded from the last `rebind`, so a
    /// swap can restore the correct binding on the way back.
    target: Option<String>,
    /// VDD device name to exclude when targeting the physical primary.
    exclude: Option<String>,
    /// HDR state recorded from the last `rebind` (frame-pool / dup format).
    is_hdr: bool,
    /// Backoff after a failed swap so a persistently-denied DDA init doesn't
    /// spam an attempt per frame.
    swap_retry_after: Option<std::time::Instant>,
}

impl DesktopManager {
    /// Cooldown between swap attempts after a failure.
    const SWAP_RETRY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(5);

    /// Start on WGC excluding the virtual-display device named `exclude` — the
    /// same entry point `lib.rs` used with the bare `WgcCapturer`.
    ///
    /// **Pre-login fallback:** WGC's WinRT/broker infrastructure requires a
    /// real interactive user session. When the service-launched host starts at
    /// the logon screen (no user logged in yet), `WgcCapturer::new` fails with
    /// `0x80070424` (ERROR_SERVICE_DOES_NOT_EXIST) and previously took the
    /// whole host down — the service respawned it every 2 s and each restart's
    /// VDD devnode cycle played the device connect/disconnect chime in an
    /// endless loop. Instead we fall back to starting on DDA, which — via the
    /// service-provided SYSTEM impersonation token — is exactly the backend
    /// that CAN capture the Winlogon/logon desktop, so Moonlight can connect
    /// at the lock screen and the user can type their PIN remotely (the whole
    /// point of true headless). After login, `maybe_swap_backend` /
    /// session `rebind` route the process back onto WGC as usual.
    pub fn new_wgc(exclude: Option<&str>) -> Result<Self> {
        let wgc_err = match WgcCapturer::new_excluding(exclude) {
            Ok(wgc) => {
                let device = wgc.device.clone();
                return Ok(Self {
                    backend: CaptureBackend::Wgc(wgc),
                    device,
                    target: None,
                    exclude: exclude.map(str::to_owned),
                    is_hdr: false,
                    swap_retry_after: None,
                });
            }
            Err(e) => e,
        };

        println!(
            "⚠️  WGC unavailable at startup ({wgc_err:?}) — likely pre-login \
             (logon/lock screen). Falling back to DDA so the login UI is \
             streamable; WGC takes over after login."
        );
        let device = WgcCapturer::create_d3d11_device()?;
        match DdaCapturer::new(device.clone(), None, false) {
            Ok(dda) => Ok(Self {
                backend: CaptureBackend::Dda(dda),
                device,
                target: None,
                exclude: exclude.map(str::to_owned),
                is_hdr: false,
                swap_retry_after: None,
            }),
            Err(dda_err) => {
                println!(
                    "❌ DDA startup fallback also failed: {dda_err:?} (no \
                     --system-token from the service, or no output attached)"
                );
                // Surface the original WGC error — it names the primary failure.
                Err(wgc_err)
            }
        }
    }

    /// Which backend is currently live — lets `lib.rs` run the idle
    /// DDA→WGC heal without exposing the backend itself.
    pub fn backend_kind(&self) -> BackendKind {
        self.backend.kind()
    }

    /// Drive the backend to match the current input desktop. Called once per
    /// capture-loop iteration; steady state is two atomic loads and a compare.
    ///
    /// Returns `Some(resized)` when a swap (or an in-place DDA restore)
    /// happened — `resized: true` means the capture dimensions changed and the
    /// caller must recreate the encoder. `None` = nothing changed.
    pub fn maybe_swap_backend(&mut self) -> Option<bool> {
        use desktop_switch::InputDesktop;

        let desk = desktop_switch::current_input_desktop();
        match (self.backend.kind(), desk) {
            (BackendKind::Wgc, InputDesktop::Secure) => self.swap_to_dda(),
            (BackendKind::Dda, InputDesktop::Default) => self.swap_to_wgc(),
            (BackendKind::Dda, _) => {
                // Still on the secure desktop (or state unknown): if the
                // duplication died (mode change mid-prompt), rebuild it.
                let lost = matches!(&self.backend, CaptureBackend::Dda(d) if d.access_lost());
                if lost {
                    self.restore_dda()
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn cooldown_active(&self) -> bool {
        self.swap_retry_after
            .is_some_and(|t| std::time::Instant::now() < t)
    }

    fn swap_to_dda(&mut self) -> Option<bool> {
        if self.cooldown_active() {
            return None;
        }
        let (old_w, old_h) = (self.backend.width(), self.backend.height());
        match DdaCapturer::new(self.device.clone(), self.target.as_deref(), self.is_hdr) {
            Ok(dda) => {
                let resized = dda.width() != old_w || dda.height() != old_h;
                println!(
                    "🔀 Capture backend: WGC → DDA (secure desktop active){}",
                    if resized { " — capture size changed" } else { "" }
                );
                // Old WGC session/pool drop here → WGC capture stops cleanly.
                self.backend = CaptureBackend::Dda(dda);
                self.swap_retry_after = None;
                Some(resized)
            }
            Err(e) => {
                println!(
                    "⚠️  DDA activation failed: {e}\n\
                     ⚠️  Staying on WGC — the client sees the last frame until the \
                     secure desktop closes. Retrying in {:?}.",
                    Self::SWAP_RETRY_COOLDOWN
                );
                self.swap_retry_after =
                    Some(std::time::Instant::now() + Self::SWAP_RETRY_COOLDOWN);
                None
            }
        }
    }

    fn swap_to_wgc(&mut self) -> Option<bool> {
        if self.cooldown_active() {
            return None;
        }
        let (old_w, old_h) = (self.backend.width(), self.backend.height());
        // Stop DDA's dedicated capture thread (which holds the SYSTEM
        // impersonation + secure-desktop attachment) before building WGC. The
        // main thread's identity/desktop are never touched, so WGC is otherwise
        // unaffected; this just tears the interlude down promptly.
        if let CaptureBackend::Dda(d) = &mut self.backend {
            d.release();
        }
        match WgcCapturer::new_on_device(
            self.device.clone(),
            self.target.as_deref(),
            self.exclude.as_deref(),
            self.is_hdr,
        ) {
            Ok(wgc) => {
                let resized = wgc.width != old_w || wgc.height != old_h;
                println!(
                    "🔀 Capture backend: DDA → WGC (interactive desktop restored){}",
                    if resized { " — capture size changed" } else { "" }
                );
                self.backend = CaptureBackend::Wgc(wgc);
                self.swap_retry_after = None;
                Some(resized)
            }
            Err(e) => {
                // The desktop just transitioned — WGC session creation can race
                // the compositor settling. Short cooldown, keep trying.
                println!("⚠️  WGC restore after secure desktop failed ({e:?}) — retrying");
                self.swap_retry_after =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
                None
            }
        }
    }

    /// Rebuild a duplication that reported ACCESS_LOST while the secure
    /// desktop is still up (e.g. a display-mode change under the prompt).
    fn restore_dda(&mut self) -> Option<bool> {
        if self.cooldown_active() {
            return None;
        }
        let (old_w, old_h) = (self.backend.width(), self.backend.height());
        if let CaptureBackend::Dda(dda) = &mut self.backend {
            match dda.try_restore() {
                Ok(()) => {
                    let resized = dda.width() != old_w || dda.height() != old_h;
                    println!("🔁 DDA duplication restored after ACCESS_LOST");
                    self.swap_retry_after = None;
                    return Some(resized);
                }
                Err(e) => {
                    println!("⚠️  DDA restore failed: {e} — retrying on cooldown");
                    self.swap_retry_after =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
                }
            }
        }
        None
    }
}

// `DesktopManager` forwards the full capture contract, so callers can treat it
// as a `DesktopCapture` directly without reaching into `.backend()`.
impl DesktopCapture for DesktopManager {
    fn try_get_frame(&mut self) -> Option<ID3D11Texture2D> {
        self.backend.try_get_frame()
    }
    fn cached_texture(&self) -> Option<&ID3D11Texture2D> {
        self.backend.cached_texture()
    }
    fn has_frame(&self) -> bool {
        self.backend.has_frame()
    }
    /// Session-level retarget. Records the target/HDR state (so later swaps
    /// restore the right binding), then applies it to the backend appropriate
    /// for the CURRENT desktop: on the interactive desktop sessions always
    /// (re)bind on WGC — including when a stale DDA backend is still in place
    /// from a secure-desktop interlude; while the secure desktop is up, the
    /// rebind retargets the live duplication instead.
    fn rebind(
        &mut self,
        gdi_device_name: Option<&str>,
        is_hdr: bool,
        expected_size: Option<(u32, u32)>,
    ) -> Result<bool> {
        self.target = gdi_device_name.map(str::to_owned);
        self.is_hdr = is_hdr;

        let on_secure = desktop_switch::current_input_desktop()
            == desktop_switch::InputDesktop::Secure;
        if self.backend.kind() == BackendKind::Dda && !on_secure {
            // Session event arrived while a DDA interlude was still latched —
            // rebind implies WGC. `expected_size` is skipped on this path (the
            // resolution guard in lib.rs re-snaps if the mode is still settling).
            let (old_w, old_h) = (self.backend.width(), self.backend.height());
            // Release DDA's impersonation + secure-desktop attachment first, or
            // WGC creation on this thread fails (see swap_to_wgc).
            if let CaptureBackend::Dda(d) = &mut self.backend {
                d.release();
            }
            let wgc = WgcCapturer::new_on_device(
                self.device.clone(),
                gdi_device_name,
                self.exclude.as_deref(),
                is_hdr,
            )?;
            let resized = wgc.width != old_w || wgc.height != old_h;
            println!("🔀 Capture backend: DDA → WGC (session rebind)");
            self.backend = CaptureBackend::Wgc(wgc);
            self.swap_retry_after = None;
            return Ok(resized);
        }
        self.backend.rebind(gdi_device_name, is_hdr, expected_size)
    }
    fn width(&self) -> u32 {
        self.backend.width()
    }
    fn height(&self) -> u32 {
        self.backend.height()
    }
    fn origin(&self) -> (i32, i32) {
        self.backend.origin()
    }
    fn device(&self) -> &ID3D11Device {
        self.backend.device()
    }
    fn kind(&self) -> BackendKind {
        self.backend.kind()
    }
}
