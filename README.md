# Nova — Native Rust GameStream Host

A zero-copy, native Rust + C++ NVENC game-streaming host that speaks the Moonlight/GameStream protocol. Goal: replace Sunshine with a portable, minimal single executable.

---

## Current State — **Alpha 2.0** (Phase 16: Session-Survival Architecture)

Alpha 2.0's headline feature is **full lock-screen streaming**: reboot the host, connect from Moonlight, see the Windows login screen, and type your PIN with the remote mouse and keyboard. Getting there required splitting Nova into two processes and adding a third, short-lived one — see [Architecture](#architecture).

| Layer | Status |
|---|---|
| Pairing (RSA/AES-ECB, PEM plaincert) | ✅ Xbox & Android confirmed |
| Per-client trust — TLS client-cert identity, per-device store | ✅ Working |
| Pairing PIN dialog across the process split (Master↔Worker relay) | ✅ Working |
| RTSP handshake (OPTIONS/DESCRIBE/SETUP×3/ANNOUNCE/PLAY) | ✅ Working |
| H.264 video (NVENC CBR, infinite GOP, intra-refresh) | ✅ Working |
| HEVC Main8 / Main10 | ✅ Working (Xbox 4K@120 confirmed) |
| AV1 Main8 (SDR, low-overhead OBU) | ✅ Working (Pixel 9 Pro confirmed) |
| HDR10 (HEVC Main10, BT.2020 PQ, MDCV/CLL SEI) | ✅ Working (Xbox confirmed) |
| 120 Hz negotiation (CCD-committed refresh) | ✅ Working |
| Strict frame pacing (duplicate P-frames on a static desktop) | ✅ Working |
| RTP packetizer + Reed-Solomon FEC | ✅ Working |
| ENet control stream (IDR, ping, input, disconnect) | ✅ Working |
| Congestion control (loss-driven bitrate cut + ramp-back) | ✅ Working |
| Audio (WASAPI loopback → Opus → RTP, AES-128-CBC) | ✅ Working |
| Ghost audio sink + mid-session routing watchdog | ✅ Working |
| Mouse (absolute + raw relative), keyboard, gamepad (ViGEmBus) | ✅ Working |
| Cursor compositing (WGC native; manual blend on DDA incl. HDR) | ✅ Working |
| Universal Virtual Display Driver (all apps, headless) | ✅ Working |
| VDD boots dormant — physical monitors undisturbed | ✅ Working |
| Dynamic resolution (VDD follows client negotiation, IddCx/CCD-native) | ✅ Working |
| Dynamic monitor naming (renames VDD to client device) | ✅ Working |
| `/resume` after client quit-without-disconnect (zombie sessions) | ✅ Working |
| **Master/Worker split — network survives sign-out & session swap** | ✅ Working |
| Secure-desktop capture (UAC / Ctrl+Alt+Del / lock screen mid-stream) | ✅ Working (WGC↔DDA live swap) |
| **Lock-screen streaming — connect pre-login, see the PIN screen** | ✅ Working |
| **Remote PIN entry — SYSTEM input helper defeats the UIPI swallow** | ✅ Working |
| SYSTEM launcher service (`NovaService`) — no logon task needed | ✅ Working |
| Emergency display restore (logoff/shutdown/crash paths) | ✅ Working |
| `nova.toml` runtime config (no recompile needed) | ✅ Working |
| Inno Setup installer (driver + service install, upgrade-safe) | ✅ Working |

---

## Echo — Nova's own client

Everything above is Nova serving **Moonlight**, and it still does. Echo is a
second, parallel path: Nova's own native client, built so the host can be
reached over the internet without port forwarding and so features Moonlight's
protocol has no room for can exist at all.

Echo runs over **one hole-punched UDP path**, with every datagram sealed under
per-session AES-128-GCM keys exchanged over TLS. It shares Nova's capture,
encoder and audio pipeline wholesale — Echo added framing, not media.

| Capability | Status |
|---|---|
| NAT hole punching + relay signalling (no port forwarding) | ✅ Working |
| Per-session sealed media (AES-128-GCM, FEC over ciphertext) | ✅ Working |
| Android client — HEVC via MediaCodec onto a SurfaceView | ✅ Working |
| GameStream PIN pairing from the client, RSA-2048 identity | ✅ Working |
| Mouse (relative + absolute) and keyboard, own unreliable channel | ✅ Working |
| **Microphone passthrough — the phone becomes the PC's mic** | ✅ Working |
| **Game audio downstream — host → client, Opus, jitter-buffered** | ✅ Working |
| **A/V sync engine — video delayed to meet audio's hardware floor** | ✅ Working |
| Gamepad over Echo | 🔜 Planned |

