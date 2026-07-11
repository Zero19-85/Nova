# Nova — Native Rust GameStream Host

A zero-copy, native Rust + C++ NVENC game-streaming host that speaks the Moonlight/GameStream protocol. Goal: replace Sunshine with a portable, minimal single executable.

---

## Current State — Production Alpha (Phase 15)

| Layer | Status |
|---|---|
| Pairing (RSA/AES-ECB, PEM plaincert) | ✅ Xbox & Android confirmed |
| Per-client trust — TLS client-cert identity, per-device store | ✅ Working |
| RTSP handshake (OPTIONS/DESCRIBE/SETUP×3/ANNOUNCE/PLAY) | ✅ Working |
| H.264 video (NVENC CBR, infinite GOP, intra-refresh) | ✅ Working |
| HEVC Main8 / Main10 | ✅ Working (Xbox 4K@120 confirmed) |
| AV1 Main8 (SDR, low-overhead OBU) | ✅ Working (Pixel 9 Pro confirmed) |
| HDR10 (HEVC Main10, BT.2020 PQ, MDCV/CLL SEI) | ✅ Working (Xbox confirmed) |
| 120 Hz negotiation (CCD-committed refresh) | ✅ Working |
| RTP packetizer + Reed-Solomon FEC | ✅ Working |
| ENet control stream (IDR, ping, input, disconnect) | ✅ Working |
| Congestion control (loss-driven bitrate cut + ramp-back) | ✅ Working |
| Audio (WASAPI loopback → Opus → RTP, AES-128-CBC) | ✅ Working |
| Mouse (absolute + raw relative), keyboard, gamepad (ViGEmBus) | ✅ Working |
| Cursor compositing (WGC native; manual blend on DDA incl. HDR) | ✅ Working |
| Universal Virtual Display Driver (all apps, headless) | ✅ Working |
| VDD boots dormant — physical monitors undisturbed | ✅ Working |
| Dynamic resolution (VDD follows client negotiation, IddCx/CCD-native) | ✅ Working |
| Dynamic monitor naming (renames VDD to client device) | ✅ Working |
| `/resume` after client quit-without-disconnect (zombie sessions) | ✅ Working |
| Secure-desktop capture (UAC / Ctrl+Alt+Del visible mid-stream) | ✅ Working (WGC↔DDA live swap) |
| SYSTEM launcher service (`NovaService`) — no logon task needed | ✅ Working |
| Lock-screen streaming — connect pre-login, type your PIN remotely | 🧪 Implemented, validating |
| Emergency display restore (logoff/shutdown/crash paths) | ✅ Working |
| `nova.toml` runtime config (no recompile needed) | ✅ Working |
| Inno Setup installer (driver + service install, upgrade-safe) | ✅ Working |

---

## Architecture

```
Moonlight client
      │  HTTP  :47989 / HTTPS :47984  (pairing + app list, client-cert verified)
      │  RTSP  :48010                 (session negotiation)
      │  ENet  :47999 UDP             (control — IDR, ping, input)
      │  RTP   :47998 UDP             (video frames + FEC)
      │  RTP   :48000 UDP             (Opus audio)
      ▼
┌────────────────────────────────────────────────────────────────┐
│  NovaService (SYSTEM)  — thin launcher, `nova-server --service` │
│    spawns ⇩ the host into the console session (elevated user   │
│    token + inheritable SYSTEM impersonation token for DDA)     │
├────────────────────────────────────────────────────────────────┤
│  Nova host  (nova-server.exe)                                  │
│                                                                │
│  src/config.rs          — nova.toml runtime config             │
│  src/pairing.rs         — HTTP/HTTPS pairing + per-cert trust  │
│  src/rtsp.rs            — RTSP session + SDP negotiation       │
│  src/control.rs         — ENet reliable-UDP control            │
│  src/rtp.rs             — RTP packetizer + RS-FEC              │
│  src/capture/           — capture backends + desktop switching │
│    wgc.rs               —   Windows.Graphics.Capture (primary) │
│    dda.rs               —   DXGI duplication (secure desktop / │
│                              lock screen, SYSTEM impersonation)│
│    desktop_switch.rs    —   input-desktop change detection     │
│  src/encoder.rs         — Rust wrapper around C++ shim         │
│  src/audio.rs           — WASAPI → Opus → RTP (single owner)   │
│  src/input.rs           — Mouse/keyboard/gamepad inject        │
│  src/virtual_display.rs — VDD lifecycle (SetupAPI + CCD)       │
│  src/service.rs         — SCM service + host token spawn       │
│  src/shutdown.rs        — WM_ENDSESSION emergency restore      │
│  src/tray.rs            — tray icon + pairing dialog           │
│                                                                │
│  shim/shim.cpp          — Zero-copy C++ NVENC FFI shim         │
│    WGC/DDA BGRA8/FP16 → typed-RTV pixel shaders (NV12/P010)    │
│    → NVENC (H.264 / HEVC Main8+Main10 / AV1 Main8)             │
└────────────────────────────────────────────────────────────────┘
      │
   Root\MttVDD   (Virtual Display Driver — IddCx, MttVDD 25.7.23)
   Boots dormant (devnode disabled). Activated per-stream.
```

