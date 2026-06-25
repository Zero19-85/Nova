//! Lifecycle orchestration for the "Virtual Desktop" (App 5) feature.
//!
//! Nova does **not** fork or build a display driver. We drive the
//! community-maintained, Microsoft-signed Virtual Display Driver (VDD)
//! project as an external dependency, exactly the way Sunshine treats
//! optional helper drivers (ViGEmBus, Steam Streaming Speakers):
//!
//!   - Upstream:        <https://github.com/VirtualDrivers/Virtual-Display-Driver>
//!   - Local reference: C:\VDD.Control.25.7.23 (pre-built package for this dev box)
//!       - SignedDrivers\x86\VDD\MttVDD.inf  — UMDF/IddCx driver, installs as
//!         a ROOT-enumerated software device, hardware id `Root\MttVDD`
//!         (see [Standard.NTamd64] section of the .inf).
//!       - SignedDrivers\x86\VDD\MttVDD.dll  — the IddCx driver binary
//!       - Dependencies\devcon.exe           — Microsoft's device console
//!         tool; the supported way to create/enable/disable a ROOT-enumerated
//!         devnode from an .inf (SetupAPI's `DiInstallDevice` equivalent).
//!       - Dependencies\vdd_settings.xml     — declares the resolution /
//!         refresh-rate table the driver advertises to Windows.
//!
//! ## Big picture
//!
//! Module owns one [`VirtualDisplay`] instance, created once at startup and
//! held for the lifetime of the process. `Root\MttVDD` is enabled once, at
//! boot, via [`VirtualDisplay::ensure_enabled_at_boot`], and stays enabled
//! for the whole process lifetime. When the Moonlight client launches App 5:
//!
//!   1. [`VirtualDisplay::configure_mode`] — make the already-enabled driver
//!      advertise a mode matching the client's negotiated resolution/fps.
//!   2. [`VirtualDisplay::activate_for_stream`] — make the virtual monitor
//!      the new desktop primary, and fully detach the physical display
//!      path(s) from the CCD topology (true headless).
//!   3. main.rs's capture loop is re-pointed at the new primary (see "DXGI
//!      Handoff Strategy" below — this is the part that touches capture.rs /
//!      shim.cpp and is intentionally NOT scaffolded here yet).
//!
//! On stream end, [`VirtualDisplay::deactivate_after_stream`] reverses all of
//! the above, restoring the host to exactly the state it was in before the
//! client connected.
//!
//! ## DXGI Handoff Strategy (Task 2 — design notes, not yet implemented)
//!
//! Today, `capture.rs::DesktopCapturer::new()` always duplicates
//! `adapter.EnumOutputs(0)` — i.e. "whatever Windows currently calls output
//! 0", which in practice is the physical primary. Swapping the primary to the
//! virtual display invalidates that duplication handle (DXGI_ERROR_ACCESS_LOST
//! at minimum, possibly a different adapter enumeration order entirely since
//! IddCx adapters are their own LUID).
//!
//! Planned sequence for `App 5` start:
//!
//!   1. `activate_for_stream()` runs steps 1-2 above. At this point Windows
//!      has a NEW primary display whose GDI device name (e.g. `\\.\DISPLAY3`)
//!      we capture in `VirtualDisplay::active_device_name()`.
//!   2. main.rs's capture loop must be told to tear down its current
//!      `DesktopCapturer` + `Encoder` and rebuild both against the new
//!      output. Concretely (future work, NOT in this file):
//!        - `capture.rs` gains `DesktopCapturer::for_output_matching(name: &str)`
//!          — same as `new()` but iterates `adapter.EnumOutputs(i)` for i in
//!          0.. until `IDXGIOutput::GetDesc().DeviceName` matches `name`,
//!          across ALL adapters returned by `IDXGIFactory1::EnumAdapters`
//!          (the IddCx adapter is a distinct adapter, not just a new output
//!          on adapter 0).
//!        - `shim.cpp` needs a `ReinitEncoder(width, height)` export
//!          (sibling to the existing `ReconfigureBitrate`, see
//!          encoder.rs:67) since the virtual display's resolution generally
//!          differs from the physical one — VP rects, NV12 staging texture,
//!          and NVENC session all need to be rebuilt at the new size.
//!        - main.rs's main loop needs a "capture target changed" flag (set by
//!          `activate_for_stream`/`deactivate_after_stream`) that causes it to
//!          drop + recreate `DesktopCapturer` and call `ReinitEncoder` before
//!          the next `AcquireNextFrame`.
//!   3. [`VirtualDisplay::deactivate_other_paths`] clears
//!      `DISPLAYCONFIG_PATH_ACTIVE` on every CCD path except the virtual
//!      display's, via `SetDisplayConfig(SDC_USE_SUPPLIED_DISPLAY_CONFIG)`.
//!      This is the "true headless" mechanism and replaces the earlier
//!      `SC_MONITORPOWER`/`sleep_displays()` approach: monitor standby risked
//!      breaking DXGI duplication of a physical output that then lost power,
//!      whereas detaching the path entirely is safe since capture has
//!      already moved to the virtual output by this point.
//!
//! Reverse sequence for stream stop is `deactivate_after_stream()` —
//! [`VirtualDisplay::restore_topology`] re-applies the topology snapshot
//! taken before activation (forcing the virtual display's path inactive) —
//! followed by the mirror-image capture/encoder rebind back to the physical
//! output.
//!
//! ## Cargo.toml audit note
//!
//! The GDI calls used below (`EnumDisplayDevicesW`, `EnumDisplaySettingsW`,
//! `ChangeDisplaySettingsExW` — the latter now only in the `#[ignore]`d
//! diagnostic tests, see "Primary-display switching" below) are covered by
//! `Win32_Graphics_Gdi`. The SetupAPI calls used for driver detection/enable/
//! disable need `Win32_Devices_DeviceAndDriverInstallation` +
//! `Win32_Devices_Properties` (for `CM_Get_DevNode_Status`). The CCD topology
//! calls (`QueryDisplayConfig`/`SetDisplayConfig`/`DisplayConfigGetDeviceInfo`)
//! need `Win32_Devices_Display`. The audio-endpoint cache/restore in
//! [`VirtualDisplay::activate_for_stream`]/[`VirtualDisplay::deactivate_after_stream`]
//! needs `Win32_Media_Audio` (`IMMDeviceEnumerator`/`IMMDevice`) on top of the
//! existing `Win32_System_Com`. All of the above are present in `Cargo.toml`.
//! No new crates required — flagging per repo rule #1.
//!
//! ## Primary-display switching: CCD, not legacy GDI
//!
//! [`VirtualDisplay::set_primary_display`] uses the modern Connecting and
//! Configuring Displays (CCD) topology API
//! (`QueryDisplayConfig`/`SetDisplayConfig`), repositioning CCD source modes
//! so the target display's source sits at desktop origin `(0, 0)` — which is
//! what GDI treats as "primary". The legacy
//! `ChangeDisplaySettingsExW`/`CDS_SET_PRIMARY` path was tried first and
//! returns `DISP_CHANGE_FAILED` outright on this driver stack, even as a true
//! no-op on the already-primary display (kept as `#[ignore]`d diagnostics:
//! `enum_modes_diagnostic`, `set_primary_noop_diagnostic`).

use std::path::{Path, PathBuf};
use std::process::Command;

use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, HWND, LUID, POINTL};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplaySettingsW, CDS_GLOBAL,
    CDS_RESET, CDS_UPDATEREGISTRY, DEVMODEW, DISPLAY_DEVICEW, DISP_CHANGE_SUCCESSFUL,
    DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS,
};
#[cfg(test)]
use windows::Win32::Graphics::Gdi::{CDS_NORESET, CDS_TYPE};
#[cfg(test)]
use windows::Win32::Graphics::Gdi::DISPLAY_DEVICE_PRIMARY_DEVICE;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo,
    GetDisplayConfigBufferSizes, QueryDisplayConfig, SetDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_DEVICE_INFO_TYPE,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
    DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TOPOLOGY_ID, QDC_ALL_PATHS,
    QDC_DATABASE_CURRENT, QDC_ONLY_ACTIVE_PATHS, QUERY_DISPLAY_CONFIG_FLAGS, SDC_ALLOW_CHANGES,
    SDC_APPLY, SDC_SAVE_TO_DATABASE, SDC_TOPOLOGY_EXTEND, SDC_USE_SUPPLIED_DISPLAY_CONFIG,
};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_Status, CM_DEVNODE_STATUS_FLAGS, CM_PROB, CM_PROB_DISABLED, CR_SUCCESS,
    SetupDiCallClassInstaller, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
    SetupDiGetClassDevsW, SetupDiGetDeviceRegistryPropertyW, SetupDiSetClassInstallParamsW,
    SetupDiSetDeviceRegistryPropertyW,
    DICS_DISABLE, DICS_ENABLE, DICS_FLAG_GLOBAL, DIF_PROPERTYCHANGE, DIGCF_PRESENT,
    GUID_DEVCLASS_DISPLAY, HDEVINFO, SP_CLASSINSTALL_HEADER, SP_DEVINFO_DATA, SP_PROPCHANGE_PARAMS,
    SPDRP_FRIENDLYNAME, SPDRP_HARDWAREID,
};
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    REG_VALUE_TYPE,
};

// `SetDefaultAudioDevice` from the C++ audio shim (`shim/audio_shim.cpp`) —
// sets `device_id` as the default render endpoint for all three roles
// (console/multimedia/communications) via the undocumented `IPolicyConfig`
// COM interface. Re-declared here (same symbol `audio.rs` binds) so
// `VirtualDisplay::deactivate_after_stream` can force-restore the audio
// endpoint it cached in `VirtualDisplay::activate_for_stream`, independent
// of `audio.rs`'s own `SinkGuard` restore path.
extern "C" {
    fn SetDefaultAudioDevice(device_id: *const u16) -> i32;
}

/// Upstream project (for `ensure_installed`'s download step). Kept as a
/// constant so the release asset can be bumped without touching logic.
///
/// Pinned to the 25.7.23 release's portable "VDD Control" zip — the same
/// package as the local reference copy (C:\VDD.Control.25.7.23). The
/// `/releases/latest/download/Virtual.Display.Driver.zip` URL previously
/// here 404s; that filename isn't attached to any release.
const VDD_RELEASE_ZIP_URL: &str =
    "https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/download/25.7.23/VDD.Control.25.7.23.zip";

/// Root-enumerated hardware ID the driver registers under — from
/// `MttVDD.inf`'s `[Standard.NTamd64]` section:
/// `%DeviceName% = MyDevice_Install, Root\MttVDD`.
const VDD_HARDWARE_ID: &str = "Root\\MttVDD";

/// INF file name inside the signed driver package
/// (`SignedDrivers\x86\VDD\MttVDD.inf`).
const VDD_INF_NAME: &str = "MttVDD.inf";

/// `HKLM\SOFTWARE\MikeTheTech\VirtualDisplayDriver` — registry key MttVDD.dll
/// consults at startup for the [`VDD_REGISTRY_VALUE`] (`VDDPATH`) string
/// value, the directory containing its `vdd_settings.xml` / `option.txt` /
/// `adapter.txt`. Confirmed via strings extracted from the installed
/// `MttVDD.dll` ("Failed to open registry key for path" /
/// "SOFTWARE\MikeTheTech\VirtualDisplayDriver" / "VDDPATH"); falls back to
/// `C:\VirtualDisplayDriver\vdd_settings.xml` (or `C:\IddSampleDriver\...`)
/// if the key/value is absent — neither of which exists on this dev box, so
/// [`VirtualDisplay::configure_mode`] points it at Nova's own copy under
/// `install_dir\Dependencies`.
const VDD_REGISTRY_KEY: &str = r"SOFTWARE\MikeTheTech\VirtualDisplayDriver";

/// REG_SZ value name under [`VDD_REGISTRY_KEY`] holding the settings
/// directory path.
const VDD_REGISTRY_VALUE: &str = "VDDPATH";

/// `DISPLAYCONFIG_PATH_INFO::flags` bit marking a path as part of the active
/// desktop topology. Documented in `wingdi.h` but not exported by
/// `windows-rs` 0.58's `Win32_Devices_Display` — defined locally at its
/// standard value. Used by [`VirtualDisplay::deactivate_other_paths`] and
/// [`VirtualDisplay::restore_topology`] to flip paths active/inactive.
const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x0000_0001;

/// Snapshot of the physical display that was primary before Nova switched to
/// the virtual one. Used only by the `#[ignore]`d CCD/GDI diagnostics below —
/// [`VirtualDisplay::activate_for_stream`]/[`VirtualDisplay::deactivate_after_stream`]
/// now save/restore the full CCD topology instead (see `saved_topology`).
#[cfg(test)]
struct DisplaySnapshot {
    /// GDI device name, e.g. `\\.\DISPLAY1` (from `EnumDisplayDevicesW`).
    device_name: String,
    width: u32,
    height: u32,
    refresh_hz: u32,
    /// Desktop-coordinate origin (`DEVMODEW.dmPosition`) — needed because
    /// exactly one display must sit at (0,0) and switching primary means
    /// re-positioning both displays.
    position: (i32, i32),
}

/// Owns the lifecycle of the virtual display device for the duration of the
/// Nova process. One instance, created at startup, reused across
/// connect/disconnect cycles.
pub struct VirtualDisplay {
    /// Where Nova caches the downloaded driver package + `devcon.exe`, e.g.
    /// `<exe_dir>\VirtualDisplayDriver\`. Mirrors
    /// `app_launcher::retroarch_dir()`'s pattern.
    install_dir: PathBuf,

    /// Set while Nova currently has the virtual monitor enabled/primary.
    /// Lets `deactivate_after_stream` no-op safely if called twice, and lets
    /// a future "is a virtual-desktop session active?" check exist without
    /// re-querying the device tree.
    active: bool,

    /// CCD topology as persisted in the database (`QDC_DATABASE_CURRENT`),
    /// captured by `activate_for_stream` before any mutation. `None` when
    /// inactive.
    ///
    /// NOT re-applied to `SetDisplayConfig` directly — every variant tried
    /// (`QDC_ALL_PATHS`, `QDC_ONLY_ACTIVE_PATHS`, `QDC_DATABASE_CURRENT`) hits
    /// `ERROR_INVALID_PARAMETER`/87 as a direct
    /// `SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_APPLY` payload. Instead
    /// [`Self::restore_topology`] takes a FRESH [`Self::query_all_topology`]
    /// snapshot as its apply payload and consults this saved snapshot only as
    /// a "what was active, where, and with what mode" reference for
    /// reactivating the physical path(s) — see its doc comment.
    saved_topology: Option<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>)>,

    /// GDI device name of the virtual monitor once it's enabled and Windows
    /// has assigned it a `\\.\DISPLAYn` slot. Filled in by
    /// `activate_for_stream`, used by capture re-hook (see module docs) and
    /// by `deactivate_after_stream`.
    active_device_name: Option<String>,

    /// `(width, height)` requested by [`activate_for_stream`], cleared by
    /// [`deactivate_after_stream`]. Lets the steady-state DXGI rebind (in
    /// lib.rs's `run()`) pass an `expected_size` to
    /// [`crate::capture::DesktopCapturer::rebind`] while a stream is active,
    /// so a transient post-CCD-apply 800x600 dip (see
    /// [`wait_for_display_resolution`]) gets retried instead of latched onto.
    active_resolution: Option<(u32, u32)>,

    /// Default audio render endpoint (device id string, NUL-terminated
    /// UTF-16), cached by `activate_for_stream` via `IMMDeviceEnumerator`
    /// *before* anything (display topology or audio sink) is mutated.
    /// Consumed by `deactivate_after_stream` to force the system back to the
    /// real speakers via `SetDefaultAudioDevice`, bypassing whatever Windows
    /// guesses once the virtual display's own audio endpoint has appeared.
    saved_audio_endpoint: Option<Vec<u16>>,
}

