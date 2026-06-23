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

## Current Phase: Phase 8 complete — Stable Alpha (2026-06-23)

All previous phases (1–7) confirmed working. Phase 8 resolved every known deployment and reliability issue.

---

### Working end-to-end (confirmed):
- Pairing (RSA/AES-ECB). **Critical:** `plaincert` must hex-encode the **PEM** bytes (not DER).
- RTSP handshake (port 48010), ENet control (UDP 47999), H.264 RTP + RS-FEC (UDP 47998), WASAPI→Opus audio (UDP 48000), mouse/keyboard/gamepad input, cursor compositing.
- **Virtual Display Driver (App 5):** boots dormant. On stream: `SetDisplayConfig(SDC_TOPOLOGY_EXTEND)` + `ChangeDisplaySettingsExW` snaps to client-negotiated resolution. All 11 modes (720p/1080p/1440p/4K × 30/60/120Hz) pre-seeded in `vdd_settings.xml`.
- **HDR10 pipeline:** WGC FP16 scRGB → typed-RTV pixel shaders → P010 BT.2020 PQ → HEVC Main10 NVENC. SEI (MDCV type 137 + MaxCLL type 144) injected manually via `seiPayloadArray`. VUI: BT.2020 / SMPTE ST 2084 / NCL / full-range.
- **Known limit:** Xbox Moonlight 1.18.0 reports `x-nv-clientSupportHevc:0`; H.264 decoder crashes at 4K@120fps (Level 5.2). Use 1080p@60fps or 1080p@120fps on Xbox.

---

### Phase 8 fixes (2026-06-23):

**HDR One-Hit Wonder bug (root cause + teardown):**
- **Root cause:** `ClientInfo.hdr_mode_sent` was not reset in the `/launch` handler — the flag was carried over via `take().unwrap_or_default()`. On reconnect the control thread saw `hdr_mode_sent=true` and silently skipped the `0x010e` HDR mode packet, so the Xbox TV never switched to HDR10 mode (whitewash).
- **Fix:** `info.hdr_mode_sent = false` added to `/launch` handler in `pairing.rs`.
- **Scorched-earth teardown:** On every disconnect (cancel or suspend), `enc.cleanup()` is always called, `enc.config.is_hdr` reset to false, Windows Advanced Color disabled on the VDD, and a fresh SDR encoder rebuilt immediately. Prevents stale `g_isHdr` / RTV / VP state from the previous session leaking into the next. `CleanupEncoder` in `shim.cpp` also resets `g_isHdr`, `g_hdrMetadataReady`, `g_encoderCodec`, `g_encoderFps`.
- **Codec transparency:** `InitEncoder` logs `[NVENC] Initialized Codec: HEVC (H.265) - 10-bit HDR` (or SDR/H264/AV1 as appropriate) for every session.

**Pairing UX — Device Naming + No Timeout:**
- **Two-field dialog:** `prompt_for_pin_and_name()` in `tray.rs` shows two sequential PowerShell `InputBox` dialogs: PIN (step 1/2) then device name (step 2/2, e.g. "Xbox").
- **No client timeout:** Android Moonlight sets `READ_TIMEOUT=7000ms` on `clientchallenge` but `enableReadTimeout=false` on `getservercert`. The PIN dialog is now opened and polled during `getservercert` (unlimited wait). By the time `clientchallenge` arrives, `session.pin` is already set and Nova responds in < 50ms — well within the 7-second budget.
- **JSON storage:** `nova_paired.json` maps `uniqueid → { "name": "Xbox" }`. Functions: `load_paired_json`, `save_paired_json`, `persist_paired_client(id, name)`.
- **`global_pin` type:** `Arc<Mutex<(String, String)>>` (pin, device_name tuple).

**DLL deployment (build.rs):**
- `cc::Build` replaced with a direct `cl.exe` + `link.exe /DLL` pipeline in `build.rs`.
- `nova_shim.dll` built to `OUT_DIR`, then **copied to `target/release/`** automatically every build.
- `cargo:warning=DLL Path: ...` line shows the exact destination during build.
- Rust binary links against the import lib (`nova_shim.lib`) via `cargo:rustc-link-lib=dylib=nova_shim`.
- Both files must be deployed together: `nova-server.exe` + `nova_shim.dll`.
- Convenience script: `.\build.ps1` (release) or `.\build.ps1 -Debug`.

