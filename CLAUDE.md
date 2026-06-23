# Nova Project Documentation & Instructions

## Project Scope
Nova is an ultra-low footprint, native Rust game-streaming host.
**Goal:** Flawlessly mimic GeForce Experience so Moonlight clients can connect.
**Architecture:**
- Async backend (`tokio` / `hyper`) for networking, mDNS, and pairing (HTTPS/XML).
- Native C++ FFI shim (`shim.cpp`) compiled to `nova_shim.dll` — zero-copy DXGI-to-NVENC hardware encoding.
- Architecture targets: High performance, ultra-low latency, and minimal portable `.exe` footprint.

## Developer Rules for Claude
1. **Always verify:** Before executing changes, audit the Rust `Cargo.toml` and `build.rs` to ensure no hallucinated static links are injected into the NVENC pipeline.
2. **Performance First:** Keep dependencies minimal. Prioritize zero-copy transfers (DXGI to NVENC).
3. **Workflow:** I (the user) will use this chat to coordinate tasks. You have access to workspace files via Claude Code. Use this to audit code and apply edits directly. If a build fails, analyze the compiler output, identify the specific missing library or header, and fix the `build.rs` or shim pathing.
4. **Consistency:** Ensure pairing logic (port 47989) and discovery (mDNS) stay compliant with the GameStream protocol.
5. **Build output:** `cargo build --release` produces two files that must be deployed together: `nova-server.exe` and `nova_shim.dll` (both in `target/release/`). The DLL is built by `build.rs` via `cl.exe` + `link.exe /DLL` and copied automatically.

## Current Phase: Phase 9 complete — Production Alpha (2026-06-23)

All previous phases (1–8) confirmed working. Phase 9 resolved every known deployment, performance, and reliability issue. Installer is production-ready.

---

### Working end-to-end (confirmed):
- Pairing (RSA/AES-ECB). **Critical:** `plaincert` must hex-encode the **PEM** bytes (not DER).
- RTSP handshake (port 48010), ENet control (UDP 47999), H.264 RTP + RS-FEC (UDP 47998), WASAPI→Opus audio (UDP 48000), mouse/keyboard/gamepad input, cursor compositing.
- **Universal VDD (all apps):** every Moonlight app routes through the Virtual Display Driver. Controlled by `nova.toml → headless_for_all_apps` (default `true`). Set `false` to restrict headless mode to App 5 only.
- **VDD boots dormant:** `SetDisplayConfig(SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_SAVE_TO_DATABASE)` removes the VDD path from the active CCD topology at boot. Physical monitors are never disturbed until a stream activates the VDD. The legacy `ChangeDisplaySettingsExW(0×0)` approach was replaced because MttVDD rejects 0×0 with `DISP_CHANGE_BADMODE`.
- **Dynamic monitor naming:** after `activate_for_stream`, `SetupDiSetDeviceRegistryPropertyW(SPDRP_FRIENDLYNAME)` renames the VDD devnode to the connected client's paired name (e.g. "Xbox"), visible in Device Manager and Display Settings.
- **HDR10 pipeline:** WGC FP16 scRGB → typed-RTV pixel shaders → P010 BT.2020 PQ → HEVC Main10 NVENC. SEI (MDCV type 137 + MaxCLL type 144) injected manually via `seiPayloadArray`. VUI: BT.2020 / SMPTE ST 2084 / NCL / full-range.
- **Known limit:** Xbox Moonlight 1.18.0 reports `x-nv-clientSupportHevc:0`; H.264 decoder crashes at 4K@120fps (Level 5.2). Use 1080p@60fps or 1080p@120fps on Xbox.

---

### Phase 9 fixes (2026-06-23):

**Graceful shutdown / Dead Man's Switch:**
- `impl Drop for VirtualDisplay` — on any exit path (Ctrl+C, OS shutdown, logoff, panic), `deactivate_after_stream()` fires automatically, restoring physical monitors before the process dies.
- Explicit ordered teardown at the end of `run()`: `enc.cleanup()` → `vd.deactivate_after_stream()` → function returns. D3D texture references are freed before the VDD's CCD path is torn down.
- OS signals already handled via `tokio::signal::windows::ctrl_close/ctrl_shutdown/ctrl_logoff`.

**VDD boot isolation fix:**
- **Root cause:** `isolate_virtual_display_at_boot` used `ChangeDisplaySettingsExW(0×0)` which MttVDD rejects with `DISP_CHANGE_BADMODE (-2)`. The early-return meant the VDD stayed active in the CCD topology, was saved to the database, and became a monitor on every reboot.
- **Fix:** `ccd_deactivate_vdd_path()` — queries `QDC_ALL_PATHS`, clears `DISPLAYCONFIG_PATH_ACTIVE` on the VDD's path entry, applies with `SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_SAVE_TO_DATABASE | SDC_ALLOW_CHANGES`. Mirrors the proven `deactivate_other_paths` pattern.

