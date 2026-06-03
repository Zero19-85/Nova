# 🚀 Nova Server — Ultra-Low Footprint Game Streaming Host

**Moonlight / GameStream compatible • Rust + C++ • <15MB single-exe target**

An ambitious next-generation replacement for Sunshine. Built for **maximum performance with minimum resource usage**.

---

### Why Nova?
- True **zero-copy** DXGI → NVENC/AMF GPU pipelines
- Extremely lightweight (<15MB executable goal)
- Leaves 99.9% of system resources for your games
- Native RTX 50-series support out of the box
- Open-Core model: Powerful free core + optional Pro/donation features
- Moonlight compatible (works with existing clients)

**Nova** = the host (server)  
**Echo** = companion client (coming later)

---

## 🎯 Master Blueprint

This project follows a clear **Master Pattern Blueprint** to become an unstoppable market disruptor in game streaming.

### Core Vision
A hyper-optimized, lightweight background service that runs effortlessly with zero-copy memory transfers entirely on the GPU.

### Open-Core Model

**Nova Core (Free / Open Source)**
- Zero-Copy Pipelines (DXGI Desktop Duplication → hardware encoder buffers)
- Intelligent Codec Tiering (H.265 baseline, AV1 on modern GPUs, H.264 fallback)
- Token-Bucket Network Pacer (eliminates wireless micro-stutter)
- Full Controller Support (XInput + ViGEmBus virtual injection)

**Nova Pro / Donation Tier**
- Windows Service Mode (runs at boot, pre-login, hidden)
- Native Echo Mic Passthrough (UMDF Virtual Audio Driver)
- Virtual Monitor / Indirect Display Driver (IDD) Sandbox (headless HDR/10-bit)
- Advanced orchestration & routing

---

## 🗺️ Master Roadmap

### Phase 3: The Pipeline Finish Line (Current Focus)
- Bind acquire_frame pointer stream into C++ EncodeFrame shim
- Generate test `.h264` file output (verifiable in VLC)
- Basic RTSP network handling for Moonlight client pairing

### Phase 4: Brainstorm & Refinement
- Low-Latency WASAPI Audio Loopback
- AV Sync Engine with hardware timing
- mDNS Zero-Config Discovery
- Dynamic Bitrate Adjustment via UDP feedback
- Mouse Lock & Virtual Absolute Input
- Auto-Game Presets (based on foreground window)
- Headless Display Emulation (no HDMI dummies needed)

### Phase 5: Single-Exe Release Engineering
- Asset embedding with `include_bytes!`
- Full LTO + panic=abort + symbol stripping
- Portable executable with no installer required

---

## 🔥 Community Most-Wanted Features
- Mouse lock & virtual absolute input handling
- Headless display emulation (zero hardware dummies)
- Auto game presets based on running titles
- Native microphone passthrough (Echo)

---

## 🛠️ Tech Stack
- **Core**: 100% Rust for safety and performance
- **Hardware Shims**: Thin C++ layer via `cc` builder (NVENC today, AMF/QuickSync tomorrow)
- **Capture**: DXGI Desktop Duplication
- **Encoding**: Direct hardware access (NVIDIA/AMD/Intel)

---

## Current Status (June 1, 2026)
**Phase 2 Complete**  
✅ DXGI Desktop Duplication working  
✅ Stable NVENC session on RTX 50-series via C++ shim  
✅ Hybrid Rust + C++ architecture locked in

**Next Step**: Phase 3 bitstream generation and zero-copy encoding loop.

---

## Quick Start (Alpha coming soon)

```bash
git clone https://github.com/Zero19-85/nova-server.git
cd nova-server
cargo run --release

Requirements: Windows 10/11 + Modern GPU (NVIDIA recommended)

Contributing & Donations
We welcome contributions! See BLUEPRINT.md for the full vision and open issues for tasks.
If you want to support faster development of Pro features (Windows Service, Virtual Audio, IDD), consider sponsoring the project on GitHub.

Star this repo if you want lower-latency, lighter game streaming! ⭐