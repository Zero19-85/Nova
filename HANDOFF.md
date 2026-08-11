# Nova — Handoff to Next Chat (2026-08-11)

## TL;DR
The **session-survival work is DONE and live-confirmed**: connect at the logon screen, enter the
PIN, sign in, sign out, sign back in — the stream survives all of it, full-screen 4K HDR10, no
kicks, no app-5 restarts. Streaming quality is good (no shimmer, no missing regions).

**ONE bug remains, and it is narrow:** flickering **on the security (UAC) screen only**, when the
mouse moves over clickable elements (e.g. the "Yes"/consent link). Everything else is clean.

- Git HEAD `9b8bbb6` on `main`. **11 commits UNPUSHED** — push when convenient.
- Working tree clean; live install hash-verified against HEAD (exe + dll); service Running.

## Key locations
| What | Where |
|---|---|
| **Source repo** (build here) | `c:\Users\nova-server` |
| **LIVE install** (hot-patch target) | `C:\Program Files\Nova Server` |
| Worker log (capture/encode/VDD/audio **+ shim**) | `C:\Program Files\Nova Server\nova.log` |
| Master log (networking/pairing/QoS/sessions) | `C:\Program Files\Nova Server\nova-service.log` |
| Input-helper log | `C:\Program Files\Nova Server\nova-input.log` |
| Runtime config | `C:\Program Files\Nova Server\nova.toml` |
| Sunshine reference | `C:\Sunshine-2026.516.143833` (`src/nvenc/nvenc_base.cpp`) |
| Apollo reference | `C:\Apollo-0.4.6` (`src/nvenc/nvenc_base.cpp`) |

## Build + hot-patch (user is fine with this; **restart the service immediately after copying**)
```powershell
cd c:\Users\nova-server
cargo build --release      # expect 30 tests via: cargo test --lib
sc.exe stop NovaService; Start-Sleep 8
Copy-Item target\release\nova-server.exe "C:\Program Files\Nova Server\" -Force
Copy-Item target\release\nova_shim.dll   "C:\Program Files\Nova Server\" -Force  # if shim changed
sc.exe start NovaService                 # ALWAYS bring it straight back up
```
Hash-verify both files, then confirm `🎬 Master network stack ready` + both worker pipes in
nova-service.log.

---

## THE OPEN BUG — UAC/secure-screen flicker on hover

**Symptom:** while a UAC prompt is up, moving the mouse over clickable elements makes the picture
flicker. Not reproducible on the normal desktop any more (that was intra refresh, fixed in
`9b8bbb6`), and not the SDR white level (fixed in `0f5748a`).

**Why the secure desktop is special:** it is the only path where capture is **DDA**, and DDA is
also the only path where **Nova blends the cursor itself** (`dda.rs` `blend_cursor` /
`blend_cursor_fp16` — DDA delivers the pointer as separate metadata, not composited).

**Leads, roughly in order:**
1. **Cursor blend.** Hovering a clickable changes the cursor SHAPE (arrow → hand), and on this path
   every shape change re-runs the blend. `blend_cursor` dispatches on `frame.format`; check the
   FP16 variant's brightness math against the rest of the frame — its doc says it maps sRGB 255 →
   scRGB 1.0 (80 nits), which no longer matches the 160-nit SDR white level the shim now uses.
   **This mismatch is real and is my prime suspect.**
2. **Motion alternates fresh vs cached frames.** A static screen re-submits
   `capturer.cached_texture()`; motion delivers fresh ones. Any per-path difference shows up as
   flicker exactly when the mouse moves. Whatever differs must differ between those two.
3. **Secure-desktop SDR white level.** `refresh_sdr_white_level` reads the level for the VDD. The
   Winlogon desktop may be composited at a different level, so FP16 frames captured during the
   interlude could carry a different SDR white than the shim assumes.
4. **DDA restore churn.** The logs show frequent `AcquireNextFrame … access lost` → restore during
   interludes. `duplicate_output` silently falls back BGRA8 when the FP16 request fails, so the
   actual capture format can change across restores (the shim follows it correctly per-frame now,
   but the two paths must land at identical brightness for that to be invisible).

**Diagnosis tooling is good now** — see "shim logging" below. The `🔭` line reports capture
size/format → encoder size/format and the destination rect, per change.

---

## What was fixed this session (chain of root causes, newest first)
1. **`9b8bbb6` intra refresh at the reference cadence.** `b3672d8` had turned it OFF to match
   Sunshine/Apollo and that regressed instantly into wrong/missing regions that never healed:
   infinite GOP ⇒ no periodic keyframes, this HEVC client requests ~2 IDRs per session, RFI inert
   (0 recoveries) ⇒ **rolling intra refresh IS Nova's only repair mechanism**. Back on at the
   references' values (`period=300, cnt=299`, `singleSliceIntraRefresh`) instead of Nova's original
   `period=cnt=fps` (a gapless wave sweeping the frame every second — the desktop shimmer).
   **Do not turn it off again** unless you first give the client a different repair path.
