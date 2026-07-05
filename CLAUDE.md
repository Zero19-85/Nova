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

## Current Phase: Phase 13 — RTP frameIndex Fix + Zombie-Proof /resume (2026-07-03)

Phase 13 fixes (a) the "black screen → ~10 s → Moonlight says reduce your bitrate" failure that became 100% reproducible with the release build on a clean network — **confirmed fixed, streaming works** — and (b) /resume kicking the client back to the app list when Moonlight was quit without disconnecting (Xbox behavior).

### Phase 13.1 — Install & driver preflight (2026-07-05):

**Installer elevation fix (`nova.iss`):**
- **Root cause of "auto-runs without admin":** the final "Launch Nova now" `[Run]` entry used `postinstall` without `runascurrentuser` — Inno Setup deliberately runs postinstall entries as the ORIGINAL unelevated user. Unelevated, the VDD devnode enable (SetupAPI `DICS_ENABLE`) and HDR10 Advanced Color switching fail silently → no virtual monitor, no HDR, black stream. The HDR10-on-VDD auto-enable itself was already implemented (pre-activation + connect-time `set_active_display_hdr(true)` when the client's ANNOUNCE confirms `dynamicRangeMode=1`) — it was the missing elevation that broke it on installed copies.
- **Fix:** `runascurrentuser` added to that entry — Nova now inherits the installer's interactive admin token, matching the elevation the `NovaServerBoot` task provides at every logon. (The manifest embedded via build.rs already declares `requireAdministrator`; RT_MANIFEST is compiled into the exe by rc.exe, so manual launches also elevate.)

**Elevation guard (`src/lib.rs`):**
- Startup preflight logs "🛡️ Elevated token confirmed" or a loud ❌ + on-screen MessageBox (background thread, non-blocking) when running unelevated — an unelevated start can otherwise only fail silently with a black screen. Uses `IsUserAnAdmin()` (Win32_UI_Shell, already a dependency).

**ViGEmBus (virtual Xbox 360 controller) preflight (`src/input.rs`, `src/lib.rs`):**
- `input::check_vigem_driver_at_startup()` — background thread probes `vigem_client::Client::connect()`. If the driver is missing: Yes/No MessageBox offering to download + run the official installer (pinned `ViGEmBus_1.22.0_x64_x86_arm64.exe` from nefarius/ViGEmBus GitHub releases, via the same PowerShell `Invoke-WebRequest` pattern as the RetroArch bootstrap). Download failure falls back to opening the releases page in the browser.
- Declining writes `vigem_install_declined.flag` next to the exe so the logon-autostart doesn't nag every boot (delete the flag to be asked again). The missing driver is still logged each start.
- `GamepadManager` connects per-session, so a mid-run install works on the next stream without restarting Nova.

### Phase 13 changes (2026-07-03):

**Zombie-proof /resume (`src/control.rs`, `src/pairing.rs`, `src/rtsp.rs`):**
- **Symptom:** quit Moonlight on Xbox mid-stream (no ENet disconnect is ever sent), reopen, tap Resume on app 5 → full RTSP handshake succeeds but the client waits on a dead session and bails back to the app list after ~7 s. Quit-app + relaunch worked; resume never did.
- **Root cause (two layers, from the 7/3 log):**
  1. The old session's ENet control peer lingers as a zombie until its 10–30 s timeout, which lands right after the /resume PLAY; `handle_event`'s Disconnect arm indiscriminately set `streaming_active=false`, tearing down the freshly resumed session. With `peer_limit: 1` the new control connection also couldn't even land until the zombie died.
  2. Deeper: /resume never restarted the session state machine at all — lib.rs's session-start block is gated on `!client_connected`, which was still true from the zombie session, so the new rikey/codec/audio were never applied ("Moonlight connected" never fired).
- **Fix:**
  - `ClientInfo.session_generation: u64` — bumped by every /launch **and** /resume (pairing.rs).
  - /launch and /resume now both arm the session with `streaming_active=false` (until PLAY) and reset `cancelled`/`hdr_mode_sent`/`dynamic_range_mode`/`bit_stream_format`. The capture loop therefore suspends a still-connected zombie session immediately and latches the new session cleanly at PLAY — new rikey, codec renegotiation, audio restart all run. Only /launch resets `activated` (resume reattaches to the live VDD with no topology flicker).
  - control.rs: `peer_limit: 2` so the resume's control connection lands instantly beside the zombie; every Connect stamps the peer with the current session generation and evicts all other peers via `Peer::reset()` (immediate slot free, no Disconnect event); the Disconnect arm ignores any peer whose stamp ≠ current generation ("stale peer — ignoring") — only the live session's peer can end the session.
- Also fixes the latent launch-over-zombie bug where pre-activation was skipped because `streaming_active` was still true from the dead session.

**RTP frameIndex must start at 1 (`src/rtp.rs`):**
- **Symptom:** full handshake succeeds, HEVC frames flow at the negotiated bitrate, but the client renders nothing, sends zero loss-stats and zero IDR re-requests, and terminates after ~10 s with `ML_ERROR_NO_VIDEO_FRAME` ("Your network connection isn't performing well. Reduce your video bitrate…").
- **Root cause:** `RtpSender.frame_index` started at 0. moonlight-common-c (`VideoDepacketizer.c`) initializes `nextFrameNumber = 1` and discards any packet with `isBefore32(frameIndex, nextFrameNumber)` — so Nova's session-start forced IDR (frame 0) was **always** discarded by every Moonlight client. Subsequent P-frames are dropped ("Waiting for IDR frame"), and the client only calls `LiRequestIdrFrame()` when `waitingForNextSuccessfulFrame` is also set — which requires a mid-frame packet loss. On a loss-free link the recovery never fires → permanent black screen.
- **Why it ever "worked":** every previously working session (incl. the Phase 12 validation on 7/2) started only because early WiFi packet loss tripped the client's recovery IDR request (visible in the 7/2 debug log as a second "client requested IDR frame" ~350 ms in). The slower debug-build pacing made loss likely; the release build's clean delivery removed the loss and exposed the bug deterministically.
- **Fix:** `frame_index` starts at 1 in `RtpSender::new()` and `reset()` — Sunshine parity (`video.cpp: int frame_nr = 1`). Regression test `first_frame_carries_frame_index_1_and_reset_restarts_at_1` locks the wire format (first frame = index 1, restarts at 1 after session reset).
- Also fixed: `#[cfg(test)]` GDI import list was missing `CDS_SET_PRIMARY` + `DM_POSITION` — `cargo test` had been broken since the Phase 12 import cleanup.
- **Status: CONFIRMED WORKING 2026-07-03** — user reports streaming works perfectly (Xbox 4K@120 H264/SDR and Android 720p HEVC sessions in the log).

## Phase 12 complete — IddCx CCD-Native VDD Resolution Fix (2026-07-02)

All previous phases (1–11) confirmed working. Phase 12 fixes VDD resolution not snapping to client-requested dimensions when using the MttVDD IddCx driver (resolution was stuck at native 2560×1440 regardless of Moonlight's requested mode).

### Phase 12 changes (2026-07-02):

**CCD-native VDD resolution (`src/virtual_display.rs`):**
- **Root cause:** MttVDD is an IddCx driver. `ChangeDisplaySettingsExW` always returns `DISP_CHANGE_FAILED (-1)` on IddCx; `EnumDisplaySettingsW(ENUM_CURRENT_SETTINGS)` always returns 0×0. All legacy GDI mode-set APIs are no-ops against IddCx.
- **`force_resolution` rewritten** to use `QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS)` + `SetDisplayConfig(SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_APPLY | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE)`. Modifies `DISPLAYCONFIG_SOURCE_MODE.width/height` and `DISPLAYCONFIG_PATH_TARGET_INFO.refreshRate` in-place before committing. Apollo-pattern refresh rate formula: `{Numerator: refresh_hz * 1000, Denominator: 1000}`.
- **`wait_for_display_resolution` rewritten** to poll `query_ccd_source_size` (CCD) instead of `EnumDisplaySettingsW` (broken for IddCx). Times out after 3 s, proceeds anyway with a warning.
- **`query_ccd_source_size` new helper:** scans `QDC_ONLY_ACTIVE_PATHS` for the named GDI device, matches the source mode entry by adapter LUID + source ID, returns `(width, height)`.
- **SDC_TOPOLOGY_EXTEND settle loop fixed:** was polling `EnumDisplaySettingsW` (always 0×0 on IddCx). Now polls `find_vdd_attached_to_desktop()` (`DISPLAY_DEVICE_ATTACHED_TO_DESKTOP` flag via `EnumDisplayDevicesW`) — set by DWM exactly when the device is live in the active topology.
- **GDI imports moved to `#[cfg(test)]`:** `ChangeDisplaySettingsExW`, `EnumDisplaySettingsW`, `CDS_*`, `DEVMODEW`, `ENUM_CURRENT_SETTINGS` — no longer used in production code path. Zero unused-import warnings.
- **Confirmed working** (2026-07-02): VDD snaps to 1280×720@60Hz, NVENC rebinds at 720p, HEVC stream at 7.5 Mbps client-negotiated, video loads in Moonlight without "reduce bitrate" error.

**Known remaining issues:**
- `ccd_isolate_vdd_and_restore_primary error 87` on disconnect (non-fatal; falls back to deactivate-only). IddCx adapter may not be found in `QDC_ALL_PATHS` with `DISPLAYCONFIG_PATH_ACTIVE` after stream ends.

---

All previous phases (1–10) confirmed working. Phase 11 delivers static-desktop Video Encode flatline (0% GPU utilisation matching Apollo/Sunshine), per-frame heap elimination in the RTP hot path, MMCSS audio scheduling, process power-throttling exemption, DSCP EF socket tagging, dynamic HDR luminance config, and thin-LTO binary hardening.

### Phase 11 changes (2026-06-25):

**Static-frame gate + IDR keep-alive (`src/lib.rs`):**
- `None =>` WGC branch no longer re-submits cached texture to NVENC every frame interval. NVENC hardware-idle on a static desktop → **0% Video Encode** in Task Manager, matching Apollo/Sunshine's flatline signature.
- `IDR_KEEPALIVE_INTERVAL = 1000 ms`: when the screen has been static, one forced IDR pulse per second keeps Moonlight's connection watchdog alive without engaging the encode engine.
- Gate: `client_connected && video_learned` — no encoding while no client is receiving.
- WGC `None` log spam reduced to first occurrence + every 300 frames (~5 s).

**`shim.cpp` hot-loop GetDesc() elimination:**
- `g_encWidth` / `g_encHeight` / `g_captureFmt` cached once in `InitColorConversion`, reset in `CleanupEncoder`. Eliminates per-frame COM `GetDesc()` round-trip from `EncodeFrame`. Removed the now-unused `vpSourceTexture` local.

**RTP shard-pool pre-allocation (`src/rtp.rs`):**
- `stream_buf: Vec<u8>` and `shard_pool: Vec<Vec<u8>>` added to `RtpSender` struct. Grow to session high-watermark and are reused/zeroed every frame. Eliminates ~36 `Vec::new()` + dealloc cycles per frame at 60–120 Hz.
- `send_packet` converted from method to free function to allow clean split-borrow access to `socket` and `shard_pool` simultaneously.
- Socket SO_SNDBUF raised from 4 MB to 8 MB (covers worst-case 4K IDR burst).

**MMCSS Pro Audio (`src/audio.rs`):**
- `AvSetMmThreadCharacteristicsW("Pro Audio")` registered on the WASAPI loopback capture thread immediately after `SetThreadPriority(TIME_CRITICAL)`. Matches Apollo/Sunshine. Elevates scheduler quantum and protects the audio thread from background preemption without REALTIME privilege.

**Process power-throttling exemption (`src/lib.rs`):**
- `SetProcessInformation(ProcessPowerThrottling, {ControlMask=1, StateMask=0})` at startup. Disables Windows 11 Efficiency Mode for the nova-server process — prevents E-core scheduling and CPU power-capping during active streaming.

**DSCP EF socket tagging (`src/rtp.rs`, `src/lib.rs`):**
- `socket2::set_tos(0xB8)` (DSCP EF = 101110 00, Expedited Forwarding) applied to both the video RTP UDP socket (port 47998) and the audio UDP socket (port 48000). Best-effort prioritisation honoured by DSCP-aware managed switches and Windows QoS Group Policy rules.

**Dynamic HDR luminance from `nova.toml` (`src/config.rs`, `src/encoder.rs`, `shim/shim.cpp`):**
- New `[hdr]` table in `nova.toml`: `max_luminance_nits` (default 1000), `max_cll_nits` (default 1000), `max_fall_nits` (default 400). BT.2020 primaries are standard constants; only luminance varies per panel.
- `encoder::set_hdr_metadata()` → `SetHdrMetadata()` FFI → `BuildHdrMetadata()` uses globals. Call injected in `lib.rs` immediately after `NovaConfig::load()`, before the first `Encoder::new()`.
- Operators can now tune HDR SEI to match their TV's actual spec (HDR600 / HDR1000 / HDR2000).

**Cargo release profile (`Cargo.toml`):**
- `[profile.release]`: `lto = "thin"`, `codegen-units = 1`, `strip = "symbols"`. Thin LTO gives ~90% of fat-LTO runtime benefit with ~10% of the link-time cost. Binary: **7.76 MB** exe + **0.08 MB** DLL.

---

### Working end-to-end (confirmed):
- Pairing (RSA/AES-ECB). **Critical:** `plaincert` must hex-encode the **PEM** bytes (not DER).
- RTSP handshake (port 48010), ENet control (UDP 47999), H.264 RTP + RS-FEC (UDP 47998), WASAPI→Opus audio (UDP 48000), mouse/keyboard/gamepad input, cursor compositing.
- **Universal VDD (all apps):** every Moonlight app routes through the Virtual Display Driver. Controlled by `nova.toml → headless_for_all_apps` (default `true`). Set `false` to restrict headless mode to App 5 only.
- **VDD hardware-disabled at boot (Phase 10):** `DICS_DISABLE` via SetupAPI leaves the devnode `CM_PROB_DISABLED` — invisible to DXGI, CCD, and PnP. Cannot steal primary on a graphics-stack crash or Safe Mode reboot. `activate_for_stream` calls `DICS_ENABLE` on client connect; `deactivate_after_stream` calls `DICS_DISABLE` on disconnect. `ensure_enabled_at_boot` cycles the devnode once to flush `vdd_settings.xml`, then disables it. CCD guard (`ccd_deactivate_vdd_path`) fires immediately after the devnode appears in GDI to prevent arrival-order primary hijack before `set_primary_display` runs.
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

**Installer — `nova.iss` (Inno Setup) + `nova-server.exe --install`:**
- `nova.iss` at project root. Bundles `nova-server.exe`, `nova_shim.dll`, and the full `VirtualDisplayDriver\` package.
- **`[Run]` step 1:** `devcon.exe install MttVDD.inf Root\MttVDD` — runs under the installer's live admin token. `WorkingDir` set to the INF directory so Windows PnP resolves `MttVDD.dll` and `mttvdd.cat`.
- **`[Run]` step 2:** `nova-server.exe --install` — registers the **`NovaServerBoot`** scheduled task via `schtasks /create /xml` with `<LogonType>InteractiveToken</LogonType>` + `<RunLevel>HighestAvailable</RunLevel>` + 5-second startup delay. Task runs in Session 1+ (never Session 0). Migrates/removes legacy task names. All child processes use `CREATE_NO_WINDOW`.
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

### Phase 10 fixes (2026-06-25):

**VDD On-Demand Lifecycle:**
- `ensure_enabled_at_boot`: cycles devnode to flush XML, calls `isolate_virtual_display_at_boot` (CCD DB consistent), then `DICS_DISABLE` — fully hardware-dormant. Returns `None` so WGC capturer binds physical primary.
- `activate_for_stream`: `DICS_ENABLE` before `wait_for_virtual_display_device_name`; immediate `ccd_deactivate_vdd_path` guard after GDI name acquired — prevents arrival-order primary steal; `SDC_TOPOLOGY_EXTEND` re-adds VDD as secondary only.
- `deactivate_after_stream`: `DICS_DISABLE` after `restore_topology` — devnode hardware-dormant between sessions.
- `VirtualDisplay::drop()` calls `deactivate_after_stream()` (existing) → also disables devnode on graceful shutdown.

**NVENC Quality Fixes (shim.cpp):**
- Cached `g_compositeSRV` — per-frame `CreateShaderResourceView` alloc eliminated from hot loop.
- `enableFillerDataInsertion=1` for H264+HEVC — prevents CBR QP oscillation on static frames ("pulsing text").
- `intraRefreshPeriod=fps, intraRefreshCnt=fps` — continuous rolling refresh; no off-gap between cycles.

**Congestion Control (encoder.rs / control.rs / lib.rs):**
- `STREAM_BITRATE_KBPS` + `CONGESTION_BITRATE_KBPS` atomics; `signal_congestion_reduction()` fires on `PT_LOSS_STATS` loss>0.
- Main loop: 2s cooldown, 20% cut on loss, 10%/5s ramp-back. `set_stream_bitrate_kbps()` tracks current CBR target.

**Thread Priority (lib.rs / audio.rs):**
- `SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL)` on capture/encode thread + both audio threads.

**WGC Stale-HMONITOR Fix (capture.rs):**
- `new_excluding()` outer retry re-resolves HMONITOR on each attempt — fixes E_INVALIDARG after VDD devnode topology cycle at boot.

**Crash-to-exit Hardening (lib.rs / nova-server.rs):**
- WGC and NVENC init failures now propagate via `?` instead of `.expect()` panic — `run()` returns `Err`, main exits with code 1.

### Phase 11 candidates:
- HDR10 colour verification on Android Moonlight (HEVC Main10 + TV)
- AV1 end-to-end test (advertised in `ServerCodecModeSupport`, shim implemented, not yet confirmed live)
- Xbox HEVC: currently reports `x-nv-clientSupportHevc:0` in v1.18.0 — investigate
- `audio.endpoint_override` in `nova.toml` wired into WASAPI pipeline (`audio.rs`)
- Monitor rename visible in Display Settings via monitor child-devnode (`CM_Set_DevNode_Property`)

See `memory/project_nova_state.md` for full session history.