### Audio, both directions

Upstream (**phone mic → PC**) renders into VB-CABLE and appears in Windows as
"CABLE Output" — any application can select it. Downstream (**PC → phone**)
carries the same Opus the GameStream path encodes, sealed onto Echo's own
channel because an Echo client listens on nothing but its punched path.

The two are kept apart by construction: the microphone renders into VB-CABLE,
and the game-audio ghost sink resolver refuses to select it. Pointing both at
one cable would feed the game back to the remote user as their own microphone.

### A/V sync

Audio latency on the reference device measures **190 ms** — 40 ms track buffer,
20 ms decoder, ~70 ms device output stage, plus 60–80 ms of jitter buffer. Most
of that is a hardware floor: the fast mixer path is already granted and the
jitter depth is burst insurance that measurement showed was necessary.

Audio cannot be pulled forward, so **video is pushed back** to meet it. The
client holds each frame a measured interval past its arrival, tracking the audio
pipeline's own latency reading automatically.

**It is off by default**, because it buys sync with input latency — a delayed
frame is a delayed view of your own mouse. Right for watching something, wrong
for playing something. The toggle lives in the stream's control panel and says
so.

Details, and the bugs that shaped all of it, are in `HANDOFF_ECHO_AUDIO.md`,
`HANDOFF_ECHO_INPUT.md`, `HANDOFF_ECHO_P2P.md` and `HANDOFF_ECHO_ANDROID.md`.

---

## Architecture

Nova runs as **three cooperating processes**, each holding exactly the privileges its job requires. This is not incidental complexity — Windows makes it mandatory, and each boundary below exists because a single-process design provably cannot cross it:

