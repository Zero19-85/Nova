# Nova Project Documentation & Instructions

## Project Scope
Nova is an ultra-low footprint, native Rust game-streaming host. 
**Goal:** Flawlessly mimic GeForce Experience so Moonlight clients can connect. 
**Architecture:** - Async backend (`tokio` / `hyper`) for networking, mDNS, and pairing (HTTPS/XML).
- Native C++ FFI shim (`shim.cpp`) for zero-copy DXGI-to-NVENC hardware encoding.
- Architecture targets: High performance, ultra-low latency, and minimal (8-12 MB) portable `.exe` footprint.

## Developer Rules for Claude
1. **Always verify:** Before executing changes, audit the Rust `Cargo.toml` and `build.rs` to ensure no hallucinated static links are injected into the NVENC pipeline.
2. **Performance First:** Keep dependencies minimal. Prioritize zero-copy transfers (DXGI to NVENC).
3. **Workflow:** - I (the user) will use this chat to coordinate tasks. 
   - You have access to my workspace files via Claude Code. Use this to audit code and apply edits directly.
   - If a build fails, analyze the compiler output, identify the specific missing library or header, and fix the `build.rs` or shim pathing.
4. **Consistency:** Ensure pairing logic (port 47989) and discovery (mDNS) stay compliant with the GameStream protocol.

## Current Phase: Phase 6 complete — HDR10 pipeline (pending final test)
Phase 5 (VDD headless orchestration + dynamic resolution negotiation) is complete and confirmed over live Xbox + Android Moonlight sessions. Working end-to-end:
- Pairing (RSA/AES-ECB). **Critical:** `plaincert` must hex-encode the **PEM** bytes (not DER) — `PEM_read_bio_X509` in Moonlight requires PEM text, not binary DER.
- RTSP handshake (port 48010), ENet control (UDP 47999), H.264 RTP + RS-FEC (UDP 47998), WASAPI→Opus audio (UDP 48000), mouse/keyboard/gamepad input, cursor compositing.
- **Virtual Display Driver (App 5):** boots dormant (0×0 via `isolate_virtual_display_at_boot`). On stream: `SetDisplayConfig(SDC_TOPOLOGY_EXTEND)` re-activates VDD from dormant, then `ChangeDisplaySettingsExW` (force_resolution) snaps to client-negotiated resolution. All 11 resolutions (720p/1080p/1440p/4K × 30/60/120Hz) are pre-seeded in `vdd_settings.xml` at boot so `force_resolution` always finds the requested mode. Encoder/SPS matches the VDD resolution exactly.
- **Known limit (2026-06-17):** Xbox Moonlight 1.18.0 reports `x-nv-clientSupportHevc:0` and its H.264 decoder crashes at 4K@120fps (exceeds H.264 Level 5.2). Use 1080p@60fps or 1080p@120fps for stable streams. True 4K@120fps requires HEVC.

**Phase 6 HDR10 architecture (2026-06-22):**
- **Capture:** WGC FP16 scRGB → `g_compositeTex`
- **CS bridge:** `kHdrCsHlsl` — 2×2 per thread, scRGB→BT.2020→PQ→YCbCr 4:2:0 full-range, written to `g_hdrP010Tex` via D3D11.3 per-plane UAVs (`R16_UNORM`/`R16G16_UNORM`, `ID3D11Device3::CreateUnorderedAccessView1`)
- **NVENC input:** `CopyResource(g_nvencInputTex, g_hdrP010Tex)` — P010→P010, always valid
- **NVENC config:** `YUV420_10BIT`, HEVC Main10, VUI: BT.2020 / SMPTE ST 2084 / NCL / full-range
- **SEI:** Manual byte-packed MDCV (type 137) + MaxCLL (type 144) via `seiPayloadArray` on forced IDR frames — replicates FFmpeg/Apollo path (NVENC native `pMasteringDisplay` ignored by driver)
- **HDR mode signalling:** `0x010e` control packet (`SS_HDR_METADATA`, 33 bytes) sent on first `PT_PERIODIC_PING` — this is what triggers `RequestSetCurrentDisplayModeAsync(Eotf2084)` on the Xbox, physically switching the TV's HDMI port into HDR10 mode. **This was the root cause of the "whitewash": Nova was missing this packet entirely.**
- **D3D11 VP bypassed for HDR:** NVIDIA driver bug zeroes chroma on any P2020 colorspace declaration; CS→P010 direct path works correctly.

**Phase 7 — Native Windows UX Polish (2026-06-22, complete):**
Nova is now a fully headless, tray-resident background application. No terminal window is ever shown.

- **Executable icon:** `assets/Nova.ico` compiled into the `.exe` via `build.rs` + `rc.exe` (resource ID 1 RT_GROUP_ICON alongside the existing UAC manifest). Windows Explorer shows the exploding-star logo.
- **System tray (`tray-icon` crate):** A dedicated OS thread owns the tray icon. Right-click shows a two-item context menu built with `muda`:
  - **Pair Device** — opens a native Windows `InputBox` (PowerShell `-WindowStyle Hidden`, VB runtime) so the user can type the PIN shown on their Moonlight device. The PIN is written to `global_pin: Arc<Mutex<String>>`, which wakes the pairing `clientchallenge` polling loop.
  - **Quit Nova** — sends `true` on a `tokio::sync::watch` channel; the main capture-loop `select!` breaks and runs full teardown (audio restore, VDD deactivate).
- **Pairing UX:** Moonlight generates and displays the PIN on the client device (standard GameStream protocol). Nova shows a tooltip update `"Nova — Pairing Request"` and waits up to 5 minutes for the user to enter the PIN via the tray menu. **Do not** generate the PIN server-side — that breaks standard Moonlight clients.
- **Headless mode:** `#![windows_subsystem = "windows"]` in `src/bin/nova-server.rs`. No console window on double-click or SCM launch.
- **Windows Service:** `windows-service = "0.7"` crate. CLI flags:
  - `nova-server.exe --install` → registers `NovaServer` as an auto-start service via Win32 SCM (`CreateServiceW`).
  - `nova-server.exe --uninstall` → marks for deletion.
  - `nova-server.exe --run-service` → entry point the SCM calls; dispatches through `service_dispatcher::start`.
- **Graceful shutdown path:** Tray "Quit" → `watch::Sender<bool>` → `shutdown_rx.changed()` in capture-loop `select!` → break → `AudioStreamer::drop()` restores host audio device.
- **Key architecture decision:** The tray event loop (`MenuEvent::receiver().try_recv()` + `TrayIconEvent::receiver().try_recv()` + `PeekMessageW` pump) runs on a dedicated OS thread, not inside the tokio runtime. `global_pin` and `shutdown_tx` bridge the sync tray world and the async capture world.

**Phase 8 candidates:**
- Verify HDR10 colours on Android Moonlight (HEVC Main10 + TV) after 0x010e fix
- AV1 end-to-end test (advertised, shim implemented, not yet confirmed live)
- Xbox HEVC support (currently reports `x-nv-clientSupportHevc:0` in v1.18.0)
- `--install` console output (currently silent with `windows_subsystem = "windows"`; fix with `AttachConsole(ATTACH_PARENT_PROCESS)` or a `MessageBoxW` result dialog)
See memory/project_nova_state.md for full history.