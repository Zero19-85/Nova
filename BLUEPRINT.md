# 🚀 NOVA / ECHO: THE MASTER BLUEPRINT

**Target Ecosystem:** High-Performance, Ultra-Low Footprint Game-Streaming (GameStream/Moonlight)  
**Core Language:** 100% Native Rust Engine with bare-metal C++ Hardware Shims

## 🎯 1. The Core Vision: Breaking the Sunshine Paradigm

Current industry standards rely on bulky abstractions and heavy configurations.  
**Nova’s Vision:** A hyper-optimized, lightweight background service (**<15MB footprint**) executing **zero-copy memory transfers** entirely on the GPU. It runs effortlessly, leaves 99.9% of system resources available for games, and embeds advanced driver tasks into a single executable.

## 💎 2. Unified Project Architecture & Monetization Blueprint

**Open-Core Split:**

### 🔹 Nova Core (Free / Open Source)
*The absolute bare-metal high-speed performance framework.*
- Zero-Copy Pipelines: Native DXGI Desktop Duplication linked straight to hardware NVENC/AMF buffers
- Codec-Tiering Baseline: H.265 (default), AV1 (modern GPUs), H.264 (legacy)
- Token-Bucket Network Pacer: Eliminates wireless micro-stutter
- Full Controller Support: Native XInput / ViGEmBus virtual injection

### 🔸 Nova Pro / Donation Service Tier
*Enterprise features and specialized pipelines.*
- Windows Service Mode Integration: Runs at boot before user login (hidden)
- Native Echo Mic Passthrough: UMDF Virtual Audio Driver with zero helper apps
- Virtual Monitor / IDD Sandbox: Indirect Display Driver for headless rigs, HDR/10-bit support

## 🗺️ 3. The Master Execution Roadmap

### 🏁 Phase 3: The Pipeline Finish Line (Current Stage)
- **Milestone 3.1:** Bind acquire_frame pointer stream into the C++ EncodeFrame shim
- **Milestone 3.2:** Save raw video packets to `test.h264` for local verification in VLC
- **Milestone 3.3:** Basic RTSP network handling for Moonlight client pairing

### 🔊 Phase 4: Brainstorm & Refinement
1. Low-Latency WASAPI Capture (high-priority thread)
2. AV Sync Engine (hardware-timed alignment)
3. mDNS Zero-Config Discovery
4. Dynamic Bitrate Adjustment (UDP feedback)
5. Mouse Lock & Virtual Absolute Input
6. Auto-Game Presets (foreground window detection)
7. Headless Display Emulation (no HDMI dummies)

### 📦 Phase 5: The Single-Exe Blueprint
- Asset embedding via `include_bytes!`
- Linker optimizations (LTO, panic=abort, symbol stripping)
- Portable executable, no installer required

## 🔥 4. Community Most-Wanted Features
- Mouse Lock & Virtual Absolute Input Handling
- Headless Display Emulation (zero hardware dummies)
- Auto-Game Presets based on running processes
- Native microphone passthrough

---

**This is our living master document.** All development decisions should align with this blueprint.

**Next Tactical Step:** When ready, type **"Go for Phase 3 bitstream generation"** to begin writing the code that feeds live VRAM buffers into the hardware encoder.