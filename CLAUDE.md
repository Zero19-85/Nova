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

**Phase 7 candidates:**
- Verify HDR10 colours on Android Moonlight (HEVC Main10 + TV) after 0x010e fix
- AV1 end-to-end test (advertised, shim implemented, not yet confirmed live)
- Xbox HEVC support (currently reports `x-nv-clientSupportHevc:0` in v1.18.0)
See memory/project_nova_state.md for full history.