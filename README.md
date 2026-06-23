# Nova — Native Rust GameStream Host

A zero-copy, native Rust + C++ NVENC game-streaming host that speaks the Moonlight/GameStream protocol. Goal: replace Sunshine with a portable, minimal single executable.

---

## Current State — Production Alpha (Phase 9)

| Layer | Status |
|---|---|
| Pairing (RSA/AES-ECB, PEM plaincert) | ✅ Xbox & Android confirmed |
| RTSP handshake (OPTIONS/DESCRIBE/SETUP×3/ANNOUNCE/PLAY) | ✅ Working |
| H.264 video (NVENC CBR, infinite GOP) | ✅ Working |
| HEVC Main8 / Main10 | ✅ Working |
| RTP packetizer + Reed-Solomon FEC | ✅ Working |
| ENet control stream (IDR, ping, input, disconnect) | ✅ Working |
| Audio (WASAPI loopback → Opus → RTP, AES-128-CBC) | ✅ Working |
| Mouse input — absolute (desktop) | ✅ Working |
| Mouse input — relative (game camera, raw delta) | ✅ Working |
| Keyboard + gamepad (ViGEmBus) | ✅ Working |
| Cursor compositing (WGC native) | ✅ Working |
| Universal Virtual Display Driver (all apps, headless) | ✅ Working |
| VDD boots dormant — physical monitors undisturbed | ✅ Working |
| Dynamic resolution (VDD follows client negotiation) | ✅ Working |
| HDR10 (HEVC Main10, VDD Advanced Color, MDCV/CLL SEI) | ✅ Working |
| Dynamic monitor naming (renames VDD to client device) | ✅ Working |
| `nova.toml` runtime config (no recompile needed) | ✅ Working |
| Graceful shutdown — physical monitors always restored | ✅ Working |
| Inno Setup installer (devcon runs as installer, no UAC) | ✅ Working |
| AV1 | 🔧 Advertised, not end-to-end tested |

---

## Architecture

```
Moonlight client
      │  HTTPS :47989/:47984  (pairing + app list)
      │  RTSP  :48010         (session negotiation)
      │  ENet  :47999 UDP     (control — IDR, ping, input)
      │  RTP   :47998 UDP     (video frames + FEC)
      │  RTP   :48000 UDP     (Opus audio)
      ▼
┌──────────────────────────────────────────────────────────┐
│  Nova  (nova-server.exe)                                 │
│                                                          │
│  src/config.rs        — nova.toml runtime config         │
│  src/pairing.rs       — HTTP/HTTPS GameStream pairing    │
│  src/rtsp.rs          — RTSP session + SDP negotiation   │
│  src/control.rs       — ENet reliable-UDP control        │
│  src/rtp.rs           — RTP packetizer + RS-FEC          │
│  src/capture.rs       — Windows Graphics Capture (WGC)  │
│  src/encoder.rs       — Rust wrapper around C++ shim     │
│  src/audio.rs         — WASAPI → Opus → RTP              │
│  src/input.rs         — Mouse/keyboard/gamepad inject    │
│  src/virtual_display.rs — VDD lifecycle (SetupAPI + CCD) │
│  src/debug.rs         — File logger (nova.log)           │
│                                                          │
│  shim/shim.cpp        — Zero-copy C++ NVENC FFI shim     │
│    WGC BGRA8/FP16 → D3D11 Video Processor (NV12/P010)   │
│    → NVENC (H.264 / HEVC Main8 + Main10 / AV1)          │
└──────────────────────────────────────────────────────────┘
      │
   Root\MttVDD   (Virtual Display Driver — IddCx, MttVDD 25.7.23)
   Boots dormant (CCD path inactive). Activated per-stream.
```

---

## Media Pipeline

### SDR — H.264 (default)
WGC BGRA8 → D3D11 Video Processor (BT.709 full→limited) → NV12 → NVENC H.264

### SDR — HEVC Main8
Same VP path → NV12 → NVENC HEVC Main

### HDR10 — HEVC Main10
WGC R16G16B16A16_Float (FP16 scRGB, VDD Advanced Color) → VP (scRGB→P010 BT.2020 PQ) → NVENC HEVC Main10 + HDR10 MDCV/CLL SEI

### AV1
Same VP path as HEVC; NVENC AV1. Advertised, not yet end-to-end tested.

---

## Virtual Display Driver — Headless Mode

Nova manages the [VirtualDrivers/Virtual-Display-Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver) (`Root\MttVDD`) lifecycle entirely in-process via SetupAPI + the Windows CCD API (`SetDisplayConfig`). No HDMI dummy plug needed.

**Boot sequence:**
1. Pre-seeds all supported modes (720p/1080p/1440p/4K × 30/60/120Hz) into `vdd_settings.xml`
2. Enables HDRPlus in `vdd_settings.xml` (required for Advanced Color / FP16 mode)
3. Cycles the devnode once so the driver loads the full mode table
4. `ccd_deactivate_vdd_path()` — clears `DISPLAYCONFIG_PATH_ACTIVE` on the VDD's CCD path and writes to the database with `SDC_SAVE_TO_DATABASE`. Physical monitors are never disturbed.