impl VirtualDisplay {
    pub fn new() -> Self {
        let install_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("VirtualDisplayDriver")))
            .unwrap_or_else(|| PathBuf::from("VirtualDisplayDriver"));

        Self {
            install_dir,
            active: false,
            saved_topology: None,
            active_device_name: None,
            active_resolution: None,
            saved_audio_endpoint: None,
        }
    }

    // -------------------------------------------------------------
    // Detection
    // -------------------------------------------------------------

    /// Is the MttVDD driver/devnode present at all (enabled OR disabled)?
    ///
    /// Enumerates `GUID_DEVCLASS_DISPLAY` via `SetupDiGetClassDevsW` +
    /// `SetupDiEnumDeviceInfo`, calling `SetupDiGetDeviceInstanceIdW` on each
    /// result and looking for an instance id starting with `ROOT\MTTVDD`
    /// (case-insensitive — matches [`VDD_HARDWARE_ID`]).
    pub fn is_installed(&self) -> bool {
        Self::find_devnode().is_some()
    }

    /// Is the devnode currently enabled (i.e. presenting a monitor to
    /// Windows right now)?
    ///
    /// Resolves the `DEVINST` via [`find_devnode`] then calls
    /// `CM_Get_DevNode_Status`. Treated as "enabled" unless the devnode's
    /// problem code is specifically `CM_PROB_DISABLED` — any other problem
    /// (or none) means the devnode is active in the tree, just possibly
    /// malfunctioning, which is still "enabled" from our orchestration
    /// standpoint (a disable/enable cycle would still be meaningful).
    pub fn is_enabled(&self) -> bool {
        let Some(devinst) = Self::find_devnode() else {
            return false;
        };

        let mut status = CM_DEVNODE_STATUS_FLAGS(0);
        let mut problem = CM_PROB(0);
        let cr = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) };

        cr == CR_SUCCESS && problem != CM_PROB_DISABLED
    }

    /// Enumerates display-class devnodes and returns the `DEVINST` of the
    /// first one whose device instance id starts with `ROOT\MTTVDD`
    /// (case-insensitive), or `None` if the driver isn't installed.
    fn find_devnode() -> Option<u32> {
        let (hdevinfo, devinfo_data) = Self::open_devnode().ok()??;
        let devinst = devinfo_data.DevInst;
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(hdevinfo);
        }
        Some(devinst)
    }

    /// Opens a fresh `SetupDiGetClassDevsW(GUID_DEVCLASS_DISPLAY,
    /// DIGCF_PRESENT)` device-info set and returns it together with the
    /// `SP_DEVINFO_DATA` of the entry whose `SPDRP_HARDWAREID` contains
    /// [`VDD_HARDWARE_ID`] (case-insensitive), or `None` if no such entry
    /// exists.
    ///
    /// The caller takes ownership of the returned `HDEVINFO` and MUST pass it
    /// to `SetupDiDestroyDeviceInfoList` once done — used both by
    /// [`find_devnode`] (which destroys it immediately) and
    /// [`set_enabled_native`] (which needs the live handle + `SP_DEVINFO_DATA`
    /// together to call `SetupDiSetClassInstallParamsW`/
    /// `SetupDiCallClassInstaller`).
    fn open_devnode() -> windows::core::Result<Option<(HDEVINFO, SP_DEVINFO_DATA)>> {
        unsafe {
            let hdevinfo = SetupDiGetClassDevsW(
                Some(&GUID_DEVCLASS_DISPLAY),
                PCWSTR::null(),
                HWND(std::ptr::null_mut()),
                DIGCF_PRESENT,
            )?;

            let mut index = 0u32;
            loop {
                let mut devinfo_data = SP_DEVINFO_DATA {
                    cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                    ..Default::default()
                };
                if SetupDiEnumDeviceInfo(hdevinfo, index, &mut devinfo_data).is_err() {
                    let _ = SetupDiDestroyDeviceInfoList(hdevinfo);
                    return Ok(None);
                }

                // Root-enumerated devices get an instance id like
                // `ROOT\DISPLAY\0001` — the `Root\MttVDD` hardware id only
                // shows up in the SPDRP_HARDWAREID property (a REG_MULTI_SZ:
                // multiple null-terminated strings, double-null terminated),
                // not the instance id.
                let mut buf = [0u8; 512];
                if SetupDiGetDeviceRegistryPropertyW(hdevinfo, &devinfo_data, SPDRP_HARDWAREID, None, Some(&mut buf), None).is_ok()
                    && Self::multi_sz_contains(&buf, VDD_HARDWARE_ID)
                {
                    return Ok(Some((hdevinfo, devinfo_data)));
                }

                index += 1;
            }
        }
    }

    /// Interprets `buf` as a `REG_MULTI_SZ` (UTF-16LE, sequence of
    /// null-terminated strings, terminated by an additional empty string /
    /// double null) and checks whether any entry case-insensitively equals
    /// `target`.
    fn multi_sz_contains(buf: &[u8], target: &str) -> bool {
        let u16s: Vec<u16> = buf.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        u16s.split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .any(|s| String::from_utf16_lossy(s).eq_ignore_ascii_case(target))
    }

    // -------------------------------------------------------------
    // Install
    // -------------------------------------------------------------

    /// Ensures the driver is installed, downloading + installing it if not.
    ///
    /// Steps:
    ///   1. `is_installed()` — early return if already present.
    ///   2. [`download_release_package`] if `install_dir` doesn't already
    ///      contain `MttVDD.inf` + `devcon.exe` (cache across runs — don't
    ///      re-download every launch).
    ///   3. [`run_elevated_devcon`]`(&["install", "<path to MttVDD.inf>",
    ///      VDD_HARDWARE_ID])` — this is the one step that needs admin, and
    ///      pops exactly one UAC prompt.
    ///   4. Re-check `is_installed()` and surface a clear error if the
    ///      elevated step was cancelled/failed (e.g. user clicked "No" on
    ///      UAC) — the caller (App 5 launch handler) should report this back
    ///      rather than silently falling back to the physical desktop.
    pub fn ensure_installed(&mut self) -> Result<(), String> {
        let devnode_installed = self.is_installed();
        if devnode_installed && self.vdd_settings_path().exists() {
            return Ok(());
        }

        let mut inf_path = Self::find_file_where(&self.install_dir, VDD_INF_NAME, 6, &Self::inf_matches_host_arch);
        if inf_path.is_none() {
            self.download_release_package()?;
            inf_path = Self::find_file_where(&self.install_dir, VDD_INF_NAME, 6, &Self::inf_matches_host_arch);
        }

        // The devnode (Root\MttVDD) is already present from a previous run —
        // download_release_package() above only needed to (re-)stage
        // install_dir\Dependencies\vdd_settings.xml for configure_mode(),
        // not to reinstall the driver.
        if devnode_installed {
            return if self.vdd_settings_path().exists() {
                println!("✅ Virtual Display Driver already installed — staged Dependencies assets under {}", self.install_dir.display());
                Ok(())
            } else {
                Err(format!(
                    "{} still not found after staging — check the release package layout",
                    self.vdd_settings_path().display()
                ))
            };
        }

        let inf_path = inf_path.ok_or_else(|| {
            format!(
                "no {} for arch {} found under {} after extraction — check the release package layout",
                VDD_INF_NAME,
                std::env::consts::ARCH,
                self.install_dir.display()
            )
        })?;

        let devcon_path = Self::find_file(&self.install_dir, "devcon.exe", 6).ok_or_else(|| {
            format!(
                "devcon.exe not found under {} after extraction — check the release package layout",
                self.install_dir.display()
            )
        })?;

        self.run_elevated_devcon(&devcon_path, &["install", &inf_path.to_string_lossy(), VDD_HARDWARE_ID])?;

        if self.is_installed() {
            println!("✅ Virtual Display Driver installed (Root\\MttVDD)");
            Ok(())
        } else {
            Err("devcon install completed but Root\\MttVDD still isn't present — the UAC prompt may have been declined".to_string())
        }
    }

    /// Downloads the signed driver package and extracts it into
    /// `install_dir`. Same pattern as
    /// `app_launcher::download_and_extract_retroarch`:
    ///   - `powershell -Command Invoke-WebRequest -Uri <VDD_RELEASE_ZIP_URL>
    ///     -OutFile <install_dir>\vdd.zip`
    ///   - extract via `tar -xf` (bsdtar handles .zip too, no 7z needed)
    ///   - leaves `install_dir\SignedDrivers\...\MttVDD.inf` and
    ///     `install_dir\Dependencies\devcon.exe` + `vdd_settings.xml` in
    ///     place for [`ensure_installed`] to find via [`find_file`].
    fn download_release_package(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.install_dir)
            .map_err(|e| format!("failed to create {}: {e}", self.install_dir.display()))?;

        let archive_path = self.install_dir.join("vdd.zip");

        println!("📦 Virtual Display Driver not found — downloading {}", VDD_RELEASE_ZIP_URL);
        let download_ps = format!(
            "$ProgressPreference='SilentlyContinue'; Invoke-WebRequest -Uri '{url}' -OutFile '{archive}'",
            url = VDD_RELEASE_ZIP_URL,
            archive = archive_path.display(),
        );
        let status = Command::new("powershell")
            .args(["-NoProfile", "-Command", &download_ps])
            .status()
            .map_err(|e| format!("failed to spawn powershell for VDD download: {e}"))?;
        if !status.success() {
            return Err(format!("VDD download failed (powershell exit {:?})", status.code()));
        }

        // bsdtar (built into Windows 10 1803+) can read .zip archives, same
        // as the RetroArch 7z extraction path.
        let extracted = Command::new("tar")
            .args(["-xf", &archive_path.to_string_lossy(), "-C", &self.install_dir.to_string_lossy()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        let _ = std::fs::remove_file(&archive_path);

        if extracted {
            Ok(())
        } else {
            Err(format!(
                "could not extract {} — install 7-Zip (https://www.7-zip.org/) or extract it into {} manually",
                archive_path.display(),
                self.install_dir.display(),
            ))
        }
    }

    /// Searches `dir` (up to `max_depth` levels deep) for a file named
    /// `name` (case-insensitive), returning its path if found. Mirrors
    /// `app_launcher::find_retroarch_exe` — the release zip's internal
    /// folder layout isn't assumed.
    fn find_file(dir: &Path, name: &str, max_depth: u32) -> Option<PathBuf> {
        Self::find_file_where(dir, name, max_depth, &|_| true)
    }

    /// Like [`find_file`], but a candidate is only accepted if `accept`
    /// returns `true` for its path — the search continues past
    /// name-matching-but-rejected candidates (e.g. an `MttVDD.inf` for the
    /// wrong CPU architecture) instead of stopping at the first one.
    fn find_file_where(dir: &Path, name: &str, max_depth: u32, accept: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
        if max_depth == 0 {
            return None;
        }
        let mut subdirs = Vec::new();
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                subdirs.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
                && accept(&path)
            {
                return Some(path);
            }
        }
        subdirs.into_iter().find_map(|sub| Self::find_file_where(&sub, name, max_depth - 1, accept))
    }

    /// Does `inf_path` contain a `[Standard.NT<arch>]` section matching the
    /// CPU architecture this binary was built for? Release packages (like
    /// the local `C:\VDD.Control.25.7.23` reference) ship one `MttVDD.inf`
    /// per architecture (`x86\VDD\MttVDD.inf` → `NTamd64`, `ARM64\VDD\MttVDD.inf`
    /// → `NTARM64`) — installing the wrong one fails devcon with exit code 2
    /// before any UAC prompt is meaningful.
    ///
    /// INF files here are UTF-16LE; this checks for the tag's bytes in both
    /// UTF-16LE and plain ASCII form so it works regardless of encoding.
    fn inf_matches_host_arch(inf_path: &Path) -> bool {
        let tag = match std::env::consts::ARCH {
            "x86_64" => "NTamd64",
            "aarch64" => "NTARM64",
            "x86" => "NTx86",
            _ => return false,
        };
        let Ok(bytes) = std::fs::read(inf_path) else {
            return false;
        };
        let ascii = tag.as_bytes();
        let utf16le: Vec<u8> = tag.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        Self::bytes_contain_ignore_case(&bytes, ascii) || Self::bytes_contain_ignore_case(&bytes, &utf16le)
    }

    fn bytes_contain_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty()
            && haystack.len() >= needle.len()
            && haystack.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
    }

    /// Runs `devcon_path` with `args`, elevated via a single UAC prompt.
    ///
    /// `Start-Process -Verb RunAs -Wait -PassThru` so (a) Nova doesn't
    /// proceed until the install/enable/disable has actually completed, and
    /// (b) `devcon`'s own exit code is propagated as the launched
    /// powershell's exit code — a UAC decline surfaces as a terminating
    /// error (non-zero exit) just the same as a devcon failure.
    ///
    /// Used by [`ensure_installed`] and [`set_enabled`] — all
    /// device-tree mutations require admin.
    fn run_elevated_devcon(&self, devcon_path: &Path, args: &[&str]) -> Result<(), String> {
        let arg_list = args
            .iter()
            .map(|a| format!("'{}'", a.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");

        let ps = format!(
            "$p = Start-Process -FilePath '{devcon}' -ArgumentList {args} -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
            devcon = devcon_path.display(),
            args = arg_list,
        );

        println!("🔐 Requesting elevation: devcon {}", args.join(" "));
        let status = Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps])
            .status()
            .map_err(|e| format!("failed to spawn powershell for elevated devcon: {e}"))?;

        if status.success() {
            Ok(())
        } else {
            Err(format!("elevated `devcon {}` failed or was cancelled (exit {:?})", args.join(" "), status.code()))
        }
    }

    // -------------------------------------------------------------
    // Resolution / refresh rate configuration
    // -------------------------------------------------------------

    /// Makes the driver advertise (and, once enabled, present) a mode
    /// matching the Moonlight client's negotiated `width x height @
    /// refresh_hz`.
    ///
    /// The MttVDD driver reads its mode table from `vdd_settings.xml`
    /// (`<resolutions><resolution><width>/<height>/<refresh_rate></resolution>`,
    /// see C:\VDD.Control.25.7.23\Dependencies\vdd_settings.xml for the
    /// schema) and its global refresh-rate list from
    /// `<global><g_refresh_rate>`.
    ///
    /// Steps:
    ///   1. [`vdd_settings_path`] — locate the live settings file (Nova's own
    ///      copy under `install_dir\Dependencies`, or wherever
    ///      [`VDD_REGISTRY_VALUE`] already points).
    ///   2. [`patch_vdd_settings_xml`] — insert a `<resolution>` entry for
    ///      `(width, height, refresh_hz)` if not already present (avoid
    ///      growing the file unbounded across repeated client connections
    ///      with the same mode), and ensure `refresh_hz` is in the
    ///      `<global><g_refresh_rate>` list.
    ///   3. [`ensure_vddpath_registry`] — point `VDDPATH` at the directory
    ///      containing that file, so MttVDD.dll actually reads it instead of
    ///      its built-in fallback table.
    ///
    /// The driver only re-reads this file/registry value on devnode
    /// (re)start, so [`activate_for_stream`] must call this BEFORE
    /// [`set_enabled`], or cycle (disable+enable) the devnode if it was
    /// already enabled.
    pub fn configure_mode(&self, width: u32, height: u32, refresh_hz: u32) -> Result<(), String> {
        println!("🖥️  Configuring VDD target topology to: {width}x{height}@{refresh_hz}Hz");

        let settings_path = self.vdd_settings_path();
        if !settings_path.exists() {
            return Err(format!(
                "{} not found — run ensure_installed() first",
                settings_path.display()
            ));
        }
        let settings_dir = settings_path
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", settings_path.display()))?
            .to_path_buf();

        let changed = Self::patch_vdd_settings_xml(&settings_path, width, height, refresh_hz)?;
        if changed {
            println!("📝 vdd_settings.xml updated with {width}x{height}@{refresh_hz}Hz ({})", settings_path.display());
        } else {
            println!("✅ vdd_settings.xml already advertises {width}x{height}@{refresh_hz}Hz ({})", settings_path.display());
        }

        self.ensure_vddpath_registry(&settings_dir)?;

        Ok(())
    }

    /// Locates the `vdd_settings.xml` the installed driver should read at
    /// runtime.
    ///
    /// MttVDD.dll opens `HKLM\SOFTWARE\MikeTheTech\VirtualDisplayDriver`
    /// (`VDD_REGISTRY_KEY`) and reads its `VDDPATH` (`VDD_REGISTRY_VALUE`)
    /// string value for the settings directory, falling back to
    /// `C:\VirtualDisplayDriver\` / `C:\IddSampleDriver\` if absent —
    /// confirmed via strings extracted from the installed `MttVDD.dll` and
    /// `VDD Control.exe`. Neither the registry key nor either fallback
    /// directory exists on this dev box, so:
    ///   - if `VDDPATH` is already set AND `<VDDPATH>\vdd_settings.xml`
    ///     exists, use that (don't fight a manually-configured install);
    ///   - otherwise use Nova's own bundled copy at
    ///     `install_dir\Dependencies\vdd_settings.xml` (extracted by
    ///     [`ensure_installed`]) and have [`ensure_vddpath_registry`] point
    ///     `VDDPATH` there.
    fn vdd_settings_path(&self) -> PathBuf {
        if let Some(dir) = Self::read_vddpath_registry() {
            let candidate = dir.join("vdd_settings.xml");
            if candidate.exists() {
                return candidate;
            }
        }
        self.install_dir.join("Dependencies").join("vdd_settings.xml")
    }

    /// Reads `HKLM\SOFTWARE\MikeTheTech\VirtualDisplayDriver\VDDPATH`
    /// (REG_SZ). Returns `None` if the key/value doesn't exist (e.g. the
    /// driver hasn't been pointed anywhere yet and is running on its
    /// built-in fallback table) — a plain read, no elevation needed.
    fn read_vddpath_registry() -> Option<PathBuf> {
        unsafe {
            let key_path: Vec<u16> = VDD_REGISTRY_KEY.encode_utf16().chain(std::iter::once(0)).collect();
            let mut hkey = HKEY(std::ptr::null_mut());
            if RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(key_path.as_ptr()), 0, KEY_READ, &mut hkey).0 != 0 {
                return None;
            }

            let value_name: Vec<u16> = VDD_REGISTRY_VALUE.encode_utf16().chain(std::iter::once(0)).collect();
            let mut buf = [0u8; 1024];
            let mut buf_len = buf.len() as u32;
            let mut value_type = REG_VALUE_TYPE(0);
            let status = RegQueryValueExW(hkey, PCWSTR(value_name.as_ptr()), None, Some(&mut value_type), Some(buf.as_mut_ptr()), Some(&mut buf_len));
            let _ = RegCloseKey(hkey);

            if status.0 != 0 || value_type != REG_SZ || buf_len < 2 {
                return None;
            }

            let u16s: Vec<u16> = buf[..buf_len as usize].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let s = String::from_utf16_lossy(&u16s).trim_end_matches('\0').to_string();
            if s.is_empty() {
                None
            } else {
                Some(PathBuf::from(s))
            }
        }
    }

    /// Ensures `HKLM\SOFTWARE\MikeTheTech\VirtualDisplayDriver\VDDPATH`
    /// points at `dir` (the directory holding the `vdd_settings.xml` that
    /// [`configure_mode`] just patched). No-ops if it's already set
    /// correctly.
    ///
    /// **Primary path**: `RegCreateKeyExW` + `RegSetValueExW` directly — works
    /// if Nova is already running elevated.
    ///
    /// **Fallback path**: on `ERROR_ACCESS_DENIED`, same dual-layer pattern as
    /// [`set_enabled`] — shell out to an elevated `reg.exe add` via
    /// [`run_elevated_devcon`]'s `Start-Process -Verb RunAs` idiom (single UAC
    /// prompt).
    fn ensure_vddpath_registry(&self, dir: &Path) -> Result<(), String> {
        let dir_str = dir.to_string_lossy().to_string();

        if let Some(current) = Self::read_vddpath_registry() {
            if current.to_string_lossy().eq_ignore_ascii_case(&dir_str) {
                return Ok(());
            }
        }

        match Self::write_vddpath_registry_native(&dir_str) {
            Ok(()) => {
                println!("✅ VDDPATH registry value set to {dir_str}");
                Ok(())
            }
            Err(e) if e.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0) => {
                println!("🔐 Native VDDPATH registry write requires elevation — falling back to reg.exe");
                self.run_elevated_reg_set_vddpath(&dir_str)
            }
            Err(e) => Err(format!("failed to write VDDPATH registry value: {e}")),
        }
    }

    /// Does the actual `RegCreateKeyExW`/`RegSetValueExW` for
    /// [`ensure_vddpath_registry`]. Returns `Err` (commonly
    /// `ERROR_ACCESS_DENIED` when unelevated) on failure.
    fn write_vddpath_registry_native(dir: &str) -> windows::core::Result<()> {
        unsafe {
            let key_path: Vec<u16> = VDD_REGISTRY_KEY.encode_utf16().chain(std::iter::once(0)).collect();
            let mut hkey = HKEY(std::ptr::null_mut());
            let status = RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(key_path.as_ptr()),
                0,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut hkey,
                None,
            );
            if status.0 != 0 {
                return Err(HRESULT::from_win32(status.0).into());
            }

            let value_name: Vec<u16> = VDD_REGISTRY_VALUE.encode_utf16().chain(std::iter::once(0)).collect();
            let data: Vec<u8> = dir.encode_utf16().chain(std::iter::once(0)).flat_map(|c| c.to_le_bytes()).collect();
            let status = RegSetValueExW(hkey, PCWSTR(value_name.as_ptr()), 0, REG_SZ, Some(&data));
            let _ = RegCloseKey(hkey);

            if status.0 != 0 {
                return Err(HRESULT::from_win32(status.0).into());
            }
            Ok(())
        }
    }

    /// Elevated fallback for [`ensure_vddpath_registry`]: `reg.exe add
    /// HKLM\<VDD_REGISTRY_KEY> /v VDDPATH /t REG_SZ /d <dir> /f`, via the same
    /// `Start-Process -Verb RunAs -Wait -PassThru` idiom as
    /// [`run_elevated_devcon`] (single UAC prompt, real exit code
    /// propagated).
    fn run_elevated_reg_set_vddpath(&self, dir: &str) -> Result<(), String> {
        let args = [
            "add",
            &format!(r"HKLM\{VDD_REGISTRY_KEY}"),
            "/v",
            VDD_REGISTRY_VALUE,
            "/t",
            "REG_SZ",
            "/d",
            dir,
            "/f",
        ];
        let arg_list = args
            .iter()
            .map(|a| format!("'{}'", a.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");

        let ps = format!(
            "$p = Start-Process -FilePath 'reg.exe' -ArgumentList {args} -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
            args = arg_list,
        );

        println!("🔐 Requesting elevation: reg {}", args.join(" "));
        let status = Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps])
            .status()
            .map_err(|e| format!("failed to spawn powershell for elevated reg.exe: {e}"))?;

        if status.success() {
            Ok(())
        } else {
            Err(format!("elevated `reg {}` failed or was cancelled (exit {:?})", args.join(" "), status.code()))
        }
    }

    /// Parses `path`, inserts a `<resolution>` entry for
    /// `(width, height, refresh_hz)` if no entry with this `(width, height)`
    /// already exists, and ensures `refresh_hz` is listed under
    /// `<global><g_refresh_rate>`, then writes the file back if anything
    /// changed. Returns whether the file was modified.
    ///
    /// Deliberately does plain string surgery rather than pulling in an XML
    /// crate — this file is only ever written by Nova (in this exact format)
    /// and the VDD Control GUI, so the `<resolutions>...</resolutions>` /
    /// `<global>...</global>` block structure is stable. Kept as a separate
    /// function from [`configure_mode`] so it can be unit tested against a
    /// fixture XML without touching the real driver config.
    fn patch_vdd_settings_xml(path: &Path, width: u32, height: u32, refresh_hz: u32) -> Result<bool, String> {
        let xml = std::fs::read_to_string(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let mut out = xml;
        let mut changed = false;

        if !Self::xml_has_resolution(&out, width, height) {
            let entry = format!(
                "        <resolution>\n            <width>{width}</width>\n            <height>{height}</height>\n            <refresh_rate>{refresh_hz}</refresh_rate>\n        </resolution>\n"
            );
            out = Self::insert_before(&out, "</resolutions>", &entry)
                .ok_or_else(|| format!("{} has no </resolutions> closing tag", path.display()))?;
            changed = true;
        }

        if !Self::xml_has_global_refresh_rate(&out, refresh_hz) {
            let entry = format!("\t\t<g_refresh_rate>{refresh_hz}</g_refresh_rate>\n");
            out = Self::insert_before(&out, "</global>", &entry)
                .ok_or_else(|| format!("{} has no </global> closing tag", path.display()))?;
            changed = true;
        }

        if changed {
            std::fs::write(path, &out).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
        }

        Ok(changed)
    }

    /// Ensures `<HDRPlus>true</HDRPlus>` is set in `vdd_settings.xml`.
    ///
    /// MttVDD ships with `HDRPlus=false` which means the virtual display does
    /// not advertise HDR10 capability in its EDID. Without this flag:
    ///   - Windows reports `advancedColorSupported=true` via the CCD API but
    ///     cannot actually apply the Advanced Color mode → infinite ACCESS_LOST
    ///     storm when `SET_ADVANCED_COLOR_STATE` is called.
    ///   - DXGI stays in BGRA8 (SDR) mode and the FP16→P010 VP blit produces
    ///     zero-byte frames.
    ///
    /// With `HDRPlus=true` the driver advertises proper HDR10 capability, the
    /// Advanced Color toggle works cleanly, and DXGI switches to
    /// `R16G16B16A16_FLOAT` (linear scRGB) as expected.
    ///
    /// Must be called before the devnode cycle in [`ensure_enabled_at_boot`]
    /// because MttVDD only re-reads the XML on (re)start.
    fn ensure_hdr_options(path: &Path) -> Result<bool, String> {
        let xml = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;

        // Simple tag-value replacement: swap false→true only when the tag exists.
        let mut out = xml;
        let mut changed = false;

        for (old, new) in [
            ("<HDRPlus>false</HDRPlus>",         "<HDRPlus>true</HDRPlus>"),
            ("<HDRPlus>False</HDRPlus>",         "<HDRPlus>true</HDRPlus>"),
        ] {
            if out.contains(old) {
                out = out.replace(old, new);
                changed = true;
            }
        }

        if changed {
            std::fs::write(path, &out)
                .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
            println!("📝 vdd_settings.xml: HDRPlus enabled for Advanced Color / HDR10 support");
        }
        Ok(changed)
    }

    /// Does the `<resolutions>...</resolutions>` block contain a
    /// `<resolution>` entry with this exact `(width, height)`, regardless of
    /// its `<refresh_rate>` (the global refresh-rate list applies to every
    /// resolution, so one entry per `(width, height)` is enough).
    fn xml_has_resolution(xml: &str, width: u32, height: u32) -> bool {
        let Some(start) = xml.find("<resolutions>") else { return false };
        let Some(end) = xml[start..].find("</resolutions>") else { return false };
        let block = &xml[start..start + end];

        let width_tag = format!("<width>{width}</width>");
        let height_tag = format!("<height>{height}</height>");
        block.split("<resolution>").skip(1).any(|entry| entry.contains(&width_tag) && entry.contains(&height_tag))
    }

    /// Does the `<global>...</global>` block already list `refresh_hz` under
    /// `<g_refresh_rate>`?
    fn xml_has_global_refresh_rate(xml: &str, refresh_hz: u32) -> bool {
        let Some(start) = xml.find("<global>") else { return false };
        let Some(end) = xml[start..].find("</global>") else { return false };
        let block = &xml[start..start + end];
        block.contains(&format!("<g_refresh_rate>{refresh_hz}</g_refresh_rate>"))
    }

    /// Returns a copy of `haystack` with `insertion` spliced in at the start
    /// of the line containing the first occurrence of `marker` (so
    /// `insertion`'s own indentation/newlines line up cleanly with the
    /// existing closing tag rather than getting jammed mid-line), or `None`
    /// if `marker` isn't found.
    fn insert_before(haystack: &str, marker: &str, insertion: &str) -> Option<String> {
        let marker_idx = haystack.find(marker)?;
        let line_start = haystack[..marker_idx].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let mut out = String::with_capacity(haystack.len() + insertion.len());
        out.push_str(&haystack[..line_start]);
        out.push_str(insertion);
        out.push_str(&haystack[line_start..]);
        Some(out)
    }

    // -------------------------------------------------------------
    // Enable / disable the monitor devnode
    // -------------------------------------------------------------

    /// Enables or disables the `Root\MttVDD` devnode in place (no
    /// reinstall/removal — same devnode, just `DICS_ENABLE`/`DICS_DISABLE`).
    ///
    /// [`open_devnode`] for the live `HDEVINFO` + `SP_DEVINFO_DATA`, then
    /// `SetupDiSetClassInstallParamsW` with an `SP_PROPCHANGE_PARAMS`
    /// (`DIF_PROPERTYCHANGE`, `DICS_ENABLE`/`DICS_DISABLE`,
    /// `DICS_FLAG_GLOBAL`) followed by `SetupDiCallClassInstaller`
    /// (`DIF_PROPERTYCHANGE`, ...) to commit it.
    ///
    /// `build.rs` embeds a `requireAdministrator` manifest in the
    /// `nova-server` binary, so Nova's process token is always elevated and
    /// this native call succeeds directly — no `devcon`/UAC fallback (that
    /// path used to stall `activate_for_stream` mid-session if the prompt
    /// wasn't answered immediately, see git history).
    pub fn set_enabled(&mut self, enabled: bool) -> Result<(), String> {
        let action = if enabled { "enable" } else { "disable" };

        match Self::set_enabled_native(enabled) {
            Ok(true) => {
                println!("✅ Root\\MttVDD devnode {}d", action);
                Ok(())
            }
            Ok(false) => Err("Root\\MttVDD devnode not found — is the driver installed?".to_string()),
            Err(e) => Err(format!("SetupDi{} property-change failed: {e}", if enabled { "Enable" } else { "Disable" })),
        }
    }

    /// Does the actual `DIF_PROPERTYCHANGE` dance for [`set_enabled`].
    /// Returns `Ok(true)` if applied, `Ok(false)` if `Root\MttVDD` isn't
    /// present, or `Err` (commonly `ERROR_ACCESS_DENIED` when unelevated).
    fn set_enabled_native(enabled: bool) -> windows::core::Result<bool> {
        let Some((hdevinfo, devinfo_data)) = Self::open_devnode()? else {
            return Ok(false);
        };

        unsafe {
            let mut params = SP_PROPCHANGE_PARAMS {
                ClassInstallHeader: SP_CLASSINSTALL_HEADER {
                    cbSize: std::mem::size_of::<SP_CLASSINSTALL_HEADER>() as u32,
                    InstallFunction: DIF_PROPERTYCHANGE,
                },
                StateChange: if enabled { DICS_ENABLE } else { DICS_DISABLE },
                Scope: DICS_FLAG_GLOBAL,
                HwProfile: 0,
            };

            let result = SetupDiSetClassInstallParamsW(
                hdevinfo,
                Some(&devinfo_data),
                Some(&mut params.ClassInstallHeader as *mut SP_CLASSINSTALL_HEADER as *const SP_CLASSINSTALL_HEADER),
                std::mem::size_of::<SP_PROPCHANGE_PARAMS>() as u32,
            )
            .and_then(|()| SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, hdevinfo, Some(&devinfo_data)));

            let _ = SetupDiDestroyDeviceInfoList(hdevinfo);
            result.map(|()| true)
        }
    }

    /// Brings `Root\MttVDD` up once, at process startup, and leaves it
    /// enabled for the server's entire lifetime.
    ///
    /// [`ensure_installed`] + [`configure_mode`]`(width, height, refresh_hz)`,
    /// then [`set_enabled`]`(true)` only if [`is_enabled`] currently reports
    /// `false`. Unlike the old `activate_for_stream`, this never disables an
    /// already-enabled devnode: toggling MttVDD off and back on while a
    /// session is starting is what produced a transient 800x600 mode that
    /// raced the client's requested resolution (the "800x600 race
    /// condition"). Bringing the devnode up once, here, before any client can
    /// connect, means it has long since settled at the configured mode by the
    /// time [`activate_for_stream`] runs.
    ///
    /// Finally, [`isolate_virtual_display_at_boot`] snaps the virtual
    /// display's mode to 0x0 so it sits dormant — disconnected from the
    /// desktop, not an enumerable DXGI output, not visible in Display
    /// Settings — until a stream activates it.
    ///
    /// Returns the virtual display's GDI device name (e.g. `\\.\DISPLAY29`),
    /// if [`wait_for_virtual_display_device_name`] found one. `run()` passes
    /// this to `capture::DesktopCapturer::new_excluding` so the boot-time
    /// capturer's "first output" fallback can't bind to the virtual display.
    pub fn ensure_enabled_at_boot(&mut self, width: u32, height: u32, refresh_hz: u32) -> Result<Option<String>, String> {
        self.ensure_installed()?;
        self.configure_mode(width, height, refresh_hz)?;

        // Pre-seed every resolution/refresh-rate pair from SupportedDisplayModeList
        // into vdd_settings.xml BEFORE the single devnode cycle below. MttVDD
        // re-reads the XML only on (re)start — doing this once at boot means
        // activate_for_stream's force_resolution call always finds the requested
        // mode in the driver's table, regardless of which resolution the client
        // asks for, without needing another devcon cycle mid-session (which would
        // race the DXGI rebind). configure_mode is idempotent for already-present
        // entries, so this is cheap on repeat boots.
        let all_modes: &[(u32, u32, u32)] = &[
            (1280, 720, 30), (1280, 720, 60), (1280, 720, 120),
            (1920, 1080, 30), (1920, 1080, 60), (1920, 1080, 120),
            (2560, 1440, 60), (2560, 1440, 120),
            (3840, 2160, 30), (3840, 2160, 60), (3840, 2160, 120),
        ];
        for &(w, h, hz) in all_modes {
            if let Err(e) = self.configure_mode(w, h, hz) {
                println!("⚠️  VDD mode pre-seed {w}x{h}@{hz}Hz failed: {e}");
            }
        }
        println!("📋 VDD mode table pre-seeded with {} resolutions (720p/1080p/1440p/4K × 30/60/120Hz)",
            all_modes.len());

        // Enable HDRPlus in vdd_settings.xml so the driver advertises HDR10
        // capability in its EDID. Required for DisplayConfigSetDeviceInfo
        // (SET_ADVANCED_COLOR_STATE) to actually work — without it the VDD reports
        // no HDR in its EDID and Windows triggers an infinite ACCESS_LOST storm.
        match Self::ensure_hdr_options(&self.vdd_settings_path()) {
            Ok(true)  => println!("📝 HDRPlus enabled in vdd_settings.xml — driver will advertise HDR10 after devnode cycle"),
            Ok(false) => println!("✅ HDRPlus already enabled in vdd_settings.xml"),
            Err(e)    => println!("⚠️  Could not patch HDRPlus in vdd_settings.xml: {e}"),
        }

        // Cycle the devnode once so MttVDD re-reads the freshly-written
        // vdd_settings.xml (IddCx only picks up the XML on (re)start).
        if self.is_enabled() {
            println!("✅ Root\\MttVDD already enabled — cycling to refresh mode table");
            self.set_enabled(false)?;
        }
        self.set_enabled(true)?;

        // While the devnode is briefly live, grab its GDI device name and
        // deactivate its CCD path. This updates the topology database so
        // Windows won't auto-promote the VDD to primary if a prior session
        // crashed and left it as the active primary in the CCD database.
        let virtual_device = Self::wait_for_virtual_display_device_name();
        Self::isolate_virtual_display_at_boot(virtual_device.as_deref());

        // Hardware-level disable: the devnode is now fully dormant.
        //
        // Why this beats CCD-parking alone:
        //   A CCD-parked (but still enabled) devnode remains visible to the
        //   PnP enumerator, DXGI, and the firmware display registry. On a
        //   graphics-stack crash the OS re-reads those structures at the next
        //   boot and can still hand the VDD the $GDI_PRIMARY flag before any
        //   Nova code has a chance to run.
        //   A SetupAPI-disabled devnode (CM_PROB_DISABLED) is invisible to all
        //   of those paths — Windows cannot promote it to primary regardless of
        //   what the CCD database says.
        //
        // Intentionally runs AFTER isolate_virtual_display_at_boot so the CCD
        // database is consistent ("VDD inactive") before the devnode disappears.
        self.set_enabled(false)
            .unwrap_or_else(|e| println!("⚠️  Could not hardware-disable VDD at boot (non-fatal): {e}"));
        println!("🕶️  Root\\MttVDD devnode hardware-disabled at boot — will enable on client connect only");

        // Return None: VDD is disabled and absent from GDI enumeration.
        // lib.rs passes this to WgcCapturer::new_excluding(None) so the WGC
        // capturer binds the physical primary with no VDD exclusion needed.
        Ok(None)
    }

    // -------------------------------------------------------------
    // Device naming
    // -------------------------------------------------------------

    /// Set the `Root\MttVDD` adapter devnode's friendly name to `display_name`.
    ///
    /// Windows uses `SPDRP_FRIENDLYNAME` as the device label shown in Device
    /// Manager and, on most Windows 11 builds, in the "Display" page of Settings
    /// when the virtual monitor is active.  The change is written to the registry
    /// immediately and persists until Nova renames it again (e.g. on the next
    /// session with a different client) or the driver is reinstalled.
    ///
    /// Called from `lib.rs` right after `activate_for_stream` succeeds, so the
    /// virtual display is already the active output when the rename lands.
    /// Requires administrator privileges (embedded manifest guarantees this).
    pub fn rename_devnode(&self, display_name: &str) -> Result<(), String> {
        let Some((hdevinfo, mut devinfo_data)) = Self::open_devnode()
            .map_err(|e| format!("enumerate display devnodes: {e}"))? else {
            return Err("Root\\MttVDD devnode not found — driver not installed?".to_string());
        };

        // REG_SZ: UTF-16LE, null-terminated.
        let name_w: Vec<u16> = display_name.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes: Vec<u8>   = name_w.iter().flat_map(|c| c.to_le_bytes()).collect();

        let result = unsafe {
            SetupDiSetDeviceRegistryPropertyW(
                hdevinfo,
                &mut devinfo_data,
                SPDRP_FRIENDLYNAME,
                Some(&bytes),
            )
        };
        unsafe { let _ = SetupDiDestroyDeviceInfoList(hdevinfo); }
        result.map_err(|e| format!("rename VDD devnode to \"{display_name}\": {e}"))
    }

    /// Phase 5.3: immediately after enabling `Root\MttVDD`, detaches the
    /// virtual display by snapping its mode to 0x0 via the legacy
    /// `ChangeDisplaySettingsExW`/`DEVMODEW` API — not CCD.
    ///
    /// Phase 5.2 did this via `SetDisplayConfig` (clearing
    /// `DISPLAYCONFIG_PATH_ACTIVE` on just the virtual display's CCD path),
    /// but that left the IDD part of an extended desktop at 800x600: it could
    /// still become DXGI output 0, so `DesktopCapturer::new()`'s startup
    /// default bound to the dummy monitor instead of the physical primary for
    /// App 1/Desktop. Setting the resolution to 0x0 is the documented way to
    /// fully disconnect a display from the desktop while keeping its
    /// driver/devnode enabled and enumerable — [`activate_for_stream`]'s
    /// later [`force_resolution`] call (a real width/height with
    /// `CDS_RESET`) is what brings it back for a stream.
    ///
    /// Takes the virtual display's GDI device name (`\\.\DISPLAYn`), already
    /// resolved by [`ensure_enabled_at_boot`] via
    /// [`wait_for_virtual_display_device_name`] — `None` if it never appeared
    /// in GDI enumeration.
    ///
    /// Uses [`ccd_deactivate_vdd_path`] to clear `DISPLAYCONFIG_PATH_ACTIVE`
    /// on the VDD's CCD path via `SetDisplayConfig(SDC_USE_SUPPLIED_DISPLAY_CONFIG
    /// | SDC_SAVE_TO_DATABASE)`.  The legacy `ChangeDisplaySettingsExW(0×0)`
    /// approach is intentionally NOT used: MttVDD's IddCx driver rejects 0×0
    /// with `DISP_CHANGE_BADMODE (-2)` because 0×0 is not in its mode list,
    /// so the old two-call stage+apply sequence always bailed out early and the
    /// VDD remained in the active topology — visible in Display Settings as a
    /// second monitor and sometimes promoted to primary.
    ///
    /// Best-effort: logs success or failure and never blocks
    /// [`ensure_enabled_at_boot`]'s return.
    fn isolate_virtual_display_at_boot(virtual_device: Option<&str>) {
        let Some(virtual_device) = virtual_device else {
            println!("⚠️  Root\\MttVDD enabled but never appeared in GDI enumeration — cannot isolate it at boot");
            return;
        };
        match Self::ccd_deactivate_vdd_path(virtual_device) {
            Ok(true)  => println!("✅ {virtual_device} already dormant at boot — no isolation needed"),
            Ok(false) => println!("🕶️  {virtual_device} removed from active CCD topology — dormant until a stream activates it"),
            Err(e)    => println!("⚠️  Failed to isolate {virtual_device} at boot: {e}"),
        }
    }

    /// Clears `DISPLAYCONFIG_PATH_ACTIVE` on every active CCD path whose
    /// source resolves to `virtual_device`, leaving all physical paths
    /// untouched.  `SDC_SAVE_TO_DATABASE` persists the "VDD dormant" state so
    /// Windows does not auto-promote the virtual monitor to primary on the
    /// next reboot.
    ///
    /// This is the exact inverse of [`deactivate_other_paths`] — that function
    /// kills the physical paths and keeps the VDD active; this one kills the
    /// VDD path and keeps everything else active.  Both use the same
    /// `QDC_ALL_PATHS` + `SDC_USE_SUPPLIED_DISPLAY_CONFIG` pattern that is
    /// proven to round-trip on this driver stack.
    ///
    /// Returns `Ok(true)` when the VDD was already inactive (no-op),
    /// `Ok(false)` when it was found and successfully deactivated.
    fn ccd_deactivate_vdd_path(virtual_device: &str) -> Result<bool, String> {
        let (mut paths, modes) = Self::query_all_topology()?;

        let mut found = false;
        for path in &mut paths {
            if path.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
                continue; // already inactive — leave it alone
            }
            let is_vdd = Self::gdi_device_name_for_source(
                path.sourceInfo.adapterId,
                path.sourceInfo.id,
            )
            .is_some_and(|name| name.eq_ignore_ascii_case(virtual_device));

            if is_vdd {
                path.flags &= !DISPLAYCONFIG_PATH_ACTIVE;
                found = true;
            }
        }

        if !found {
            return Ok(true); // VDD not in active topology — already dormant
        }

        let flags = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_SAVE_TO_DATABASE | SDC_ALLOW_CHANGES;
        let status = unsafe { SetDisplayConfig(Some(&paths), Some(&modes), flags) };
        if status != 0 {
            return Err(format!("SetDisplayConfig could not deactivate VDD CCD path (error {status})"));
        }
        Ok(false)
    }

    // -------------------------------------------------------------
    // Stream start/stop orchestration (Task 2)
    // -------------------------------------------------------------

    /// Full "App 5 launched" sequence. See module-level "DXGI Handoff
    /// Strategy" docs for what happens after this returns (capture/encoder
    /// re-hook, which lives outside this module).
    ///
    /// Steps:
    ///   0. [`cache_default_audio_endpoint`] → `self.saved_audio_endpoint` —
    ///      done FIRST, before any display or audio mutation, so the cached
    ///      id is the *real* host speaker regardless of what the topology
    ///      swap below does to the default-device guess.
    ///   1. [`query_database_topology`] → local `saved_topology` — a snapshot
    ///      of the CCD database's full current paths/modes (including
    ///      inactive ones, e.g. the dormant virtual-display path left by
    ///      [`isolate_virtual_display_at_boot`]), before anything is mutated.
    ///      `Root\MttVDD` is already enabled ([`ensure_enabled_at_boot`] ran
    ///      at startup), so this is the "how do I put everything back"
    ///      baseline. `QDC_DATABASE_CURRENT`, not `QDC_ONLY_ACTIVE_PATHS`/
    ///      `QDC_ALL_PATHS` — see [`Self::saved_topology`] for why.
    ///   2. [`configure_mode`]`(width, height, refresh_hz)` — usually a
    ///      no-op at this point: [`ensure_enabled_at_boot`] already cycled
    ///      `Root\MttVDD` once at startup (before
    ///      [`crate::capture::DesktopCapturer::new`] bound to anything)
    ///      specifically so the driver's mode table is current. No devnode
    ///      cycle here — doing it per-stream raced the DXGI rebind that
    ///      follows this call against the IDD's transient post-cycle mode
    ///      (see `run()`'s "Enable Root\\MttVDD ONCE" comment). If
    ///      `width`x`height`@`refresh_hz` truly isn't in the driver's table
    ///      yet (first time this resolution has ever been requested),
    ///      [`force_resolution`] below fails with `DISP_CHANGE_BADMODE` (-2)
    ///      — but `configure_mode` has by then persisted it to
    ///      `vdd_settings.xml`, so the next boot's cycle picks it up.
    ///   3. [`set_primary_display`]`(virtual_device)` — repositions the
    ///      virtual monitor's CCD source to the (0,0) desktop origin (the new
    ///      primary), shifting whatever was there beside it. Uses
    ///      `QueryDisplayConfig`/`SetDisplayConfig` (CCD), not the legacy
    ///      `ChangeDisplaySettingsExW`/`CDS_SET_PRIMARY` path — the latter is
    ///      rejected outright (`DISP_CHANGE_FAILED`) on this driver stack even
    ///      as a no-op (see diagnostics in the test module).
    ///   4. [`deactivate_other_paths`]`(virtual_device)` — clears
    ///      `DISPLAYCONFIG_PATH_ACTIVE` on every other active path, so the
    ///      virtual display becomes the ONLY active output ("true headless").
    ///   5. [`force_resolution`]`(&virtual_device, width, height, refresh_hz)`
    ///      — explicitly snaps the virtual monitor's active mode to the
    ///      Moonlight-requested resolution/refresh via
    ///      `ChangeDisplaySettingsExW`. Steps 3-4 (CCD) only route/activate
    ///      paths; they never change which advertised mode is current, so
    ///      without this the virtual display stays at whatever mode it booted
    ///      into (typically the 800x600 failsafe).
    ///   6. [`wait_for_display_resolution`], then `self.saved_topology` /
    ///      `self.active_device_name` / `self.active = true`.
    pub fn activate_for_stream(&mut self, width: u32, height: u32, refresh_hz: u32) -> Result<(), String> {
        self.saved_audio_endpoint = Self::cache_default_audio_endpoint();
        match &self.saved_audio_endpoint {
            Some(_) => println!("🔊 Cached current default audio endpoint before mutating display/audio state"),
            None => println!("⚠️  Could not query the current default audio endpoint — restore-on-disconnect will be skipped"),
        }

        let saved_topology = Self::query_database_topology()?;
        println!("📸 Saved current display topology from the CCD database ({} path(s))", saved_topology.0.len());

        // configure_mode must run BEFORE enabling the devnode so MttVDD reads
        // the correct vdd_settings.xml on startup (IddCx reads it at start time).
        self.configure_mode(width, height, refresh_hz)?;

        // On-demand hardware enable — the devnode was left disabled at boot by
        // ensure_enabled_at_boot. Enabling here gives DXGI/CCD a live IddCx
        // adapter exactly when the stream needs it and for no longer.
        if !self.is_enabled() {
            self.set_enabled(true)?;
        }

        let virtual_device = Self::wait_for_virtual_display_device_name()
            .ok_or_else(|| "timed out waiting for the virtual display to appear in GDI enumeration".to_string())?;

        // ── Topology guard: prevent primary-display hijack on enable ──────────
        // When the devnode comes online Windows consults the CCD database to
        // determine where the new output belongs. If a prior crashed session
        // left the VDD as the stored primary, Windows may hand it the
        // DISPLAYCONFIG_PATH_ACTIVE flag with position (0,0) before any Nova
        // code runs — causing a black screen on the physical monitor.
        //
        // Calling ccd_deactivate_vdd_path immediately after the devnode
        // appears in GDI ensures the VDD path is explicitly NOT active in the
        // CCD topology. The physical GPU paths keep their DISPLAYCONFIG_PATH_ACTIVE
        // flags and remain at (0,0). The subsequent SDC_TOPOLOGY_EXTEND then
        // re-adds the VDD as a *secondary* (not primary) display.
        match Self::ccd_deactivate_vdd_path(&virtual_device) {
            Ok(true)  => {} // already inactive — no primary steal occurred
            Ok(false) => println!("🛡️  Topology guard: cleared VDD from active CCD path on enable — physical primary retained"),
            Err(e)    => println!("⚠️  Topology guard ccd_deactivate_vdd_path: {e} — proceeding"),
        }

        // The devnode is now hardware-enabled but CCD-inactive. Use
        // SDC_TOPOLOGY_EXTEND to add the VDD to the desktop as a secondary
        // display (not primary) so force_resolution can target it.
        println!("🔌 Extending desktop topology to include VDD via SDC_TOPOLOGY_EXTEND...");
        let ext_status = unsafe {
            SetDisplayConfig(
                None::<&[DISPLAYCONFIG_PATH_INFO]>,
                None::<&[DISPLAYCONFIG_MODE_INFO]>,
                SDC_APPLY | SDC_TOPOLOGY_EXTEND,
            )
        };
        if ext_status == 0 {
            println!("✅ VDD re-added to the extended desktop — waiting for topology to settle...");
            std::thread::sleep(std::time::Duration::from_millis(500));
        } else {
            println!("⚠️  SDC_TOPOLOGY_EXTEND failed (error {ext_status}) — force_resolution may not work on a disconnected VDD");
        }
        Self::force_resolution(&virtual_device, width, height, refresh_hz);
        Self::wait_for_display_resolution(&virtual_device, width, height);

        // Atomic primary promotion + physical path deactivation in a single
        // SetDisplayConfig call. A two-step sequence (set_primary_display then
        // deactivate_other_paths) creates a window where the VDD is the active
        // primary but physical monitors are still in the topology — UAC prompts
        // and window focus land on the headless VDD canvas during that gap.
        // The combined call collapses both mutations into one transaction, so
        // the physical monitors are never simultaneously "primary" and "active"
        // while the VDD owns position (0,0).
        match Self::set_primary_and_deactivate_others(&virtual_device) {
            Ok(()) => println!("🖥️  {virtual_device} ({width}x{height}@{refresh_hz}Hz) atomically set as primary — physical paths deactivated (true headless)"),
            Err(e) => println!("⚠️  Atomic headless transition failed: {e} — stream may not be fully isolated"),
        }

        // Re-snap the resolution: SDC_ALLOW_CHANGES in set_primary_and_deactivate_others
        // can silently select a different mode (often the 800x600 failsafe) even
        // though configure_mode already wrote the correct entry into vdd_settings.xml.
        Self::force_resolution(&virtual_device, width, height, refresh_hz);

        // Wait for the mode to stabilise so the DXGI rebind below sees the
        // correct DesktopCoordinates rather than a transient resolution.
        Self::wait_for_display_resolution(&virtual_device, width, height);

        self.saved_topology = Some(saved_topology);
        self.active_device_name = Some(virtual_device);
        self.active_resolution = Some((width, height));
        self.active = true;

        Ok(())
    }

    /// Reverses [`activate_for_stream`]. Safe to call even if activation
    /// partially failed (each step checks `self.active`/`Option`s and
    /// no-ops if its corresponding setup step didn't happen).
    ///
    /// Steps:
    ///   1. [`restore_topology`]`(self.saved_topology, self.active_device_name)`
    ///      — re-applies the exact pre-activation path/mode array, forcing
    ///      the virtual display's path inactive regardless of what it was
    ///      doing beforehand (see [`restore_topology`] doc comment).
    ///      `Root\MttVDD` itself stays enabled — [`ensure_enabled_at_boot`]
    ///      made enable/disable a boot-time-only operation.
    ///   2. Force the default audio playback device back to
    ///      `self.saved_audio_endpoint` via `SetDefaultAudioDevice` —
    ///      explicit restore to the speaker GUID cached *before* launch,
    ///      rather than letting Windows guess (which lands on the NVIDIA HDMI
    ///      endpoint once the virtual display's audio device has appeared).
    ///   3. Clear `saved_topology` / `active_device_name` /
    ///      `saved_audio_endpoint`, `self.active = false`.
    pub fn deactivate_after_stream(&mut self) -> Result<(), String> {
        if !self.active {
            return Ok(());
        }

        let mut error: Option<String> = None;

        // Disable Advanced Color before restoring topology — the VDD is still
        // the active primary here, so the target id is still valid. After
        // restore_topology the path is gone and the call would fail.
        if let Err(e) = self.set_active_display_hdr(false) {
            // Non-fatal: log and continue; it only matters if HDR was enabled.
            println!("⚠️  Could not disable Advanced Color on stream end: {e}");
        }

        if let Some(saved) = self.saved_topology.take() {
            match Self::restore_topology(&saved, self.active_device_name.as_deref()) {
                Ok(()) => println!("🖥️  Restored the original display topology — virtual display detached"),
                Err(e) => {
                    println!("⚠️  Failed to restore the original display topology: {e}");
                    error = Some(e);
                }
            }
        }

        // Hardware-disable the devnode after topology is restored.
        // Physical paths are already active again at this point, so disabling
        // the VDD devnode here is safe — there is no active display that
        // references it. The disable ensures the VDD is hardware-dormant for
        // the idle period between sessions and survives a crash: if the
        // process is killed before this line runs, ensure_enabled_at_boot on
        // the next reboot will cycle the devnode and re-run
        // isolate_virtual_display_at_boot, repairing the CCD database entry.
        if let Err(e) = self.set_enabled(false) {
            println!("⚠️  Could not hardware-disable VDD devnode on stream end: {e}");
            error.get_or_insert(e);
        } else {
            println!("🕶️  Root\\MttVDD devnode hardware-disabled — dormant until next client connect");
        }

        if let Some(endpoint) = self.saved_audio_endpoint.take() {
            if unsafe { SetDefaultAudioDevice(endpoint.as_ptr()) } == 0 {
                println!("🔊 Default audio output forced back to the cached pre-stream speaker");
            } else {
                let e = "SetDefaultAudioDevice failed to restore the cached audio endpoint".to_string();
                println!("⚠️  {e} — check Windows sound settings");
                error.get_or_insert(e);
            }
        }

        self.active_device_name = None;
        self.active_resolution = None;
        self.active = false;

        match error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// GDI device name (`\\.\DISPLAYn`) of the virtual monitor, once
    /// [`activate_for_stream`] has run. Used by the capture re-hook
    /// (`rebind_capture_and_encoder` in lib.rs) to pick the right
    /// `IDXGIOutput`.
    pub fn active_device_name(&self) -> Option<&str> {
        self.active_device_name.as_deref()
    }

    /// `(width, height)` of the mode [`activate_for_stream`] requested, once
    /// it has run. Used by the steady-state DXGI rebind in lib.rs's `run()`
    /// to pass `expected_size` to
    /// [`crate::capture::DesktopCapturer::rebind`].
    #[allow(dead_code)]
    pub fn active_resolution(&self) -> Option<(u32, u32)> {
        self.active_resolution
    }

    /// Enables or disables Windows Advanced Color (HDR10 / scRGB rendering) on
    /// the currently-active VDD output using `DisplayConfigSetDeviceInfo` with
    /// `DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE` (type value 10).
    ///
    /// When enabled, Windows switches the VDD's frame buffer to
    /// `DXGI_FORMAT_R16G16B16A16_FLOAT` (linear scRGB). The shim's
    /// `InitColorConversion(is_hdr=true)` VP path (`FP16 scRGB → P010 BT.2020 PQ`)
    /// then has the correct HDR source data to feed NVENC's P010 input surface.
    ///
    /// This triggers `DXGI_ERROR_ACCESS_LOST` on the existing duplication handle —
    /// the capture loop's ACCESS_LOST handler re-creates the duplication in the new
    /// (HDR / FP16) color mode automatically, so no explicit rebind is needed here.
    ///
    /// Best-effort: logs success or failure and never panics. The VDD may not
    /// support Advanced Color (e.g. MttVDD versions that don't expose HDR modes);
    /// in that case the encoder will fall back to encoding BGRA8 as HEVC Main10
    /// which produces incorrect (over-bright) colors but doesn't crash.
    /// Returns whether the active VDD output reports `advancedColorSupported` via
    /// the CCD `GET_ADVANCED_COLOR_INFO` query (type=9).
    ///
    /// Requires `HDRPlus=true` in `vdd_settings.xml` (patched at boot by
    /// [`ensure_hdr_options`]) so the driver advertises HDR10 in its EDID.
    /// Without it the query returns `advancedColorSupported=false`.
    ///
    /// All shipping MttVDD releases (including 25.7.23) use `IddCx0102` (IddCx 1.2),
    /// which does not implement IddCx Advanced Color callbacks. Calling
    /// `SET_ADVANCED_COLOR_STATE` on an already-HDR display (Windows may auto-enable
    /// Advanced Color when the VDD EDID declares HDR10 capability) triggers a
    /// spurious re-initialization storm. [`set_active_display_hdr`] guards against
    /// this with an idempotency check — it skips the SET call if the display is
    /// already in the desired state.
    pub fn is_advanced_color_supported(&self) -> bool {
        let Some(device_name) = self.active_device_name.as_deref() else {
            return false;
        };
        let Ok((adapter_id, target_id)) = Self::find_target_for_device_name(device_name) else {
            return false;
        };
        let Some((supported, enabled)) = Self::query_advanced_color_info(adapter_id, target_id) else {
            return false;
        };
        println!("🔍 VDD Advanced Color on {device_name}: supported={supported}, enabled={enabled}");
        supported
    }

    /// Queries `DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO` (type=9) for the given
    /// CCD target. Returns `(advancedColorSupported, advancedColorEnabled)` from
    /// the bitfield union, or `None` if the API call fails.
    ///
    /// The full `DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO` struct (wingdi.h) is:
    ///   header (20 bytes) + value u32 + colorEncoding u32 + bitsPerColorChannel u32
    /// All fields must be present so the `size` field in the header matches.
    fn query_advanced_color_info(adapter_id: LUID, target_id: u32) -> Option<(bool, bool)> {
        #[repr(C)]
        struct GetAdvancedColorInfo {
            header:                DISPLAYCONFIG_DEVICE_INFO_HEADER,
            value:                 u32, // bits: 0=supported, 1=enabled, 2=wideColorEnforced, 3=forceDisabled
            color_encoding:        u32, // DISPLAYCONFIG_COLOR_ENCODING enum
            bits_per_color_channel: u32,
        }
        let mut info = GetAdvancedColorInfo {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(9i32), // GET_ADVANCED_COLOR_INFO
                size:   std::mem::size_of::<GetAdvancedColorInfo>() as u32,
                adapterId: adapter_id,
                id:        target_id,
            },
            value: 0,
            color_encoding: 0,
            bits_per_color_channel: 0,
        };
        let ret = unsafe {
            DisplayConfigGetDeviceInfo(
                &mut info.header as *mut DISPLAYCONFIG_DEVICE_INFO_HEADER,
            )
        };
        if ret != 0 {
            println!("⚠️  GET_ADVANCED_COLOR_INFO failed: error {ret}");
            return None;
        }
        Some(((info.value & 1) != 0, (info.value & 2) != 0))
    }

    /// Shared helper: walk active CCD paths and return the (adapterId, targetId)
    /// for the path whose source GDI device name matches `device_name`.
    fn find_target_for_device_name(device_name: &str) -> Result<(LUID, u32), String> {
        unsafe {
            let mut n_paths: u32 = 0;
            let mut n_modes: u32 = 0;
            let e = GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut n_paths, &mut n_modes);
            if e.0 != 0 { return Err(format!("GetDisplayConfigBufferSizes: {}", e.0)); }

            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); n_paths as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); n_modes as usize];
            let e = QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS,
                &mut n_paths, paths.as_mut_ptr(),
                &mut n_modes, modes.as_mut_ptr(), None);
            if e.0 != 0 { return Err(format!("QueryDisplayConfig: {}", e.0)); }

            for path in &paths[..n_paths as usize] {
                let mut src: DISPLAYCONFIG_SOURCE_DEVICE_NAME = std::mem::zeroed();
                src.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                };
                if DisplayConfigGetDeviceInfo(&mut src.header as *mut _ as *mut _) != 0 { continue; }
                let name = String::from_utf16_lossy(&src.viewGdiDeviceName);
                if name.trim_end_matches('\0').eq_ignore_ascii_case(device_name) {
                    return Ok((path.targetInfo.adapterId, path.targetInfo.id));
                }
            }
            Err(format!("No active path for {device_name}"))
        }
    }

    pub fn set_active_display_hdr(&self, enable: bool) -> Result<(), String> {
        let device_name = self.active_device_name.as_deref()
            .ok_or_else(|| "VDD not active — cannot set Advanced Color".to_string())?;

        let (adapter_id, target_id) = Self::find_target_for_device_name(device_name)
            .map_err(|e| format!("Cannot locate VDD target for Advanced Color: {e}"))?;

        // Idempotency guard: skip SET_ADVANCED_COLOR_STATE if the display is
        // already in the desired state. On IddCx 1.2 (all shipping MttVDD),
        // calling SET when the state is already correct triggers a spurious
        // ACCESS_LOST re-initialization storm (Windows may auto-enable Advanced
        // Color when the VDD EDID declares HDR10, so this guard fires often).
        if let Some((_supported, currently_enabled)) =
            Self::query_advanced_color_info(adapter_id, target_id)
        {
            if currently_enabled == enable {
                println!("✅ Advanced Color on {device_name} already {} — skipping SET",
                    if enable { "enabled" } else { "disabled" });
                return Ok(());
            }
        }

        Self::set_advanced_color_raw(adapter_id, target_id, enable).map(|()| {
            println!("🎨 Advanced Color (HDR) {} on {device_name}",
                if enable { "enabled → DXGI will provide FP16 scRGB frames" }
                else      { "disabled → DXGI back to BGRA8 SDR" });
        }).map_err(|e| {
            let msg = format!(
                "DisplayConfigSetDeviceInfo(SET_ADVANCED_COLOR_STATE={}) on {device_name} failed: {e}",
                enable as u8
            );
            println!("⚠️  {msg}");
            msg
        })
    }

    /// Forces a complete Advanced Color state cycle (SDR → HDR), bypassing the
    /// idempotency guard in [`set_active_display_hdr`].
    ///
    /// When the VDD devnode is re-enabled after a session (`HDRPlus=true` in
    /// the EDID), Windows auto-enables Advanced Color before any Nova code runs.
    /// A subsequent `set_active_display_hdr(true)` then hits the idempotency
    /// guard ("already enabled"), skips the SET call, and the MDCV/MaxCLL SEI
    /// metadata from the previous session is never refreshed — resulting in
    /// washed-out colours on Xbox reconnect.
    ///
    /// This method sends `SET_ADVANCED_COLOR_STATE=0` unconditionally, waits for
    /// the display to settle in SDR, then sends `SET_ADVANCED_COLOR_STATE=1` —
    /// guaranteeing a pristine MDCV/MaxCLL push regardless of the cached state.
    pub fn force_hdr_reconnect_cycle(&self) -> Result<(), String> {
        let device_name = self.active_device_name.as_deref()
            .ok_or_else(|| "VDD not active — cannot cycle Advanced Color".to_string())?;

        let (adapter_id, target_id) = Self::find_target_for_device_name(device_name)
            .map_err(|e| format!("HDR reconnect cycle: cannot locate VDD target: {e}"))?;

        // Force-disable regardless of reported state — clears Windows' cached
        // Advanced Color envelope from any previous session.
        let _ = Self::set_advanced_color_raw(adapter_id, target_id, false);
        println!("🔄 HDR reconnect: forced Advanced Color OFF (SDR flush) on {device_name}");

        // Allow the display pipeline to settle before re-enabling.
        std::thread::sleep(std::time::Duration::from_millis(150));

        // Force-enable — triggers a fresh MDCV/MaxCLL SEI sequence.
        Self::set_advanced_color_raw(adapter_id, target_id, true).map_err(|e| {
            format!("HDR reconnect: Advanced Color re-enable failed on {device_name}: {e}")
        })?;
        println!("🔄 HDR reconnect: Advanced Color force-re-enabled on {device_name} — fresh HDR10 metadata");

        Ok(())
    }

    /// Sends `DisplayConfigSetDeviceInfo(SET_ADVANCED_COLOR_STATE)` for the
    /// given (adapterId, targetId) with no idempotency guard. Shared by
    /// [`set_active_display_hdr`] (guarded path) and
    /// [`force_hdr_reconnect_cycle`] (bypass path).
    fn set_advanced_color_raw(adapter_id: LUID, target_id: u32, enable: bool) -> Result<(), String> {
        // DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE layout (wingdi.h):
        //   DISPLAYCONFIG_DEVICE_INFO_HEADER  header;
        //   union { struct { UINT32 enableAdvancedColor:1; UINT32 reserved:31; }; UINT32 value; };
        // Type 10 = SET variant (type 9 = GET variant).
        #[repr(C)]
        struct SetAdvancedColorState {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
            value:  u32,
        }
        let req = SetAdvancedColorState {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(10i32),
                size:   std::mem::size_of::<SetAdvancedColorState>() as u32,
                adapterId: adapter_id,
                id:        target_id,
            },
            value: u32::from(enable),
        };
        let ret = unsafe {
            DisplayConfigSetDeviceInfo(
                &req.header as *const DISPLAYCONFIG_DEVICE_INFO_HEADER,
            )
        };
        if ret == 0 { Ok(()) } else {
            Err(format!("SET_ADVANCED_COLOR_STATE={} error {ret}", enable as u8))
        }
    }

    /// Re-snaps the already-active VDD to `width`×`height`@`refresh_hz`
    /// without touching CCD topology. Use when `activate_for_stream`'s
    /// `force_resolution` + `wait_for_display_resolution` timed out and the
    /// VDD settled at the wrong mode (e.g. 4K@120fps taking longer than 3 s
    /// to stabilise, leaving the VDD at 1080p). Calling this at connect time
    /// gives the VDD a second chance to reach the right mode before the
    /// capture and encoder are rebound.
    pub fn re_snap_resolution(&self, width: u32, height: u32, refresh_hz: u32) {
        if let Some(device) = self.active_device_name.as_deref() {
            println!("📐 VDD re-snap: forcing {device} → {width}x{height}@{refresh_hz}Hz");
            Self::force_resolution(device, width, height, refresh_hz);
            Self::wait_for_display_resolution(device, width, height);
        }
    }

    /// Polls [`find_virtual_display_device_name`] for up to ~2 seconds,
    /// since GDI's display enumeration can lag the devnode-arrival event
    /// (`WM_DISPLAYCHANGE`) by a frame or two right after [`set_enabled`]
    /// returns.
    /// Blocking poll: re-applying the CCD topology
    /// ([`set_primary_display`]/[`deactivate_other_paths`], just above) makes
    /// Windows momentarily drop the IDD to a failsafe 800x600 mode before it
    /// picks up the `vdd_settings.xml` mode [`configure_mode`] wrote — the
    /// DXGI rebind that follows `activate_for_stream`'s return would
    /// otherwise see `DesktopCoordinates` for 800x600 and lock the encoder at
    /// the wrong resolution until the next topology change.
    ///
    /// Polls `EnumDisplaySettingsW(ENUM_CURRENT_SETTINGS)` for `device_name`
    /// every 100ms for up to 3 seconds, returning as soon as it reports
    /// `width`x`height`. Does NOT return early on timeout-without-match —
    /// logs the last-seen mode and proceeds anyway, so a stuck IDD is visible
    /// in the log instead of silently producing a wrong-sized stream.
    fn wait_for_display_resolution(device_name: &str, width: u32, height: u32) {
        let name_w: Vec<u16> = device_name.encode_utf16().chain(std::iter::once(0)).collect();
        const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(3000);
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

        let deadline = std::time::Instant::now() + TIMEOUT;
        let mut last_seen = (0u32, 0u32);
        loop {
            let mut mode = DEVMODEW {
                dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            let ok = unsafe { EnumDisplaySettingsW(PCWSTR(name_w.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode).as_bool() };
            if ok {
                last_seen = (mode.dmPelsWidth, mode.dmPelsHeight);
                if last_seen == (width, height) {
                    println!("✅ {device_name} stabilized at {width}x{height} — proceeding with DXGI rebind");
                    return;
                }
            }
            if std::time::Instant::now() >= deadline {
                println!(
                    "⚠️  {device_name} still reporting {}x{} after {}ms (wanted {width}x{height}) — proceeding anyway, DXGI rebind may need another pass",
                    last_seen.0, last_seen.1, TIMEOUT.as_millis()
                );
                return;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Phase 5.2, directive 2: explicitly snaps `device_name`'s active mode
    /// to `width`x`height`@`refresh_hz` via the legacy
    /// `ChangeDisplaySettingsExW`/`DEVMODEW` API.
    ///
    /// CCD's `SetDisplayConfig` (used by [`set_primary_display`] and
    /// [`deactivate_other_paths`], called just before this in
    /// [`activate_for_stream`]) only routes which paths are active and where
    /// their sources sit on the desktop — it has no effect on which of a
    /// target's advertised modes is "current". Left alone, the virtual
    /// display can stay parked on its 800x600 failsafe mode indefinitely
    /// while CCD-wise it's already the active primary.
    ///
    /// This doesn't invent a new mode: [`configure_mode`] already wrote
    /// `width`x`height`@`refresh_hz` into `vdd_settings.xml` and the devnode
    /// has been enabled since boot, so the driver already advertises this
    /// exact mode as one `ChangeDisplaySettingsExW` can select — this call
    /// just picks it.
    ///
    /// Reads the current `DEVMODEW` via `EnumDisplaySettingsW
    /// (ENUM_CURRENT_SETTINGS)` first and overwrites only
    /// `dmPelsWidth`/`dmPelsHeight`/`dmDisplayFrequency` (via `dmFields =
    /// DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY`), leaving every
    /// other field (color depth, position, orientation, ...) as Windows
    /// reported it.
    ///
    /// Uses `CDS_UPDATEREGISTRY | CDS_GLOBAL | CDS_RESET`. `CDS_UPDATEREGISTRY`
    /// persists the mode to the registry as well as applying it.
    /// `CDS_GLOBAL` writes that registry change for all users rather than
    /// just the current session — relevant since Nova runs headless/as a
    /// service, not an interactive user session. `CDS_RESET` is the critical
    /// addition for the "stuck at 800x600" symptom: without it, Windows can
    /// decide the requested mode is "equivalent enough" to the current one
    /// and silently skip applying it, even though `ChangeDisplaySettingsExW`
    /// still returns `DISP_CHANGE_SUCCESSFUL`. `CDS_RESET` forces the mode
    /// change to be applied immediately and unconditionally. Elsewhere in
    /// this module, `CDS_SET_PRIMARY`-class calls are documented as returning
    /// `DISP_CHANGE_FAILED` outright on this driver stack even as a true
    /// no-op (see `set_primary_noop_diagnostic`); this call's `dmFields`
    /// (size/refresh, not position/primary) differ enough that it may not hit
    /// the same rejection, but the `DISP_CHANGE` result is logged either way
    /// so a rejection is visible. [`wait_for_display_resolution`], called
    /// right after this, is the other signal — if it times out without ever
    /// seeing `width`x`height`, this call was rejected or ineffective.
    ///
    /// Best-effort: logs success or failure and never returns an error;
    /// [`activate_for_stream`] proceeds to [`wait_for_display_resolution`]
    /// regardless.
    fn force_resolution(device_name: &str, width: u32, height: u32, refresh_hz: u32) {
        let name_w: Vec<u16> = device_name.encode_utf16().chain(std::iter::once(0)).collect();

        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let _ = unsafe { EnumDisplaySettingsW(PCWSTR(name_w.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode) };

        mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
        mode.dmPelsWidth = width;
        mode.dmPelsHeight = height;
        mode.dmDisplayFrequency = refresh_hz;
        // Defensive, see isolate_virtual_display_at_boot. NOTE: -2 below is
        // DISP_CHANGE_BADMODE ("mode not supported"), not BADPARAM — if this
        // still fails, width x height @ refresh_hz isn't in the driver's
        // currently-loaded mode table (see activate_for_stream step 2).
        mode.dmDriverExtra = 0;

        let result = unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(name_w.as_ptr()),
                Some(&mode),
                HWND(std::ptr::null_mut()),
                CDS_UPDATEREGISTRY | CDS_GLOBAL | CDS_RESET,
                None,
            )
        };

        if result == DISP_CHANGE_SUCCESSFUL {
            println!("🔧 {device_name} resolution snapped to {width}x{height}@{refresh_hz}Hz (DISP_CHANGE_SUCCESSFUL)");
        } else {
            println!("⚠️  ChangeDisplaySettingsExW failed to snap {device_name} to {width}x{height}@{refresh_hz}Hz (DISP_CHANGE {})", result.0);
        }
    }

    fn wait_for_virtual_display_device_name() -> Option<String> {
        for attempt in 0..10 {
            if let Some(name) = Self::find_virtual_display_device_name() {
                return Some(name);
            }
            if attempt < 9 {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
        None
    }

    /// Finds the GDI device name (`\\.\DISPLAYn`) of the enabled virtual
    /// monitor by enumerating adapter-level `EnumDisplayDevicesW(None, i,
    /// ...)` entries and matching `DeviceID` against [`VDD_HARDWARE_ID`]
    /// (case-insensitive) — the same hardware-id substring match
    /// [`open_devnode`] uses, just via GDI's enumeration instead of SetupAPI.
    fn find_virtual_display_device_name() -> Option<String> {
        unsafe {
            let mut index = 0u32;
            loop {
                let mut device = DISPLAY_DEVICEW {
                    cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                    ..Default::default()
                };
                if !EnumDisplayDevicesW(PCWSTR::null(), index, &mut device, 0).as_bool() {
                    return None;
                }

                let device_id = String::from_utf16_lossy(&device.DeviceID);
                if device_id.to_uppercase().contains("MTTVDD") {
                    return Some(String::from_utf16_lossy(&device.DeviceName).trim_end_matches('\0').to_string());
                }

                index += 1;
            }
        }
    }

    // -------------------------------------------------------------
    // CCD / GDI helpers (primary display switching)
    // -------------------------------------------------------------

    /// Records the current primary display's GDI device name, resolution,
    /// refresh rate, and position. Used only by the `#[ignore]`d CCD/GDI
    /// diagnostics below — [`activate_for_stream`]/[`deactivate_after_stream`]
    /// save/restore the full CCD topology instead (see `saved_topology`,
    /// [`Self::query_all_topology`], [`Self::restore_topology`]).
    #[cfg(test)]
    fn snapshot_current_primary() -> Result<DisplaySnapshot, String> {
        unsafe {
            let mut index = 0u32;
            loop {
                let mut device = DISPLAY_DEVICEW {
                    cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                    ..Default::default()
                };
                if !EnumDisplayDevicesW(PCWSTR::null(), index, &mut device, 0).as_bool() {
                    return Err("EnumDisplayDevicesW found no display with DISPLAY_DEVICE_PRIMARY_DEVICE set".to_string());
                }

                if device.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE != 0 {
                    let device_name = String::from_utf16_lossy(&device.DeviceName).trim_end_matches('\0').to_string();

                    let name_w: Vec<u16> = device_name.encode_utf16().chain(std::iter::once(0)).collect();
                    let mut mode = DEVMODEW {
                        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                        ..Default::default()
                    };
                    if !EnumDisplaySettingsW(PCWSTR(name_w.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode).as_bool() {
                        return Err(format!("EnumDisplaySettingsW(ENUM_CURRENT_SETTINGS) failed for {device_name}"));
                    }

                    let position = mode.Anonymous1.Anonymous2.dmPosition;

                    return Ok(DisplaySnapshot {
                        device_name,
                        width: mode.dmPelsWidth,
                        height: mode.dmPelsHeight,
                        refresh_hz: mode.dmDisplayFrequency,
                        position: (position.x, position.y),
                    });
                }

                index += 1;
            }
        }
    }

    /// Shared `GetDisplayConfigBufferSizes`/`QueryDisplayConfig` loop backing
    /// [`query_active_topology`] (`QDC_ONLY_ACTIVE_PATHS`),
    /// [`query_all_topology`] (`QDC_ALL_PATHS`), and
    /// [`query_database_topology`] (`QDC_DATABASE_CURRENT`).
    ///
    /// Loops on `ERROR_INSUFFICIENT_BUFFER` since the topology can change
    /// between [`GetDisplayConfigBufferSizes`] and [`QueryDisplayConfig`]
    /// (e.g. a monitor is plugged/unplugged concurrently).
    ///
    /// `QueryDisplayConfig`'s `currentTopologyId` out-param is documented as
    /// **required (non-null) when `flags == QDC_DATABASE_CURRENT`, and
    /// required to be null for every other flag** — passing the wrong one
    /// either way is `ERROR_INVALID_PARAMETER`/87. [`query_database_topology`]
    /// hitting exactly this (always passing `None`) was the cause of App 5's
    /// pre-activation Error 87, even though `GetDisplayConfigBufferSizes` and
    /// `QueryDisplayConfig` were already called with the same `flags`.
    fn query_topology(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>), String> {
        unsafe {
            loop {
                let mut num_paths = 0u32;
                let mut num_modes = 0u32;
                let err = GetDisplayConfigBufferSizes(flags, &mut num_paths, &mut num_modes);
                if err.0 != 0 {
                    return Err(format!("GetDisplayConfigBufferSizes failed (error {})", err.0));
                }

                let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); num_paths as usize];
                let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); num_modes as usize];
                let mut out_paths = num_paths;
                let mut out_modes = num_modes;

                let mut topology_id = DISPLAYCONFIG_TOPOLOGY_ID::default();
                let topology_id_ptr: Option<*mut DISPLAYCONFIG_TOPOLOGY_ID> = if flags == QDC_DATABASE_CURRENT {
                    Some(&mut topology_id as *mut DISPLAYCONFIG_TOPOLOGY_ID)
                } else {
                    None
                };

                let err = QueryDisplayConfig(
                    flags,
                    &mut out_paths,
                    paths.as_mut_ptr(),
                    &mut out_modes,
                    modes.as_mut_ptr(),
                    topology_id_ptr,
                );
                if err == ERROR_INSUFFICIENT_BUFFER {
                    continue;
                }
                if err.0 != 0 {
                    return Err(format!("QueryDisplayConfig failed (error {})", err.0));
                }

                paths.truncate(out_paths as usize);
                modes.truncate(out_modes as usize);
                return Ok((paths, modes));
            }
        }
    }

    /// Queries the active CCD display topology: one [`DISPLAYCONFIG_PATH_INFO`]
    /// per active path plus the backing source/target mode info array.
    #[allow(dead_code)]
    fn query_active_topology() -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>), String> {
        Self::query_topology(QDC_ONLY_ACTIVE_PATHS)
    }

    /// Queries the full CCD display topology, including paths that are
    /// currently inactive (e.g. a physical monitor about to be detached, or
    /// the virtual display's path before it's activated). Used by
    /// [`deactivate_other_paths`], which needs the about-to-be-deactivated
    /// physical path(s) present in the array (with `DISPLAYCONFIG_PATH_ACTIVE`
    /// cleared) — `SDC_USE_SUPPLIED_DISPLAY_CONFIG` with an all-paths array is
    /// the documented way to flip individual paths active/inactive without
    /// disturbing the rest of the topology.
    ///
    /// Used by [`deactivate_other_paths`] to toggle individual paths
    /// active/inactive in place, and by [`restore_topology`] (a fresh call,
    /// not [`VirtualDisplay::saved_topology`] itself) as the apply payload
    /// when reverting that toggle.
    fn query_all_topology() -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>), String> {
        Self::query_topology(QDC_ALL_PATHS)
    }

    /// Queries the CCD database's current topology — every path/mode the
    /// database knows about (active or not), including inactive paths (e.g.
    /// the dormant virtual display before activation).
    ///
    /// Used by [`activate_for_stream`] for `self.saved_topology`, which
    /// [`restore_topology`] consults as a "what was active and where"
    /// reference. NOT a valid `SetDisplayConfig
    /// (SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_APPLY)` payload — that
    /// combination returns `ERROR_INVALID_PARAMETER`/87 for
    /// `QDC_DATABASE_CURRENT`-sourced arrays (see [`restore_topology`]).
    fn query_database_topology() -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>), String> {
        Self::query_topology(QDC_DATABASE_CURRENT)
    }

    /// Resolves a CCD path source (`adapterId`/`id`) to its GDI device name
    /// (e.g. `\\.\DISPLAY3`) via
    /// `DisplayConfigGetDeviceInfo(DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME)`.
    fn gdi_device_name_for_source(adapter_id: LUID, source_id: u32) -> Option<String> {
        let mut request = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: adapter_id,
                id: source_id,
            },
            ..Default::default()
        };
        let status = unsafe { DisplayConfigGetDeviceInfo(&mut request.header) };
        if status != 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&request.viewGdiDeviceName).trim_end_matches('\0').to_string())
    }

    #[allow(dead_code)]
    /// Makes the display identified by GDI device name `device_name` the new
    /// desktop primary, using the modern CCD topology API
    /// (`QueryDisplayConfig`/`SetDisplayConfig`) rather than the legacy
    /// `ChangeDisplaySettingsExW`/`CDS_SET_PRIMARY` path — the latter returns
    /// `DISP_CHANGE_FAILED` outright on this driver stack, even as a true
    /// no-op (see `set_primary_noop_diagnostic`).
    ///
    /// GDI's notion of "primary" is the display source sitting at desktop
    /// origin `(0, 0)`. So this:
    ///   1. Queries the active topology (`QDC_ONLY_ACTIVE_PATHS`).
    ///   2. Finds the source-mode entry for `device_name` (via
    ///      [`gdi_device_name_for_source`]) and the source-mode entry
    ///      currently positioned at `(0, 0)` (the display being demoted).
    ///   3. Swaps positions: `device_name`'s source mode moves to `(0, 0)`;
    ///      the demoted source moves to `(device_name's width, 0)` so it
    ///      doesn't overlap.
    ///   4. Re-applies the (otherwise unchanged) path/mode arrays via
    ///      `SetDisplayConfig(SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG |
    ///      SDC_SAVE_TO_DATABASE | SDC_ALLOW_CHANGES)`.
    ///
    /// This only repositions sources within the existing active-path set —
    /// it never marks a path active/inactive, so it can't blank a physical
    /// monitor outright (the safer of the two approaches discussed for
    /// keeping the host monitor dark: `app_launcher::sleep_displays()`
    /// already handles "dark" via `SC_MONITORPOWER` independently of which
    /// display is primary).
    ///
    /// No-ops (logs and returns `Ok`) if `device_name` is already at `(0,
    /// 0)`.
    fn set_primary_display(device_name: &str) -> Result<(), String> {
        let (paths, mut modes) = Self::query_active_topology()?;

        let mut target_idx: Option<usize> = None;
        let mut origin_idx: Option<usize> = None;

        for path in &paths {
            let idx = unsafe { path.sourceInfo.Anonymous.modeInfoIdx } as usize;
            if idx >= modes.len() || modes[idx].infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                continue;
            }

            let Some(name) = Self::gdi_device_name_for_source(path.sourceInfo.adapterId, path.sourceInfo.id) else {
                continue;
            };
            if name.eq_ignore_ascii_case(device_name) {
                target_idx = Some(idx);
            }

            let pos = unsafe { modes[idx].Anonymous.sourceMode.position };
            if pos.x == 0 && pos.y == 0 {
                origin_idx = Some(idx);
            }
        }

        let target_idx = target_idx.ok_or_else(|| format!("{device_name} not found among active CCD display paths"))?;

        if origin_idx == Some(target_idx) {
            println!("🖥️  {device_name} is already the desktop primary (CCD source at (0,0))");
            return Ok(());
        }

        let target_width = unsafe { modes[target_idx].Anonymous.sourceMode.width } as i32;

        if let Some(origin_idx) = origin_idx {
            modes[origin_idx].Anonymous.sourceMode.position = POINTL { x: target_width, y: 0 };
        }
        modes[target_idx].Anonymous.sourceMode.position = POINTL { x: 0, y: 0 };

        let flags = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_SAVE_TO_DATABASE | SDC_ALLOW_CHANGES;
        let status = unsafe { SetDisplayConfig(Some(&paths), Some(&modes), flags) };
        if status != 0 {
            return Err(format!("SetDisplayConfig failed to make {device_name} the CCD primary (error {status})"));
        }

        Ok(())
    }

    #[allow(dead_code)]
    /// "True headless": clears `DISPLAYCONFIG_PATH_ACTIVE` on every currently
    /// active CCD path except the one whose source resolves to
    /// `virtual_device`, then commits via `SetDisplayConfig`.
    ///
    /// Re-queries [`query_all_topology`] (not just the active subset) so the
    /// physical path(s) being deactivated stay present in the supplied array
    /// — `SetDisplayConfig(SDC_USE_SUPPLIED_DISPLAY_CONFIG)` with an all-paths
    /// array is the documented mechanism for toggling individual paths
    /// active/inactive in place, leaving the rest of the topology (source
    /// positions, modes, etc.) untouched. After this call, `virtual_device`
    /// is the only active display — equivalent to physically unplugging every
    /// other monitor, without touching `Root\MttVDD`'s devnode-enabled state.
    ///
    /// Returns `Err` if `virtual_device` isn't found among the currently
    /// active paths (it should always be, since [`activate_for_stream`] calls
    /// [`set_primary_display`]`(virtual_device)` first) or if
    /// `SetDisplayConfig` rejects the change.
    fn deactivate_other_paths(virtual_device: &str) -> Result<(), String> {
        let (mut paths, modes) = Self::query_all_topology()?;

        let mut found_virtual = false;
        for path in &mut paths {
            if path.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
                continue;
            }

            let idx = unsafe { path.sourceInfo.Anonymous.modeInfoIdx } as usize;
            let is_virtual = idx < modes.len()
                && modes[idx].infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
                && Self::gdi_device_name_for_source(path.sourceInfo.adapterId, path.sourceInfo.id)
                    .is_some_and(|name| name.eq_ignore_ascii_case(virtual_device));

            if is_virtual {
                found_virtual = true;
            } else {
                path.flags &= !DISPLAYCONFIG_PATH_ACTIVE;
            }
        }

        if !found_virtual {
            return Err(format!("{virtual_device} not found among active CCD display paths"));
        }

        let flags = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_SAVE_TO_DATABASE | SDC_ALLOW_CHANGES;
        let status = unsafe { SetDisplayConfig(Some(&paths), Some(&modes), flags) };
        if status != 0 {
            return Err(format!("SetDisplayConfig failed to deactivate other display paths (error {status})"));
        }

        Ok(())
    }

    /// Atomically promotes `device_name` to the desktop primary **and** clears
    /// `DISPLAYCONFIG_PATH_ACTIVE` on every other path in a single
    /// `SetDisplayConfig` call.
    ///
    /// The two-step sequence (`set_primary_display` then `deactivate_other_paths`)
    /// created a race window where the VDD held position (0,0) while physical
    /// monitors were still active — UAC prompts and window-manager focus events
    /// landed on the headless canvas during that gap. Collapsing both mutations
    /// into one transaction eliminates that window.
    ///
    /// Uses `QDC_ALL_PATHS` so inactive paths remain present in the applied
    /// array (required by `SDC_USE_SUPPLIED_DISPLAY_CONFIG`). Mode indices are
    /// valid for currently-active paths; inactive paths are left untouched.
    fn set_primary_and_deactivate_others(device_name: &str) -> Result<(), String> {
        let (mut paths, mut modes) = Self::query_all_topology()?;

        let mut found_virtual = false;

        for path in &mut paths {
            if path.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
                continue;
            }

            let idx = unsafe { path.sourceInfo.Anonymous.modeInfoIdx } as usize;
            if idx >= modes.len() || modes[idx].infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                continue;
            }

            let is_virtual = Self::gdi_device_name_for_source(
                path.sourceInfo.adapterId,
                path.sourceInfo.id,
            )
            .is_some_and(|n| n.eq_ignore_ascii_case(device_name));

            if is_virtual {
                // Place VDD at the desktop origin — GDI's definition of "primary".
                modes[idx].Anonymous.sourceMode.position = POINTL { x: 0, y: 0 };
                found_virtual = true;
            } else {
                // Deactivate every other active path in the same call.
                path.flags &= !DISPLAYCONFIG_PATH_ACTIVE;
            }
        }

        if !found_virtual {
            return Err(format!("{device_name} not found among active CCD paths — cannot promote to primary"));
        }

        let flags = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_SAVE_TO_DATABASE | SDC_ALLOW_CHANGES;
        let status = unsafe { SetDisplayConfig(Some(&paths), Some(&modes), flags) };
        if status != 0 {
            return Err(format!(
                "SetDisplayConfig failed to atomically set {device_name} as primary and deactivate other paths (error {status})"
            ));
        }

        Ok(())
    }

    /// Reverses [`deactivate_other_paths`]/[`set_primary_display`] by cloning
    /// `saved` itself — the [`query_database_topology`] snapshot
    /// [`activate_for_stream`] captures into `self.saved_topology` before it
    /// touches anything — instead of starting from a fresh
    /// [`query_all_topology`].
    ///
    /// `QDC_ALL_PATHS` returns one path entry per (source × every target the
    /// adapter could possibly drive that source to). On this system the
    /// physical source (`\\.\DISPLAY1`, one `(adapterId, sourceId)` pair)
    /// alone appears in 7 of its 29 entries — all resolving to the same GDI
    /// name via [`gdi_device_name_for_source`], which depends only on
    /// `sourceInfo.adapterId`/`id`, not `targetInfo`. The old approach
    /// matched `saved`'s single `\\.\DISPLAY1` entry against a fresh
    /// `QDC_ALL_PATHS` snapshot by GDI name, matched all 7 of those, and
    /// setting `DISPLAYCONFIG_PATH_ACTIVE` on all 7 asked `SetDisplayConfig`
    /// for one source driving 7 simultaneously-active targets —
    /// `ERROR_INVALID_PARAMETER`/87.
    ///
    /// `saved.0`/`saved.1` don't have this ambiguity: `QDC_DATABASE_CURRENT`
    /// returns one entry per *actually configured* path — on this system
    /// exactly two, `\\.\DISPLAY1` and `virtual_device`, both active (the
    /// extended-desktop state [`activate_for_stream`] started from) — each
    /// already pointing at its own valid
    /// `DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE`/`_TARGET` entries in `saved.1`.
    /// Cloning both vectors and:
    ///
    ///   1. clearing `DISPLAYCONFIG_PATH_ACTIVE` on the path whose source
    ///      resolves (via [`gdi_device_name_for_source`]) to `virtual_device`
    ///      — leaving its mode-info entries exactly as cloned, the same
    ///      "clear ACTIVE, leave modeInfoIdx pointing at still-valid
    ///      entries" pattern [`deactivate_other_paths`] uses (proven to
    ///      round-trip), and
    ///   2. zeroing each remaining active path's source-mode `position` to
    ///      `(0, 0)` — undoing [`set_primary_display`]'s swap so the lone
    ///      remaining display sits at the desktop origin/primary even if
    ///      `saved` itself reflects a topology where `virtual_device` was
    ///      primary,
    ///
    /// yields a minimal, internally-consistent array for
    /// `SetDisplayConfig(SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG |
    /// SDC_SAVE_TO_DATABASE | SDC_ALLOW_CHANGES)` — same flags as
    /// [`deactivate_other_paths`].
    fn restore_topology(
        saved: &(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>),
        virtual_device: Option<&str>,
    ) -> Result<(), String> {
        let mut paths = saved.0.clone();
        let mut modes = saved.1.clone();

        for path in &mut paths {
            if path.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
                continue;
            }

            let is_virtual = virtual_device.is_some_and(|vd| {
                Self::gdi_device_name_for_source(path.sourceInfo.adapterId, path.sourceInfo.id)
                    .is_some_and(|name| name.eq_ignore_ascii_case(vd))
            });

            if is_virtual {
                path.flags &= !DISPLAYCONFIG_PATH_ACTIVE;
                continue;
            }

            let idx = unsafe { path.sourceInfo.Anonymous.modeInfoIdx } as usize;
            if idx < modes.len() && modes[idx].infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                modes[idx].Anonymous.sourceMode.position = POINTL { x: 0, y: 0 };
            }
        }

        let flags = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_SAVE_TO_DATABASE | SDC_ALLOW_CHANGES;
        let status = unsafe { SetDisplayConfig(Some(&paths), Some(&modes), flags) };
        if status != 0 {
            println!("🔎 restore_topology failed (error {status}) — dumping path state:");
            for (i, p) in paths.iter().enumerate() {
                let name = Self::gdi_device_name_for_source(p.sourceInfo.adapterId, p.sourceInfo.id);
                let (src_idx, tgt_idx) = unsafe { (p.sourceInfo.Anonymous.modeInfoIdx, p.targetInfo.Anonymous.modeInfoIdx) };
                let src_valid = (src_idx as usize) < modes.len() && modes[src_idx as usize].infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE;
                let pos = src_valid.then(|| unsafe { modes[src_idx as usize].Anonymous.sourceMode.position });
                println!(
                    "🔎   path[{i}]: gdi={name:?} adapterId={:#x}:{:#x} sourceId={} active={} src_idx={src_idx} tgt_idx={tgt_idx} pos={pos:?} (modes.len()={})",
                    p.sourceInfo.adapterId.HighPart, p.sourceInfo.adapterId.LowPart, p.sourceInfo.id,
                    p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0, modes.len()
                );
            }
            return Err(format!("SetDisplayConfig failed to restore the saved display topology (error {status})"));
        }

        Ok(())
    }

    /// Queries the current default audio *render* (playback) endpoint via
    /// native Core Audio (`IMMDeviceEnumerator::GetDefaultAudioEndpoint` +
    /// `IMMDevice::GetId`), returning its device id string as a
    /// NUL-terminated UTF-16 buffer ready to pass straight to
    /// `SetDefaultAudioDevice`.
    ///
    /// Called by [`activate_for_stream`] *before* any display/audio mutation
    /// — this is the "real" host speaker endpoint, independent of whatever
    /// `audio.rs`'s `SinkGuard` later does (and independent of any
    /// default-device change Windows makes once the virtual display's own
    /// audio endpoint appears).
    ///
    /// Returns `None` (logged by the caller) if COM/Core Audio isn't
    /// available — restoration is then skipped rather than failing the whole
    /// activation.
    fn cache_default_audio_endpoint() -> Option<Vec<u16>> {
        unsafe {
            // Ignore the result: COM may already be initialized (possibly
            // with a different concurrency model) on this thread, which is
            // fine — CoCreateInstance still works either way.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            let id = device.GetId().ok()?;

            let id_string = id.to_string().ok();
            CoTaskMemFree(Some(id.0 as *const _));

            let id_string = id_string?;
            Some(id_string.encode_utf16().chain(std::iter::once(0)).collect())
        }
    }
}

