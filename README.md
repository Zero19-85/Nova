
# Nova — Native Rust GameStream Host

A zero-copy, native Rust + C++ NVENC game-streaming host that speaks the Moonlight/GameStream protocol. Goal: replace Sunshine with a portable, <15 MB single executable.

---

## Current State

**Phase 5 complete (VDD orchestration + dynamic resolution) — streaming end-to-end on real hardware.**

| Layer | Status |
|---|---|
| Pairing (RSA/AES-ECB, PEM plaincert) | ✅ Working — Xbox & Android confirmed |
| RTSP handshake (OPTIONS/DESCRIBE/SETUP×3/ANNOUNCE/PLAY) | ✅ Working |
| H.264 video (NVENC CBR, infinite GOP, intra refresh) | ✅ Working |
| RTP packetizer + Reed-Solomon FEC (20% parity) | ✅ Working |
| ENet control stream (IDR requests, ping, disconnect) | ✅ Working |
| Audio (WASAPI loopback → Opus → RTP, AES-128-CBC) | ✅ Working |
| Input (mouse, keyboard, gamepad via ViGEmBus) | ✅ Working |
| Cursor compositing (alpha + XOR invert pass) | ✅ Working |
| Virtual Display Driver (App 5 — headless, any resolution) | ✅ Working |
| Dynamic resolution negotiation (VDD follows client request) | ✅ Working |
| HEVC Main10 / HDR10 scaffolding | 🔧 Wired, not end-to-end tested |
| AV1 | 🔧 Advertised, not tested |

---

## Architecture

```
Moonlight client
      │  HTTPS :47989/:47984  (pairing)
      │  RTSP  :48010         (session negotiation)
      │  ENet  :47999 UDP     (control — IDR, ping, input)
      │  RTP   :47998 UDP     (video frames + FEC)
      │  RTP   :48000 UDP     (Opus audio)
      ▼
┌─────────────────────────────────────────────────────┐
│  Nova  (nova-server.exe)                            │
│                                                     │
│  src/pairing.rs   — HTTP/HTTPS GameStream pairing   │
│  src/rtsp.rs      — RTSP session + SDP negotiation  │
│  src/control.rs   — ENet reliable-UDP control       │
│  src/rtp.rs       — RTP packetizer + RS-FEC         │
│  src/capture.rs   — DXGI Desktop Duplication        │
│  src/encoder.rs   — Rust wrapper around C++ shim    │
│  src/audio.rs     — WASAPI → Opus → RTP             │
│  src/input.rs     — Mouse/keyboard/gamepad inject   │
│  src/virtual_display.rs — VDD lifecycle (CCD API)   │
│                                                     │
│  shim/shim.cpp    — Zero-copy C++ NVENC FFI shim    │
│    DXGI texture → D3D11 Video Processor (NV12/P010) │
│    → NVENC (H.264 / HEVC Main10 / AV1)             │
└─────────────────────────────────────────────────────┘
      │
   Root\MttVDD   (Virtual Display Driver — IddCx)
   DXGI Desktop Duplication → physical or virtual output
```

---

## Media Pipeline

### SDR (default, `--codec h264`)
DXGI BGRA8 → D3D11 Video Processor (BT.709 full→limited) → NV12 → NVENC H.264

### HEVC / future HDR (`--codec hevc`)
DXGI BGRA8 (SDR) or R16G16B16A16_FLOAT (HDR desktop) → VP (scRGB→P010 BT.2020 PQ for HDR; BT.709 for SDR) → P010/NV12 → NVENC HEVC Main10 (HDR10 MDCV/CLL SEI) or Main

### AV1 (`--codec av1`)
Same VP path as HEVC; NVENC AV1. Advertised in ServerCodecModeSupport, not yet end-to-end tested.

---

## Virtual Display Driver (Headless)

Nova manages the [VirtualDrivers/Virtual-Display-Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver) (Root\MttVDD) lifecycle entirely in-process via SetupAPI + CCD (`SetDisplayConfig`). No HDMI dummy plug needed.

Boot sequence:
1. Pre-seeds all supported resolutions (720p–4K, 30/60/120 Hz) into `vdd_settings.xml`
2. Cycles the devnode once so the driver loads the full mode table
3. Parks VDD at 0×0 (dormant) via `ChangeDisplaySettingsExW`

On App 5 launch:
1. `SetDisplayConfig(SDC_TOPOLOGY_EXTEND)` — wakes VDD from dormant into the active desktop
2. `ChangeDisplaySettingsExW` — snaps VDD to client-negotiated resolution/refresh
3. CCD `SetDisplayConfig` — repositions VDD to desktop origin (new primary), deactivates physical display
4. DXGI rebinds to VDD; encoder recreates at VDD resolution → SPS matches client exactly
5. On stream end: full CCD topology restore, host audio endpoint restore

---

## Quick Start

```bash
git clone https://github.com/Zero19-85/Nova.git
cd Nova
cargo build --release
.\target\release\nova-server.exe
```

Optional flags:
```
--codec h264|hevc|av1    Encoder codec (default: h264)
--bitrate N              Starting bitrate Kbps — overridden by client ANNOUNCE (default: 15000)
--fps N                  Idle capture fps (default: 60)
--fec N                  FEC parity % — 0 disables (default: 20)
```

The binary self-manages the VDD installation and all encoder lifecycle. Requires an NVIDIA GPU with NVENC support (RTX series recommended).

---

## System Requirements

- **OS:** Windows 10 / 11
- **GPU:** NVIDIA (NVENC) — RTX series for HEVC/AV1
- **Virtual Display:** [VDD Control 25.7.23](https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/tag/25.7.23) — Nova downloads and installs automatically on first run
- **Gamepad passthrough:** [ViGEmBus](https://github.com/ViGEm/ViGEmBus) (optional)
- **Audio routing:** Steam Streaming Speakers (optional — falls back to host speakers)

---

## Known Limitations

- **H.264 at 4K@120fps** exceeds Xbox H264 decoder Level 5.2 — use 1080p or 1440p@60fps with H.264. True 4K@120fps requires HEVC (Xbox Moonlight 1.18+ with HEVC enabled).
- **HDR10** requires `--codec hevc` + a display in HDR mode (DXGI provides R16G16B16A16_FLOAT frames) + a Moonlight client that negotiates `videoFormat=0x102`.
- **mDNS auto-discovery** may not work across WiFi APs with multicast isolation — add the host IP manually in Moonlight.

---

## Roadmap

| Phase | Description | State |
|---|---|---|
| 1–3 | Core pipeline (DXGI→NVENC→RTP, RTSP, pairing) | ✅ Complete |
| 4 | Audio, input, cursor, reconnect, mDNS | ✅ Complete |
| 5 | VDD headless orchestration, dynamic resolution, Xbox pairing | ✅ Complete |
| 6 | HDR10 end-to-end (HEVC Main10, VDD HDR mode, SEI metadata) | 🔧 In progress |
| 7 | Portable single-exe (LTO, asset embedding, zero installer) | Planned |