**Capture backends.** WGC is the primary backend (HDR-capable, DWM-composited cursor). When the input desktop switches to the secure desktop — a UAC prompt, Ctrl+Alt+Del, or the logon/lock screen — Nova swaps live to DXGI Desktop Duplication running on a dedicated thread that impersonates SYSTEM (token supplied by the service), then swaps back when the interactive desktop returns. The client keeps seeing the real screen the whole time. A host started before login boots directly on DDA so the lock screen is streamable.

---

## Media Pipeline

### SDR — H.264 / HEVC Main8 / AV1 Main8
WGC BGRA8 → typed-RTV pixel shaders (BT.709 full→limited) → NV12 → NVENC

### HDR10 — HEVC Main10
WGC R16G16B16A16_Float (FP16 scRGB, VDD Advanced Color) → pixel shaders (scRGB→P010 BT.2020 PQ) → NVENC HEVC Main10 + HDR10 MDCV/CLL SEI

### AV1 notes
Confirmed working end-to-end (Moonlight Android, Pixel 9 Pro). Nova emits the low-overhead OBU bitstream Moonlight expects (`TD → SEQ_HDR → FRAME` on keyframes) — the NVIDIA SDK sample class's default IVF container wrapping is disabled, which was the root cause of AV1 being undecodable in earlier builds. Currently 8-bit SDR (Main8, `0x1000`); AV1 Main10/HDR is a planned follow-up. **NVENC AV1 encode requires an RTX 40-series (Ada) or newer GPU.**

---

## Virtual Display Driver — Headless Mode

Nova manages the [VirtualDrivers/Virtual-Display-Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver) (`Root\MttVDD`) lifecycle entirely in-process via SetupAPI + the Windows CCD API (`SetDisplayConfig`). No HDMI dummy plug needed.

**Boot sequence:**
1. Pre-seeds all supported modes (720p/1080p/1440p/4K × 30/60/120Hz) into `vdd_settings.xml`
2. Enables HDRPlus in `vdd_settings.xml` (required for Advanced Color / FP16 mode)
3. Cycles the devnode once so the driver loads the full mode table, heals any stale headless topology left by an unclean shutdown, then hardware-disables the devnode (`DICS_DISABLE`) — invisible to DXGI/CCD/PnP until a client connects
4. Sweeps phantom monitor devnodes left behind by previous enable/disable cycles

**On stream start (every app, controlled by `nova.toml → headless_for_all_apps`):**
1. `DICS_ENABLE` wakes the devnode; a CCD guard prevents it stealing primary on arrival
2. `SetDisplayConfig(SDC_TOPOLOGY_EXTEND)` adds the VDD as a secondary display
3. CCD source-mode write snaps the VDD to the client-negotiated resolution **and refresh** (IddCx ignores legacy `ChangeDisplaySettingsExW`; the target-mode index is invalidated so 120 Hz actually commits, and the committed value is read back and logged)
4. CCD topology write makes the VDD primary and deactivates physical display paths (true headless)
5. `SPDRP_FRIENDLYNAME` renames the VDD devnode to the connected device's paired name (e.g. "Xbox")
6. WGC rebinds to the VDD; the encoder is recreated at the negotiated resolution
7. On stream end: full CCD topology restore, audio endpoint restore, devnode disabled again