**Performance sweep (camera-pan stutter):**
- **REL mouse input:** `inject_mouse_move_rel` now sends raw wire deltas as `MOUSEEVENTF_MOVE` (no ABSOLUTE flag). The old path called `GetCursorPos` + 4×`GetSystemMetrics` per packet — 5 kernel transitions at 100–200 Hz during a camera pan. Games read relative input via `WM_INPUT / GetRawInputData`, not absolute cursor position.
- **RTP pacing:** `PACE_GAP` is now `300µs × (60 / fps)` — at 120fps it halves to 150µs, keeping total pacing overhead under 10% of the 8.33ms frame budget for large IDR frames.
- **Socket buffer:** UDP send buffer raised from 2MB to 4MB.
- **WGC miss sleep:** reduced from 2ms to 1ms in the `try_get_frame` None branch.

**`nova.toml` runtime config:**
- `serde` + `toml` crates added. `src/config.rs` defines `NovaConfig` with `StreamConfig`, `AudioConfig`, `NetworkConfig`.
- `nova.toml` auto-generated in the exe directory on first run with all defaults documented inline.
- Priority chain: **CLI arg → nova.toml → built-in default**. All `--width/--height/--bitrate/--codec/--fps/--fec` args now override config rather than hardcode defaults.
- Key fields: `bitrate_kbps`, `fps`, `codec`, `enable_hdr`, `headless_for_all_apps`, `fec_percentage`, `audio.endpoint_override`.
- `enable_hdr = true` bypasses `is_advanced_color_supported()` check — useful when HDRPlus is set in `vdd_settings.xml` but the CCD query is slow.

**Dynamic monitor naming:**
- `/launch` handler dumps all Moonlight parameters to `nova.log` (rikey redacted) for diagnostics.
- `uniqueid` from `/launch` is looked up in `nova_paired.json` to resolve the device's friendly name.
- `ClientInfo.device_name` carries the name through to `lib.rs`.
- `VirtualDisplay::rename_devnode(name)` calls `SetupDiSetDeviceRegistryPropertyW(SPDRP_FRIENDLYNAME)` after `activate_for_stream` succeeds.

**Headless mode toggle (`nova.toml`):**
- `headless_for_all_apps = true` (default) — all apps route through VDD.
- `headless_for_all_apps = false` — only App 5 activates headless; other apps stream the physical primary.
- `app_launcher::uses_virtual_display(app_id, headless_for_all)` implements the gate in both the pre-activation and connect-time paths.

**Installer — `nova.iss` (Inno Setup):**
- `nova.iss` at project root. Bundles `nova-server.exe`, `nova_shim.dll`, and the full `VirtualDisplayDriver\` package.
- **`[Run]` step 1:** `devcon.exe install MttVDD.inf Root\MttVDD` — runs under the installer's live admin token, no UAC child-process suppression. `WorkingDir` set to the INF directory so Windows PnP can resolve `MttVDD.dll` and `mttvdd.cat`.
- **`[Run]` step 2:** `nova-server.exe --install` — registers the ONLOGON scheduled task.
- **`[Run]` step 3:** `nova-server.exe` — launches Nova for this session.
- Build pre-requisite: copy `C:\VDD.Control.25.7.23\` → `<project root>\VirtualDisplayDriver\` before compiling the installer.
- Architecture-aware: x64 and ARM64 paths handled via `Check: IsARM64`.

**Dead code cleanup:**
- `UpdateCursorShape` / `UpdateCursorPosition` FFI declarations and Rust wrappers removed — superseded by WGC `SetIsCursorCaptureEnabled(true)` cursor compositing.
- `Direct3D11CaptureFrame` unused import removed from `capture.rs`.
- `CDS_NORESET` / `CDS_TYPE` moved to `#[cfg(test)]` (only used in `#[ignore]`d diagnostic tests).

---

### Deployment checklist:
```
target/release/nova-server.exe   ← main binary
target/release/nova_shim.dll     ← C++ encoder shim (must be alongside .exe)
```
- `nova.toml` is auto-generated on first run — no manual copy needed.
- `nova.log` is written to the exe directory — tail it for diagnostics.
- `nova_paired.json` persists across restarts — contains paired device names.

### Inno Setup build steps:
```powershell
cargo build --release
Copy-Item -Recurse "C:\VDD.Control.25.7.23" ".\VirtualDisplayDriver"
# Open nova.iss in Inno Setup Compiler → Compile
# Output: Output\NovaSetup-0.1.0.exe
```

---

### Tray UX (current state):
- Right-click context menu:
  - **Pair Device** — auto-opens two-field dialog on pairing request (triggered during `getservercert`); user also can pre-open via tray menu.
  - **Quit Nova** — graceful shutdown via `watch::Sender<bool>`.
- `global_pin: Arc<Mutex<(String, String)>>` — tuple of (PIN, device_name).
- `TrayCmd::OpenPairDialog` — new command that opens the dialog proactively from `getservercert`.

---

### Phase 10 candidates:
- HDR10 colour verification on Android Moonlight (HEVC Main10 + TV)
- AV1 end-to-end test (advertised in `ServerCodecModeSupport`, shim implemented, not yet confirmed live)
- Xbox HEVC: currently reports `x-nv-clientSupportHevc:0` in v1.18.0 — investigate
- `audio.endpoint_override` in `nova.toml` wired into WASAPI pipeline (`audio.rs`)
- Monitor rename visible in Display Settings via monitor child-devnode (`CM_Set_DevNode_Property`)

See `memory/project_nova_state.md` for full session history.