**On stream start (every app, controlled by `nova.toml → headless_for_all_apps`):**
1. `SetDisplayConfig(SDC_TOPOLOGY_EXTEND)` — wakes VDD from dormant into the active desktop
2. `ChangeDisplaySettingsExW` — snaps VDD to client-negotiated resolution/refresh
3. CCD `SetDisplayConfig` — moves VDD source to desktop origin (new primary), deactivates physical display paths
4. `SetupDiSetDeviceRegistryPropertyW(SPDRP_FRIENDLYNAME)` — renames VDD to client device name (e.g. "Xbox")
5. WGC rebinds to VDD; encoder recreates at VDD resolution → SPS matches client exactly
6. On stream end: full CCD topology restore, audio endpoint restore, Advanced Color disable

**Graceful shutdown — Dead Man's Switch:**
`impl Drop for VirtualDisplay` + explicit teardown at `run()` exit ensures physical monitors are always restored even on Ctrl+C, OS shutdown, or panic. `enc.cleanup()` always runs before `vd.deactivate_after_stream()` to release D3D texture references before the VDD's CCD path is removed.

---

## Quick Start

```bash
git clone https://github.com/Zero19-85/Nova.git
cd Nova
cargo build --release
.\target\release\nova-server.exe
```

On first run Nova creates `nova.toml` in the exe directory. Edit it to change bitrate, codec, fps, and other settings without recompiling.

**CLI overrides** (all optional — `nova.toml` values used when omitted):
```
--codec h264|hevc|av1    Encoder codec
--bitrate N              Bitrate Kbps
--fps N                  Frame rate
--fec N                  FEC parity % (0 = disabled)
--width N / --height N   VDD boot resolution
```

---

## Configuration — `nova.toml`

Auto-generated on first run alongside `nova-server.exe`:

```toml
[stream]
width                = 1920    # VDD boot resolution (Moonlight overrides per-session)
height               = 1080
bitrate_kbps         = 15000
fps                  = 60
codec                = "h264"  # "h264" | "hevc" | "av1"
enable_hdr           = false   # force HDR10 even if VDD capability query is slow
headless_for_all_apps = true   # route all apps through VDD (set false for App 5 only)

[audio]
endpoint_override = ""         # friendly name or GUID of audio endpoint (empty = default)

[network]
fec_percentage = 20            # Reed-Solomon parity % (0 = disabled)
```

---

## Installer

`nova.iss` at the project root is the production Inno Setup script. It bundles the VDD package and installs the driver using the installer's own admin token — no UAC child-process suppression, no internet download required at runtime.

**Build steps:**
```powershell
cargo build --release
# Copy pre-extracted VDD package to project root:
Copy-Item -Recurse "C:\VDD.Control.25.7.23" ".\VirtualDisplayDriver"
# Open nova.iss in Inno Setup Compiler and press Compile
# Output: Output\NovaSetup-0.1.0.exe
```

**What the installer does:**
1. Copies `nova-server.exe`, `nova_shim.dll`, and `VirtualDisplayDriver\` to `{app}`
2. Runs `devcon.exe install MttVDD.inf Root\MttVDD` — installs driver under the installer's elevated token
3. Runs `nova-server.exe --install` — registers the ONLOGON/Highest-Privileges scheduled task
4. Launches Nova for the current session

---

## Deployment Files

```
nova-server.exe      ← main binary (must be alongside nova_shim.dll)
nova_shim.dll        ← C++ NVENC/D3D11 shim
nova.toml            ← runtime config (auto-created on first run)
nova.log             ← rolling log (auto-created, tail for diagnostics)
nova_paired.json     ← paired device store (auto-created after first pair)
VirtualDisplayDriver\← VDD package (bundled by installer)
```

---

## System Requirements

- **OS:** Windows 10 1803+ / Windows 11
- **GPU:** NVIDIA with NVENC — RTX series recommended for HEVC/AV1/HDR10
- **VDD:** Bundled in installer (`VDD.Control.25.7.23`) — no manual install needed
- **Gamepad passthrough:** [ViGEmBus](https://github.com/ViGEm/ViGEmBus) (optional)
- **Audio routing:** Steam Streaming Speakers or virtual audio device (optional — falls back to host speakers)

---

## Known Limitations

- **H.264 at 4K@120fps** exceeds Xbox H264 decoder Level 5.2 — use 1080p@60fps on Xbox. HEVC resolves this.
- **Xbox Moonlight 1.18.0** reports `x-nv-clientSupportHevc:0` — investigation pending.
- **mDNS auto-discovery** may not work across WiFi APs with multicast isolation — add the host IP manually in Moonlight.
- **`audio.endpoint_override`** is stored in `nova.toml` but not yet wired into the WASAPI pipeline (Phase 10).
- **Monitor rename** updates Device Manager immediately; Display Settings reflects it on most Windows 11 builds.

---

## Roadmap

| Phase | Description | State |
|---|---|---|
| 1–4 | Core pipeline (DXGI→NVENC→RTP, RTSP, pairing, audio, input) | ✅ Complete |
| 5 | VDD headless orchestration, dynamic resolution | ✅ Complete |
| 6 | HDR10 end-to-end (HEVC Main10, VDD Advanced Color, SEI) | ✅ Complete |
| 7 | Task Scheduler deployment, DPI fix, file logger | ✅ Complete |
| 8 | HDR teardown, device naming, pairing UX, DLL deploy | ✅ Complete |
| 9 | Graceful shutdown, VDD boot isolation, perf sweep, config, installer | ✅ Complete |
| 10 | AV1 confirmed, audio endpoint override, monitor child-devnode rename | Planned |