**Display safety net:** `impl Drop`, console-ctrl hooks, and a dedicated `WM_ENDSESSION` monitor window all funnel into one claim-once emergency restore — physical monitors come back even on logoff, OS shutdown, or a hard crash mid-stream. Boot-time healing covers the power-loss case.

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

**Service / deployment subcommands:**
```
--install-service        Register NovaService (SYSTEM launcher, auto-start)
--uninstall-service      Stop + remove the service
--install / --uninstall  Legacy scheduled-task deployment (fallback — no
                         secure-desktop or lock-screen capture)
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

[hdr]
max_luminance_nits = 1000      # match your TV: HDR600 / HDR1000 / HDR2000
max_cll_nits       = 1000
max_fall_nits      = 400
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
2. Runs `devcon.exe install MttVDD.inf Root\MttVDD` — installs the driver under the installer's elevated token
3. Runs `nova-server.exe --install-service` — registers **NovaService** (LocalSystem, auto-start) and removes any legacy scheduled task
4. Starts the service, which spawns the host into the current session

Upgrades stop the running service/host first so binaries are never locked mid-copy. An optional (opt-in) task can disable the UAC secure desktop for setups that prefer prompts on the normal desktop; the uninstaller restores the Windows default.

---

## Deployment Files

```
nova-server.exe      ← main binary (host + service modes; must be alongside nova_shim.dll)
nova_shim.dll        ← C++ NVENC/D3D11 shim
nova.toml            ← runtime config (auto-created on first run)
nova.log             ← host log  (auto-created, tail for diagnostics)
nova-service.log     ← service log (spawn/respawn history)
nova_paired.json     ← per-device trust store, keyed by client-cert SHA-256
VirtualDisplayDriver\← VDD package (bundled by installer)
```

---

## System Requirements

- **OS:** Windows 10 1803+ / Windows 11
- **GPU:** NVIDIA with NVENC — RTX series recommended for HEVC/HDR10; **RTX 40-series (Ada) or newer required for AV1 encode**
- **VDD:** Bundled in installer (`VDD.Control.25.7.23`) — no manual install needed
- **Gamepad passthrough:** [ViGEmBus](https://github.com/ViGEm/ViGEmBus) (optional — Nova offers to install it on first run)
- **Audio routing:** Steam Streaming Speakers or virtual audio device (optional — falls back to host speakers)

---

## Known Limitations

- **H.264 at 4K@120fps** exceeds H264 decoder Level 5.2 on some clients (e.g. Xbox) — use HEVC or AV1 at high resolutions/refresh rates.
- **AV1 is 8-bit SDR only** for now (Main8). HDR sessions negotiate HEVC Main10; AV1 Main10 is planned.
- **mDNS auto-discovery** may not work across WiFi APs with multicast isolation — add the host IP manually in Moonlight.
- **`audio.endpoint_override`** is stored in `nova.toml` but not yet wired into the WASAPI pipeline.
- **Cursor on the secure desktop** is blended manually on the DDA path (all shape types, SDR + HDR); minor visual differences vs. DWM compositing are possible during UAC/lock-screen interludes.
- **Scheduled-task deployment** (`--install`) still works but cannot capture the secure desktop or lock screen — the service deployment is required for those.

---

## Roadmap

| Phase | Description | State |
|---|---|---|
| 1–4 | Core pipeline (DXGI→NVENC→RTP, RTSP, pairing, audio, input) | ✅ Complete |
| 5 | VDD headless orchestration, dynamic resolution | ✅ Complete |
| 6 | HDR10 end-to-end (HEVC Main10, VDD Advanced Color, SEI) | ✅ Complete |
| 7–9 | Deployment, graceful shutdown, VDD boot isolation, perf, installer | ✅ Complete |
| 10–11 | VDD on-demand lifecycle, NVENC quality, congestion control, perf polish | ✅ Complete |
| 12 | IddCx CCD-native resolution switching | ✅ Complete |
| 13 | `/resume` zombie sessions, frameIndex fix, install elevation, boot healing | ✅ Complete |
| 14 | Per-client cert trust, phantom-monitor cleanup, emergency display restore, HDR/120Hz negotiation | ✅ Complete |
| 15 | Secure-desktop capture (WGC↔DDA), SYSTEM launcher service, audio single-owner, AV1 | ✅ Complete |
| Next | AV1 Main10/HDR, audio endpoint override wiring, lock-screen boot validation | 🔜 Planned |
