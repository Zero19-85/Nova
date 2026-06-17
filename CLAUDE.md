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

## Current Phase: Phase 6 (HDR10 end-to-end)
Phase 5 (VDD headless orchestration + dynamic resolution negotiation) is complete and confirmed over live Xbox + Android Moonlight sessions. Working end-to-end:
- Pairing (RSA/AES-ECB). **Critical:** `plaincert` must hex-encode the **PEM** bytes (not DER) — `PEM_read_bio_X509` in Moonlight requires PEM text, not binary DER.
- RTSP handshake (port 48010), ENet control (UDP 47999), H.264 RTP + RS-FEC (UDP 47998), WASAPI→Opus audio (UDP 48000), mouse/keyboard/gamepad input, cursor compositing.
- **Virtual Display Driver (App 5):** boots dormant (0×0 via `isolate_virtual_display_at_boot`). On stream: `SetDisplayConfig(SDC_TOPOLOGY_EXTEND)` re-activates VDD from dormant, then `ChangeDisplaySettingsExW` (force_resolution) snaps to client-negotiated resolution. All 11 resolutions (720p/1080p/1440p/4K × 30/60/120Hz) are pre-seeded in `vdd_settings.xml` at boot so `force_resolution` always finds the requested mode. Encoder/SPS matches the VDD resolution exactly.
- **Known limit (2026-06-17):** Xbox Moonlight 1.18.0 reports `x-nv-clientSupportHevc:0` and its H.264 decoder crashes at 4K@120fps (exceeds H.264 Level 5.2). Use 1080p@60fps or 1080p@120fps for stable streams. True 4K@120fps requires HEVC.

**Phase 6 next steps:**
1. Confirm stable 1080p stream on Xbox (next test — resolution within H.264 limits).
2. HDR10: Rust side fully wired (`enc.config.is_hdr`, `IsHdrSupported=1` for App 5). Shim fully implemented (HEVC Main10, P010, BT.2020 PQ VP, MDCV/CLL SEI). Blocked on: Moonlight client sending `videoFormat=0x102` + VDD/display in HDR mode for DXGI R16G16B16A16_FLOAT frames.
3. AV1: advertised (ServerCodecModeSupport bit 256), shim implemented — not end-to-end tested.
See memory/project_nova_state.md for full history.