2. **`0f5748a` SDR white level is not a constant.** Windows composites SDR into an Advanced Color
   surface at the display's own "SDR content brightness" — **160 nits on this box**, not BT.2408's
   203. Queried via `DISPLAYCONFIG_SDR_WHITE_LEVEL` for the display being ENCODED FOR (the VDD), fed
   to the shim's `SetSdrWhiteLevel` → `ToneMapParams` cbuffer at b0 → `gSdrWhiteScRGB`. Also closed
   an **ABA hazard**: the shim had cached capture size/format keyed on the texture POINTER, and a
   backend swap can reallocate at the same address. It reads the descriptor every frame now.
3. **`a63b277` sign-out no longer blocks on impossible work.** Windows blocks on `WM_ENDSESSION`;
   Nova ran a full display restore there regardless of cause, including a CCD restore that fails
   with error 5 because the secure desktop already owns the input. Now split on
   `ENDSESSION_LOGOFF`: sign-out restores audio only and returns (the successor Worker heals
   topology from the Default desktop); real shutdown keeps the full restore. Logs
   `⏱️ WM_ENDSESSION handled in N ms`. **Not yet measured live — check this on the next sign-out.**
4. **`2c58ea8` the freeze: Master was dropping every frame.** `media_supervisor` gated forwarding on
   `video_learned`, cleared it on each new media pipe, and re-armed from `try_learn_target()` — an
   EDGE detector. A Worker respawn re-adopts a client that never went away, so the edge never fires
   and the gate stays shut forever. New `RtpSender::has_target()` ("is a target KNOWN"). Plus a
   thaw IDR on every ConfigureStart, and an orphaned-VDD heal on Worker startup (a Worker killed by
   sign-out leaves the VDD enabled+primary; the replacement then captured a dead virtual display =
   black screen with cursor).
5. **`d594d20` encoder/capture decoupling + shim logging + standard users.** See below.

## Architecture facts that must not be re-broken
- **Encoder geometry/colour space belongs to the SESSION, not the capture.** A Moonlight client
  fixes its decoder at session start; changing resolution or HDR profile mid-session corrupts it
  permanently (black + green region, needs an app relaunch on the client). The shim scales
  (aspect-preserving, even-aligned bars) and cross-converts SDR↔HDR, so any Worker can serve any
  session — that is what makes a mid-session handoff invisible.
- **Capture pixel format follows the DISPLAY's real Advanced Color state**, never the encoder's HDR
  flag (`rebind_capture_and_encoder`'s `capture_hdr`). Asking DXGI for FP16 from an SDR display
  gives scRGB with SDR white at 80 nits, and a failed FP16 request silently falls back to BGRA8.
- **VDD/CCD work is gated on the DESKTOP (secure vs Default), never the Worker's identity.** The old
  "no VDD under SYSTEM" rule came from a pre-login (i.e. secure-desktop) observation.
- **Standard (non-admin) users:** only `bobby` is an administrator; `mordo` and `zimme` are not.
  Signing into a standard account makes the elevated Worker impossible
  (`ERROR_ELEVATION_REQUIRED` — no linked token, exe is `requireAdministrator`). Detected and
  suppressed per-session now; the SYSTEM Worker drives the VDD so those sessions still stream.
  **Grep `↳ interactive spawn failed` first if a host comes up as a fallback unexpectedly.**
- **Already correct — do NOT "fix" again:** Master holds every socket (RTSP/control/RTP/audio) for
  the service's lifetime and never drops them across a Worker swap;
  `SERVICE_ACCEPT_SESSIONCHANGE`/`SERVICE_CONTROL_SESSIONCHANGE` are registered and wake the
  reconcile loop; desktop retargeting (`OpenInputDesktop`/`SetThreadDesktop`) lives in `dda.rs`,
  `input.rs`, `capture/desktop_switch.rs`.

## Shim logging WORKS now (fixed 2026-08-11)
`InitShimLog` opened nova.log with `FILE_SHARE_READ` while Rust held it for write ⇒ sharing
violation ⇒ **every `[Shim]`/`[NVENC]`/🔧/🔭 line was silently discarded for the whole Master/Worker
era**. Any older note saying "shim logs don't reach nova.log" is stale. Format codes in `🔭`:
`0x57` BGRA8, `0xA` FP16, `0x67` NV12, `0x68` P010.

## Guardrails
- Commit style: end messages with `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`; user
  commits/pushes to `main` directly (solo repo). Only push when asked.
- Check the Windows event log before assuming a crash:
  `Get-WinEvent -FilterHashtable @{LogName='System'; Id=@(41,1074,6005,6006,109)}`.
- **Read the logs before theorising.** Every root cause this session came from a log line, and every
  guess made without one was wrong.