- **WGC capture needs an interactive user.** `Windows.Graphics.Capture`'s broker fails with `0x80070424` under SYSTEM, so the capture/encode process must run as the logged-in user.
- **The Winlogon desktop admits only SYSTEM.** Capturing the lock/PIN screen needs a SYSTEM-derived token, which the user-session process cannot obtain for itself.
- **Injecting input into the credential provider needs a SYSTEM *primary* token.** UIPI judges the injecting process's primary token, not a thread's impersonation token — see [the UIPI trap](#the-uipi-silent-swallow).
- **Session 0 cannot inject input at all.** `SendInput` is session-local, so the service can never do it on the Worker's behalf.

```
Moonlight client
      │  HTTP  :47989 / HTTPS :47984  (pairing + app list, client-cert verified)
      │  RTSP  :48010                 (session negotiation)
      │  ENet  :47999 UDP             (control — IDR, ping, input)
      │  RTP   :47998 UDP             (video frames + FEC)
      │  RTP   :48000 UDP             (Opus audio)
      ▼
┌──────────────────────────────────────────────────────────────────────┐
│  MASTER — `nova-server --service`   (NovaService, SYSTEM, Session 0) │
│  Owns ALL networking, so client connections survive sign-out,        │
│  fast user switching, and Worker respawns.                           │
│    pairing.rs · rtsp.rs · control.rs · rtp.rs · session_negotiate.rs │
│    mDNS · RtpSender · audio TX · Worker supervision                  │
└───────────────┬──────────────────────────────────┬───────────────────┘
                │ \\.\pipe\NovaControl             │ \\.\pipe\NovaInput
                │ \\.\pipe\NovaMedia               │ (only while locked)
                ▼                                  ▼
┌───────────────────────────────────┐  ┌────────────────────────────────┐
│  WORKER — `--worker`              │  │  INPUT HELPER                  │
│  (elevated USER, console session) │  │  `--system-input-helper`       │
│                                   │  │  (SYSTEM primary token,        │
│  capture/  wgc.rs · dda.rs ·      │  │   console session)             │
│            desktop_switch.rs      │  │                                │
│  encoder.rs → shim → NVENC        │  │  Spawned only for a secure-    │
│  virtual_display.rs (VDD/CCD)     │  │  desktop interlude; injects    │
│  audio.rs (WASAPI → Opus)         │  │  mouse/keyboard into Winlogon, │
│  input.rs (mouse/kbd/gamepad)     │  │  then is killed on unlock.     │
│  tray.rs (icon + PIN dialog)      │  │  Gamepad stays on the Worker.  │
└───────────────────────────────────┘  └────────────────────────────────┘
      │
   Root\MttVDD   (Virtual Display Driver — IddCx, MttVDD 25.7.23)
   Boots dormant (devnode disabled). Activated per-stream.
```

**Why the split matters.** Before Phase 16, signing out killed the one process that owned both the network and the capture pipeline, so every client connection died with it. Now the Master holds the sockets and the client session while Workers come and go underneath it — at sign-out, session swap, or a crash. The Master replays the active session's `ConfigureStart` to each newly-connected Worker so streaming resumes without the client noticing.

**Capture backends.** WGC is primary (HDR-capable, DWM-composited cursor). When the input desktop switches to the secure desktop — a UAC prompt, Ctrl+Alt+Del, or the logon/lock screen — Nova swaps live to DXGI Desktop Duplication on a dedicated thread that impersonates the service-supplied SYSTEM token, then swaps back when the interactive desktop returns. A Worker started before login boots directly on DDA, and if no backend is available yet it retries in place every 3 s instead of exiting (which would crash-loop against the service's respawn backoff).

**Frame pacing.** WGC/DDA only deliver frames on desktop damage, so a motionless screen used to starve the client's decoder and let CBR degrade the picture until something moved. Nova now re-submits the last captured surface as a duplicate P-frame on every missed slot, giving Moonlight an uninterrupted constant-fps bitstream — the idle bitrate goes into refining the static image instead. NVENC still idles at 0% when no client is connected.

### The UIPI "silent swallow"

Worth documenting because it costs days to diagnose. With SYSTEM impersonation and a successful `SetThreadDesktop(Winlogon)`, the Worker's `SendInput` calls **return success** at the PIN screen and nothing appears in the password field.

The ACL checks impersonation satisfies (`OpenInputDesktop`, `SetThreadDesktop`, DXGI duplication) are kernel-object checks, which honour a thread's impersonation token. Injected input reaching the credential provider is gated separately by UIPI/integrity in `win32k`, which evaluates the **injecting process's primary token** — and the Worker's is the interactive user (High integrity), below Winlogon's System integrity. The call is accepted at the API boundary and the event is dropped before the UI sees it, so there is no error to log. The signature of this trap: **capture works, input silently doesn't.**

Sunshine sidesteps it by running its host as SYSTEM-in-session, which Nova cannot do (WGC breaks). Nova instead spawns a minimal `--system-input-helper` process with a SYSTEM primary token into the console session for the duration of the interlude, and the Master detours mouse/keyboard packets to it. Everything else — WGC, HDR, audio, gamepad — stays with the interactive Worker.

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

**On stream start:**
1. `DICS_ENABLE` wakes the devnode; a CCD guard prevents it stealing primary on arrival
2. `SetDisplayConfig(SDC_TOPOLOGY_EXTEND)` adds the VDD as a secondary display
3. CCD source-mode write snaps the VDD to the client-negotiated resolution **and refresh** (IddCx ignores legacy `ChangeDisplaySettingsExW`; the target-mode index is invalidated so 120 Hz actually commits, and the committed value is read back and logged)
4. CCD topology write makes the VDD primary and deactivates physical display paths (true headless)
5. `SPDRP_FRIENDLYNAME` renames the VDD devnode to the connected device's paired name (e.g. "Xbox")
6. WGC rebinds to the VDD; the encoder is recreated at the negotiated resolution
7. The Worker launches the selected app **onto the virtual display** (the VDD is primary by this point, so windows can't open on the sleeping physical panel)
8. On stream end: full CCD topology restore, audio endpoint restore, devnode disabled again

**App routing.** Apps 2 (Steam Big Picture), 3 (Xbox app), 4 (RetroArch), and 5 (Virtual Desktop) all run headless on the virtual display. App 1 (Desktop) mirrors the physical primary. `nova.toml → headless_for_all_apps = true` (the default) routes *everything* through the VDD, including App 1.

**Display safety net:** `impl Drop`, console-ctrl hooks, and a dedicated `WM_ENDSESSION` monitor window all funnel into one claim-once emergency restore — physical monitors come back even on logoff, OS shutdown, or a hard crash mid-stream. Boot-time healing covers the power-loss case.

---

## Quick Start

```bash
git clone https://github.com/Zero19-85/Nova.git
cd Nova
cargo build --release
.\target\release\nova-server.exe --install-service
```

The service deployment is strongly recommended — it is what enables lock-screen streaming, secure-desktop capture, and session survival. A bare `.\nova-server.exe` still runs the legacy single-process host for quick local testing.

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

**Internal modes** (spawned by the service, not for manual use): `--worker`, `--system-input-helper`, `--service`, `--system-token <n>`, `--system-fallback`, `--skip-vdd-cycle`.

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
headless_for_all_apps = true   # route every app through the VDD (apps 2-5 are
                               # always headless regardless of this setting)

[audio]
endpoint_override = ""         # friendly-name substring or endpoint ID of the
                               # device to use as the ghost sink (empty = built-in
                               # list: Steam Streaming Speakers, VB-CABLE)

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
4. Starts the service, which spawns the Worker into the current session

Upgrades stop the running service/host first so binaries are never locked mid-copy. An optional (opt-in) task can disable the UAC secure desktop for setups that prefer prompts on the normal desktop; the uninstaller restores the Windows default.

---

## Deployment Files

```
nova-server.exe      ← main binary (all modes; must be alongside nova_shim.dll)
nova_shim.dll        ← C++ NVENC/D3D11 shim
nova.toml            ← runtime config (auto-created on first run)
nova.log             ← Worker log   (capture/encode/VDD/audio)
nova-service.log     ← Master log   (networking, pairing, Worker supervision)
nova-input.log       ← input-helper log (secure-desktop injection only)
nova_paired.json     ← per-device trust store, keyed by client-cert SHA-256
VirtualDisplayDriver\← VDD package (bundled by installer)
```

---

## System Requirements

- **OS:** Windows 10 1803+ / Windows 11
- **GPU:** NVIDIA with NVENC — RTX series recommended for HEVC/HDR10; **RTX 40-series (Ada) or newer required for AV1 encode**
- **VDD:** Bundled in installer (`VDD.Control.25.7.23`) — no manual install needed
- **Gamepad passthrough:** [ViGEmBus](https://github.com/ViGEm/ViGEmBus) (optional — Nova offers to install it on first run)
- **Audio routing:** Steam Streaming Speakers or another virtual audio device (optional — falls back to host speakers)

---

## Known Limitations

- **Signing out mid-stream leaves the client on a black screen.** The stream does not automatically recover to the login screen; disconnect and reconnect in Moonlight to get it back. Reboot → lock screen → remote PIN entry works correctly; this affects the *sign-out* transition specifically. Fix planned for the next polish pass (see [Roadmap](#roadmap)).
- **H.264 at 4K@120fps** exceeds H.264 decoder Level 5.2 on some clients (e.g. Xbox) — use HEVC or AV1 at high resolutions/refresh rates.
- **AV1 is 8-bit SDR only** for now (Main8). HDR sessions negotiate HEVC Main10; AV1 Main10 is planned.
- **mDNS auto-discovery** may not work across WiFi APs with multicast isolation — add the host IP manually in Moonlight.
- **Cursor on the secure desktop** is blended manually on the DDA path (all shape types, SDR + HDR); minor visual differences vs. DWM compositing are possible during UAC/lock-screen interludes.
- **Bundled Virtual Audio Driver (MTT) cannot load** under Secure Boot — it is code-signed but not Microsoft attestation-signed (device problem code 52). Steam Streaming Speakers or VB-CABLE serve as the ghost sink instead; do not work around this by disabling Secure Boot.
- **Scheduled-task deployment** (`--install`) still works but cannot capture the secure desktop or lock screen, and does not get the Master/Worker split — the service deployment is required for those.
- **Echo's client has no Opus packet-loss concealment.** Android's `MediaCodec` exposes no concealment entry point, so a lost audio packet is a 20 ms gap rather than an extrapolated one. Loss is reported separately from silence so the two are never confused. Software PLC is on the backlog.
- **Echo's A/V sync costs input latency** when enabled — it works by holding video back. Off by default; leave it off while playing.
- **Echo game audio requires a virtual sink** (Steam Streaming Speakers or NVIDIA Virtual Audio). Without one the host has no ghost sink to capture and the stream is silent. VB-CABLE is deliberately excluded from that list — it is the microphone's endpoint.

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
| **16** | **Master/Worker split, lock-screen streaming + remote PIN entry (SYSTEM input helper), strict frame pacing, universal VDD app routing** | ✅ **Alpha 2.0** |
| 17 | Sign-out stream recovery, AV1 Main10/HDR, attestation-signed audio driver | 🔜 Planned |

Echo's own track, numbered separately because it is a parallel product:

| Phase | Description | State |
|---|---|---|
| E1–E5 | Hole punching, relay signalling, sealed media, RUDP tunnel, keyframe gate | ✅ Complete |
| E6 | Android client — JNI bridge, MediaCodec decode, PIN pairing | ✅ Complete |
| E7 | Mouse + keyboard input, own unreliable channel; microphone passthrough | ✅ Complete |
| **E8–E9** | **Ghost-sink isolation, downstream game audio, A/V sync engine** | ✅ **Complete** |
| E10 | Gamepad over Echo; software Opus PLC on the client | 🔜 Planned |