impl Default for VirtualDisplay {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        if self.active {
            println!("🛡️  VirtualDisplay drop — restoring physical display topology before exit");
            if let Err(e) = self.deactivate_after_stream() {
                println!("⚠️  VirtualDisplay drop cleanup failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read-only sanity check against the real device tree — run with
    /// `cargo test virtual_display -- --nocapture` to confirm SetupAPI
    /// detection compiles and runs without admin rights. Does NOT install
    /// anything, so its result depends on whatever state this dev box is
    /// already in (informational, not pass/fail).
    #[test]
    fn detect_vdd_devnode() {
        let vd = VirtualDisplay::new();
        println!("install_dir:  {}", vd.install_dir.display());
        println!("is_installed: {}", vd.is_installed());
        println!("is_enabled:   {}", vd.is_enabled());
    }

    /// Recursively copies `src` into `dst`, creating directories as needed.
    /// Test-only helper to stage the local reference VDD package without
    /// relying on `download_release_package`'s (unverified) URL.
    fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());
            if src_path.is_dir() {
                copy_dir_recursive(&src_path, &dst_path)?;
            } else {
                std::fs::copy(&src_path, &dst_path)?;
            }
        }
        Ok(())
    }

    /// LIVE driver install round-trip. Stages the local reference package
    /// (`C:\VDD.Control.25.7.23`) into `install_dir` so
    /// `download_release_package` is skipped, then calls the real
    /// `ensure_installed()`. This WILL pop a UAC prompt and, if approved,
    /// installs the MttVDD driver/devnode on this machine.
    ///
    /// Run explicitly: `cargo test virtual_display::tests::install_vdd_live
    /// -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore = "performs a real elevated driver install — run explicitly"]
    fn install_vdd_live() {
        let mut vd = VirtualDisplay::new();

        let src = Path::new(r"C:\VDD.Control.25.7.23");
        assert!(src.exists(), "local reference package missing: {}", src.display());

        println!("Staging {} -> {}", src.display(), vd.install_dir.display());
        copy_dir_recursive(src, &vd.install_dir).expect("stage local VDD package");

        println!("install_dir:        {}", vd.install_dir.display());
        println!("is_installed before: {}", vd.is_installed());

        let result = vd.ensure_installed();
        println!("ensure_installed() -> {:?}", result);

        println!("is_installed after:  {}", vd.is_installed());
        println!("is_enabled after:    {}", vd.is_enabled());

        assert!(result.is_ok(), "ensure_installed failed: {:?}", result);
        assert!(vd.is_installed(), "Root\\MttVDD not present after install");
    }

