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

## Current Phase: Phase 5 (HDR10 / Color Pipeline)
Phase 4 (Networking & Packetization) is complete and user-confirmed over live Moonlight sessions: RTSP control channel (port 48010), ENet control stream (UDP 47999), H.264 NAL units packaged into RTP with Reed-Solomon FEC, audio (WASAPI loopback -> Opus over RTP), input passthrough (mouse/keyboard/gamepad), cursor compositing, and virtual-display (App 5 "Virtual Desktop") activate/restore are all working end-to-end.
- **Next steps:** Implement HDR10 — NVENC HDR metadata / HEVC Main10 10-bit profile (encoder.rs, shim.cpp), HDR-capable DXGI capture format (capture.rs), and RTSP/SDP HDR signaling (rtsp.rs). See memory/project_nova_state.md ("Next up: HDR10") for history and standing conventions.