**Deployment: Task Scheduler (not Windows Service):**
- **Why not a service:** Windows Services run in Session 0. DXGI Desktop Duplication, D3D11 Video Processor, and Windows Graphics Capture all require the interactive session (Session 1+). In Session 0 they return `E_ACCESSDENIED`, producing "half-green / half-smeared" frames.
- **`--install`:** Runs `schtasks /create /tn "Nova Game Streaming" /sc ONLOGON /rl HIGHEST /f`. Also runs the **Ghost Protocol** (deletes stale `nova_shim.dll` from `System32` / `SysWOW64`) and cleans up any old `NovaServer` SCM service.
- **`--uninstall`:** Runs `schtasks /delete /tn "Nova Game Streaming" /f`, kills running instance, and cleans up old SCM service.
- **`windows-service` crate removed** from `Cargo.toml`. `Win32_System_Services` feature removed.
- **Inno Setup `[Run]` section:**
  ```ini
  [Run]
  Filename: "{app}\nova-server.exe"; Parameters: "--install"; Flags: runhidden waituntilterminated
  Filename: "{app}\nova-server.exe"; Flags: nowait runhidden

  [UninstallRun]
  Filename: "{app}\nova-server.exe"; Parameters: "--uninstall"; Flags: runhidden waituntilterminated
  Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM nova-server.exe"; Flags: runhidden waituntilterminated
  ```

**DPI Awareness (4K scaling fix):**
- **Bug:** Without DPI awareness, on a 4K/200% display Windows lied to `GetMonitorInfoW` and DXGI, reporting 1920×1080. WGC created a 1920×1080 frame pool; NVENC was sized 3840×2160. The frame filled only the top-left quarter of the encode buffer — producing exactly "top-right smear / bottom-half green".
- **Fix 1 (manifest):** `nova-server.manifest` now includes `<dpiAware>true/pm</dpiAware>` and `<dpiAwareness>PerMonitorV2</dpiAwareness>`. Applied before `main()` runs.
- **Fix 2 (runtime):** First line of `main()` calls `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)` as belt-and-suspenders.
- **Feature added:** `Win32_UI_HiDpi` in `Cargo.toml`.

**File Logger (`nova.log`):**
- `debug.rs` rewritten: opens `{exe_dir}\nova.log`, calls `SetStdHandle(STD_OUTPUT_HANDLE, file)` and `SetStdHandle(STD_ERROR_HANDLE, file)` → **all `println!`/`eprintln!` in the entire process redirect to the log file** with zero call-site changes.
- `InitShimLog(path)` exported from `shim.cpp`: opens same log file, `_dup2`s CRT fds 1+2, so all `ShimLog()` calls and any remaining `printf()` also land there.
- All 29 `printf()` calls in `shim.cpp` replaced with `ShimLog()`.
- `log_shim_dll_path()` logs which `nova_shim.dll` is loaded (path, size, modified date) and scans PATH + System32 for stale stray copies.
- `OpenNvEncSession` logs current Session ID — if `CurrentSession=0` the log immediately identifies the root cause.
- New Cargo.toml features: `Win32_Storage_FileSystem`, `Win32_System_Console`.

---

### Tray UX (current state):
- Right-click context menu:
  - **Pair Device** — auto-opens two-field dialog on pairing request (triggered during `getservercert`); user also can pre-open via tray menu.
  - **Quit Nova** — graceful shutdown via `watch::Sender<bool>`.
- `global_pin: Arc<Mutex<(String, String)>>` — tuple of (PIN, device_name).
- `TrayCmd::OpenPairDialog` — new command that opens the dialog proactively from `getservercert`.

---

### Phase 9 candidates:
- HDR10 colour verification on Android Moonlight (HEVC Main10 + TV) post 0x010e fix
- AV1 end-to-end test (advertised in `ServerCodecModeSupport`, shim implemented, not yet confirmed live)
- Xbox HEVC: currently reports `x-nv-clientSupportHevc:0` in v1.18.0 — investigate
- Inno Setup installer polish: auto-launch after install confirmation, version upgrade path

See `memory/project_nova_state.md` for full session history.