    /// LIVE enable/disable round-trip against the `Root\MttVDD` devnode.
    /// Requires the driver to already be installed (run `install_vdd_live`
    /// first). Disables the devnode, confirms `is_enabled() == false`, then
    /// re-enables it and confirms `is_enabled() == true`. May pop a UAC
    /// prompt per toggle if Nova isn't already running elevated.
    ///
    /// Run explicitly: `cargo test virtual_display::tests::toggle_vdd_live
    /// -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore = "performs a real elevated devnode enable/disable — run explicitly"]
    fn toggle_vdd_live() {
        let mut vd = VirtualDisplay::new();
        assert!(vd.is_installed(), "Root\\MttVDD not installed — run install_vdd_live first");

        println!("is_enabled before: {}", vd.is_enabled());

        let disable_result = vd.set_enabled(false);
        println!("set_enabled(false) -> {:?}", disable_result);
        println!("is_enabled after disable: {}", vd.is_enabled());
        assert!(disable_result.is_ok(), "disable failed: {:?}", disable_result);
        assert!(!vd.is_enabled(), "devnode still enabled after set_enabled(false)");

        let enable_result = vd.set_enabled(true);
        println!("set_enabled(true) -> {:?}", enable_result);
        println!("is_enabled after enable: {}", vd.is_enabled());
        assert!(enable_result.is_ok(), "enable failed: {:?}", enable_result);
        assert!(vd.is_enabled(), "devnode still disabled after set_enabled(true)");
    }

    /// `patch_vdd_settings_xml` against a copy of the real bundled template
    /// (`Dependencies\vdd_settings.xml`, which ships with `30Hz`-only entries
    /// for the standard resolutions and a `<global>` list including 60Hz):
    ///   - 1920x1080@60 — resolution already present (@30), 60 already in
    ///     `<global>` → expect no change.
    ///   - 3440x1440@100 — neither present → expect both inserted, and the
    ///     result re-patches cleanly (idempotent on a second pass).
    #[test]
    fn patch_vdd_settings_xml_fixture() {
        let dir = std::env::temp_dir().join(format!("nova_vdd_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("vdd_settings.xml");

        let template = std::path::Path::new(r"C:\VDD.Control.25.7.23\Dependencies\vdd_settings.xml");
        assert!(template.exists(), "reference template missing: {}", template.display());
        std::fs::copy(template, &path).expect("copy template");

        // Already present (resolution @30 exists, 60 already a global rate)
        // -> no change.
        let changed = VirtualDisplay::patch_vdd_settings_xml(&path, 1920, 1080, 60).expect("patch (no-op case)");
        assert!(!changed, "1920x1080@60 should already be satisfied by the template");

        // Neither the (width,height) nor the refresh rate exist yet -> both
        // get appended.
        let changed = VirtualDisplay::patch_vdd_settings_xml(&path, 3440, 1440, 100).expect("patch (insert case)");
        assert!(changed, "3440x1440@100 should be newly inserted");

        let xml = std::fs::read_to_string(&path).expect("read patched xml");
        assert!(VirtualDisplay::xml_has_resolution(&xml, 3440, 1440), "3440x1440 resolution missing after patch");
        assert!(VirtualDisplay::xml_has_global_refresh_rate(&xml, 100), "100Hz missing from <global> after patch");
        println!("patched xml:\n{xml}");

        // Re-running with the same target is now a no-op.
        let changed = VirtualDisplay::patch_vdd_settings_xml(&path, 3440, 1440, 100).expect("patch (idempotent case)");
        assert!(!changed, "second patch with the same mode should be a no-op");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// LIVE `configure_mode` round-trip: requires the driver to already be
    /// installed (run `install_vdd_live` first). Patches the bundled
    /// `vdd_settings.xml` for 2560x1440@120 and points `VDDPATH` at it,
    /// then re-reads both back to confirm. May pop a UAC prompt if Nova
    /// isn't already running elevated and `VDDPATH` isn't already set
    /// correctly.
    ///
    /// Run explicitly: `cargo test virtual_display::tests::configure_mode_live
    /// -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore = "may pop a UAC prompt to set HKLM\\...\\VDDPATH — run explicitly"]
    fn configure_mode_live() {
        let vd = VirtualDisplay::new();
        assert!(vd.is_installed(), "Root\\MttVDD not installed — run install_vdd_live first");

        let result = vd.configure_mode(2560, 1440, 120);
        println!("configure_mode(2560, 1440, 120) -> {:?}", result);
        assert!(result.is_ok(), "configure_mode failed: {:?}", result);

        let settings_path = vd.vdd_settings_path();
        let xml = std::fs::read_to_string(&settings_path).expect("read settings after configure_mode");
        assert!(VirtualDisplay::xml_has_resolution(&xml, 2560, 1440), "2560x1440 missing from {}", settings_path.display());
        assert!(VirtualDisplay::xml_has_global_refresh_rate(&xml, 120), "120Hz missing from <global> in {}", settings_path.display());

        let registry_dir = VirtualDisplay::read_vddpath_registry();
        println!("VDDPATH registry: {:?}", registry_dir);
        assert_eq!(registry_dir.as_deref(), settings_path.parent(), "VDDPATH should point at the settings directory");
    }

    /// DIAGNOSTIC: dumps every mode `EnumDisplaySettingsW` reports for the
    /// live virtual display, to debug why `ChangeDisplaySettingsExW` rejects
    /// a given width/height/refresh combination.
    ///
    /// Run explicitly: `cargo test virtual_display::tests::enum_modes_diagnostic
    /// -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore = "diagnostic only — run explicitly"]
    fn enum_modes_diagnostic() {
        let device_name = VirtualDisplay::find_virtual_display_device_name().expect("virtual display not found in GDI enumeration");
        println!("virtual display device name: {device_name}");

        let name_w: Vec<u16> = device_name.encode_utf16().chain(std::iter::once(0)).collect();

        let mut current = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
        unsafe { EnumDisplaySettingsW(PCWSTR(name_w.as_ptr()), ENUM_CURRENT_SETTINGS, &mut current) };
        println!(
            "current: {}x{}@{}Hz bpp={} fields={:?}",
            current.dmPelsWidth, current.dmPelsHeight, current.dmDisplayFrequency, current.dmBitsPerPel, current.dmFields
        );

        let mut i = 0u32;
        loop {
            let mut mode = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
            let ok = unsafe { EnumDisplaySettingsW(PCWSTR(name_w.as_ptr()), windows::Win32::Graphics::Gdi::ENUM_DISPLAY_SETTINGS_MODE(i), &mut mode) };
            if !ok.as_bool() {
                break;
            }
            println!(
                "mode {i}: {}x{}@{}Hz bpp={}",
                mode.dmPelsWidth, mode.dmPelsHeight, mode.dmDisplayFrequency, mode.dmBitsPerPel
            );
            i += 1;
        }
        println!("total modes: {i}");
    }

    /// DIAGNOSTIC: issues a no-op `CDS_SET_PRIMARY` call on the display
    /// that's *already* primary (position unchanged at (0,0)) to isolate
    /// whether `ChangeDisplaySettingsExW`/`CDS_SET_PRIMARY`/`CDS_NORESET`
    /// works at all on this system, independent of the virtual display.
    ///
    /// Run explicitly: `cargo test virtual_display::tests::set_primary_noop_diagnostic
    /// -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore = "diagnostic only — run explicitly"]
    fn set_primary_noop_diagnostic() {
        let snapshot = VirtualDisplay::snapshot_current_primary().expect("snapshot current primary");
        println!(
            "current primary: {} ({}x{}@{}Hz at {:?})",
            snapshot.device_name, snapshot.width, snapshot.height, snapshot.refresh_hz, snapshot.position
        );
        assert_eq!(snapshot.position, (0, 0), "current primary should already be at (0,0)");

        let name_w: Vec<u16> = snapshot.device_name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut mode = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
        unsafe { EnumDisplaySettingsW(PCWSTR(name_w.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode) };

        mode.dmFields = DM_POSITION;
        mode.Anonymous1.Anonymous2.dmPosition = POINTL { x: 0, y: 0 };

        let result = unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(name_w.as_ptr()),
                Some(&mode),
                HWND(std::ptr::null_mut()),
                CDS_UPDATEREGISTRY | CDS_SET_PRIMARY | CDS_NORESET,
                None,
            )
        };
        println!("no-op CDS_SET_PRIMARY on {} -> DISP_CHANGE {}", snapshot.device_name, result.0);

        let apply = unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), None, HWND(std::ptr::null_mut()), CDS_TYPE(0), None) };
        println!("apply -> DISP_CHANGE {}", apply.0);

        // Same no-op, but without CDS_NORESET (applies immediately, no
        // separate apply call needed) — isolates whether CDS_NORESET is the
        // problem.
        let result2 = unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(name_w.as_ptr()),
                Some(&mode),
                HWND(std::ptr::null_mut()),
                CDS_UPDATEREGISTRY | CDS_SET_PRIMARY,
                None,
            )
        };
        println!("no-op CDS_SET_PRIMARY (no NORESET) on {} -> DISP_CHANGE {}", snapshot.device_name, result2.0);
    }

    /// DIAGNOSTIC, read-only: dumps the active CCD topology
    /// (`QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS)`), resolving each source's
    /// GDI device name and current position, and cross-checks that the
    /// source at `(0, 0)` matches [`snapshot_current_primary`]'s GDI-reported
    /// primary. Makes no `SetDisplayConfig` calls — safe to run anytime,
    /// including remotely.
    ///
    /// Run explicitly: `cargo test virtual_display::tests::query_ccd_topology_diagnostic
    /// -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore = "diagnostic only — run explicitly"]
    fn query_ccd_topology_diagnostic() {
        let (paths, modes) = VirtualDisplay::query_active_topology().expect("query active CCD topology");
        println!("active paths: {}", paths.len());

        let mut origin_name: Option<String> = None;
        for (i, path) in paths.iter().enumerate() {
            let idx = unsafe { path.sourceInfo.Anonymous.modeInfoIdx } as usize;
            if idx >= modes.len() || modes[idx].infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                println!("path {i}: source modeInfoIdx {idx} out of range / not a source mode — skipping");
                continue;
            }
            let name = VirtualDisplay::gdi_device_name_for_source(path.sourceInfo.adapterId, path.sourceInfo.id);
            let mode = unsafe { modes[idx].Anonymous.sourceMode };
            println!(
                "path {i}: gdi={:?} adapterId={:?} sourceId={} pos=({},{}) size={}x{}",
                name, path.sourceInfo.adapterId, path.sourceInfo.id, mode.position.x, mode.position.y, mode.width, mode.height
            );
            if mode.position.x == 0 && mode.position.y == 0 {
                origin_name = name;
            }
        }

        let gdi_primary = VirtualDisplay::snapshot_current_primary().expect("snapshot current primary");
        println!("GDI-reported primary: {}", gdi_primary.device_name);
        println!("CCD source at (0,0):  {:?}", origin_name);
        assert_eq!(origin_name.as_deref(), Some(gdi_primary.device_name.as_str()), "CCD (0,0) source should match the GDI primary");
    }

    /// LIVE end-to-end `activate_for_stream`/`deactivate_after_stream` round
    /// trip: requires the driver to already be installed (run
    /// `install_vdd_live` first). Activates a 1920x1080@60 virtual desktop
    /// session, confirms the virtual monitor becomes the GDI primary at the
    /// target mode, then deactivates and confirms the original primary is
    /// restored. Swaps the live desktop primary display twice — expect the
    /// screen to flicker/rearrange during this test.
    ///
    /// Run explicitly: `cargo test virtual_display::tests::activate_deactivate_stream_live
    /// -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore = "swaps the live desktop primary display — run explicitly"]
    fn activate_deactivate_stream_live() {
        let mut vd = VirtualDisplay::new();
        assert!(vd.is_installed(), "Root\\MttVDD not installed — run install_vdd_live first");

        let original_primary = VirtualDisplay::snapshot_current_primary().expect("snapshot original primary");
        println!(
            "original primary: {} ({}x{}@{}Hz at {:?})",
            original_primary.device_name, original_primary.width, original_primary.height, original_primary.refresh_hz, original_primary.position
        );

        let activate_result = vd.activate_for_stream(1920, 1080, 60);
        println!("activate_for_stream(1920, 1080, 60) -> {:?}", activate_result);
        assert!(activate_result.is_ok(), "activate_for_stream failed: {:?}", activate_result);
        assert!(vd.active, "vd.active should be true after activate_for_stream");

        let virtual_device = vd.active_device_name().expect("active_device_name set after activate").to_string();
        println!("virtual device is now primary: {virtual_device}");

        let new_primary = VirtualDisplay::snapshot_current_primary().expect("snapshot new primary");
        assert_eq!(new_primary.device_name, virtual_device, "virtual display should be the new GDI primary");
        assert_eq!((new_primary.width, new_primary.height, new_primary.refresh_hz), (1920, 1080, 60), "virtual display should be at the requested mode");

        let deactivate_result = vd.deactivate_after_stream();
        println!("deactivate_after_stream() -> {:?}", deactivate_result);
        assert!(deactivate_result.is_ok(), "deactivate_after_stream failed: {:?}", deactivate_result);
        assert!(!vd.active, "vd.active should be false after deactivate_after_stream");
        assert!(vd.active_device_name().is_none(), "active_device_name should be cleared after deactivate_after_stream");

        let restored_primary = VirtualDisplay::snapshot_current_primary().expect("snapshot restored primary");
        assert_eq!(restored_primary.device_name, original_primary.device_name, "original display should be primary again");
    }
}
