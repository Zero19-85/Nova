# Nova — Handoff to Next Chat

## ⚠️ NEWEST FIRST — session persistence, flow control, and the fallout (2026-08-17)

**Shipped, deployed live, and pushed. 124 lib tests + 69 in echo-client.** Detached sessions
with hot reconnect on both client kinds, a per-session bitrate budget, and four follow-up
fixes the first live runs exposed. Full engineering record and every invariant:
**CLAUDE.md → "Session persistence + flow control (2026-08-17)"** and the three sections
after it. Read those before touching `session_watcher`, `resume_suspended`,
`echo::session::sweep`, `qos.rs`, or the tunnel slot logic.

**Live-confirmed working:** detach on silence, `⚡ reclaiming its detached session`, the
5 s contended slot handover, an idle host at 0% encode, and the microphone restored to a
real device.

**Owed live validation:** the Moonlight side of detach/resume (only Echo has been exercised),
`resume_suspended`'s fast path (`⚡ Reclaimed the detached virtual display` — if you see
`🖥️ Cannot reclaim …` instead, the reason is in the parentheses and the session is still
correct, just not fast), and the grace expiry actually firing at 300 s.

**⚠️ The APK must be rebuilt** — `nativeRelease` and the dashboard's "End session on host"
button are client-side only, and the host already has everything it needs for them.

### What the live runs cost, in order — each one is a lesson worth keeping

1. **A closed tunnel tore down instead of detaching**, un-doing every detach microseconds
   after it happened. The sweep and the transport had only ever been tested apart.
2. **NVENC encoded at ~15 Mbps on a completely idle host** — the duplicate-frame path was
   gated on `client_connected`, the real-frame path never was.
3. **The Master re-sent one cached IDR forever** after any session ended, because
   `video_learned` and `last_idr` are sticky and a departed client keeps pinging.
4. **A returning client was locked out of the tunnel slot for 30 s** — its own re-punch from
   a new port erased the incumbent's liveness reading (`last_rx` remembers one sender).
5. **"End session on host" needed four presses** — it drove a real connection and stopped it
   at the grant, racing the session it had just created. Ending a session never needed a
   session; it is one RPC on the control tunnel.

**The recurring shape:** every one of these was found by reading `nova-service.log`, and
none by a test. Two of them were features that worked perfectly in isolation and were
cancelled by a neighbour. **When something new spans two modules, go and read what the other
one does at the same moment.**

**Three things were found already built while scoping this** — do not rebuild them: the
Android MediaCodec decoder (complete since 2026-08-15), QoS slow-start recovery
(`QosController` is already AIMD-with-memory), and the Moonlight detached state
(`Deactivate { cancelled: false }`).

**Configuration note:** the live `nova.toml` still sets `idle_teardown_secs = 300`, so this
box's detach grace is **5 minutes, not the 600 default**. The alias is working as designed;
add `detach_grace_secs` to change it.

---

## Previous handoff (2026-08-11, evening)

## TL;DR
**Session survival AND the UAC-screen flicker are both DONE and live-confirmed.** Connect at the
logon screen, enter the PIN, sign in, sign out, sign back in — the stream survives all of it,
full-screen 4K HDR10. Hovering clickable elements on the UAC/security screen is now clean.

**No known open bugs.** The next session starts on polish/QoL, not firefighting.

- Git HEAD `b598afd` on `main`, working tree clean, live install hash-verified (exe + dll), service
  Running. *(Stale as of 2026-08-17 — see the section above.)*
- Two items remain merely *unmeasured* (see the bottom) — neither is a known defect.

## Key locations
| What | Where |
|---|---|
| **Source repo** (build here) | `c:\Users\nova-server` |
| **LIVE install** (hot-patch target) | `C:\Program Files\Nova Server` |
| Worker log (capture/encode/VDD/audio **+ shim**) | `C:\Program Files\Nova Server\nova.log` |
| Master log (networking/pairing/QoS/sessions) | `C:\Program Files\Nova Server\nova-service.log` |
| Input-helper log | `C:\Program Files\Nova Server\nova-input.log` |
| Runtime config | `C:\Program Files\Nova Server\nova.toml` |
| Sunshine reference | `C:\Sunshine-2026.516.143833` (`src/platform/windows/display_ddup.cpp`) |
| Apollo reference | `C:\Apollo-0.4.6` |

## Build + hot-patch (**restart the service immediately after copying**)
```powershell
cd c:\Users\nova-server
cargo build --release      # expect 124 tests via: cargo test --lib (2026-08-17)
sc.exe stop NovaService; Start-Sleep 8
Copy-Item target\release\nova-server.exe "C:\Program Files\Nova Server\" -Force
Copy-Item target\release\nova_shim.dll   "C:\Program Files\Nova Server\" -Force  # if shim changed
sc.exe start NovaService                 # ALWAYS bring it straight back up
```
Hash-verify both files, then confirm `🎬 Master network stack ready` + both worker pipes in
nova-service.log.

---

## How the UAC flicker was actually fixed (2026-08-11)

Worth reading as a method, because the first hypothesis was wrong and the log said so.

**Symptom:** flicker on the security/UAC screen only, while the mouse moved over clickable elements.

**The wrong lead (mine, from code reading):** `blend_cursor_fp16` mapped sRGB 255 → scRGB 1.0 (80
nits) while this display's SDR white is 160. Real mismatch, fixed in `b301d1b` — but *not* the
flicker.

**What the logs actually showed:** the shim's `🔭` line (prints only on change) was alternating
BGRA8 ↔ FP16 **39 times in 16 seconds**, clustered exactly where frames were arriving and absent
across every `⏳ static desktop` stretch. Instrumenting further (`🎨` flip counter + logging the
duplication's REAL format) produced the decisive pair:

```
✅ DDA duplication active … (3840x2160 fmt=0xA — REQUESTED 0x57, DXGI declined)
🎨 DDA capture format changed 0xA → 0x57 (flip #206 …)
```

A duplication cannot change format mid-session — so those BGRA8 frames were never desktop frames.

**Root cause (`b598afd`):** `run_acquire_loop` copied `AcquireNextFrame`'s texture unconditionally,
never checking `LastPresentTime`. Zero there means the wake-up carried **no new desktop image** — a
pointer-only update (cursor moved / changed shape) — and the surface DXGI returns for it is not a
desktop frame. Hovering a clickable is almost entirely pointer-only updates, so the encoder
alternated between the real secure desktop and that other surface several times a second. Sunshine
gates on the same field in `display_ddup.cpp`.

**Fix:** refresh the desktop image only when `LastPresentTime != 0`, held as a cursor-free
`pristine` copy. Pointer-only updates still publish (else the cursor freezes until the screen
behind it repaints), but the cursor blends onto a COPY — which also stops successive positions
smearing a trail. A `🎨` line now means a genuine mid-duplication format change; it should be silent.

---

## Architecture facts that must not be re-broken
- **Never disable intra refresh.** It is Nova's ONLY repair path: infinite GOP, this HEVC client
  requests ~2 IDRs per session, RFI inert (0 recoveries). Keep `period=300, cnt=299`,
  `singleSliceIntraRefresh`. Turning it off regressed instantly into wrong/missing regions that
  never healed (`b3672d8` → reverted by `9b8bbb6`).
- **Encoder geometry/colour space belongs to the SESSION, not the capture.** A Moonlight client
  fixes its decoder at session start; changing resolution or HDR profile mid-session corrupts it
  permanently. The shim scales (aspect-preserving, even-aligned bars) and cross-converts SDR↔HDR, so
  any Worker can serve any session — that is what makes a mid-session handoff invisible.
- **Capture pixel format follows the DISPLAY's real Advanced Color state**, never the encoder's HDR
  flag. And `is_hdr=false` does **not** guarantee an SDR capture: on an Advanced Color display DXGI
  declines the BGRA8 request and returns FP16 (`REQUESTED 0x57, DXGI declined`). Harmless — the shim
  follows the real format per-frame — but never key logic off that flag.
- **SDR white is not a constant.** 160 nits on this box, queried via `DISPLAYCONFIG_SDR_WHITE_LEVEL`
  for the display being ENCODED FOR. Both the conversion shaders and the DDA cursor blend read it
  (`encoder::sdr_white_level()`), so they agree by construction — keep it that way.
- **VDD/CCD work is gated on the DESKTOP (secure vs Default), never the Worker's identity.**
- **Standard (non-admin) users:** only `bobby` is an administrator; `mordo` and `zimme` are not.
  Signing into a standard account makes the elevated Worker impossible (`ERROR_ELEVATION_REQUIRED`);
  detected and suppressed per-session, and the SYSTEM Worker drives the VDD so those sessions still
  stream. **Grep `↳ interactive spawn failed` first if a host comes up as a fallback unexpectedly.**
- **Already correct — do NOT "fix" again:** Master holds every socket (RTSP/control/RTP/audio) for
  the service's lifetime and never drops them across a Worker swap; `SERVICE_ACCEPT_SESSIONCHANGE`
  is registered and wakes the reconcile loop; desktop retargeting lives in `dda.rs`, `input.rs`,
  `capture/desktop_switch.rs`.

## Diagnostics that work now
- **Shim logging reaches nova.log** (fixed 2026-08-11 — `InitShimLog` had opened it `FILE_SHARE_READ`
  while Rust held it for write, so every `[Shim]`/`[NVENC]`/🔧/🔭 line was discarded for the whole
  Master/Worker era). Any older note saying otherwise is stale.
- `🔭` = capture size/format → encoder size/format + dst rect, **on change only**. Format codes:
  `0x57` BGRA8, `0xA` FP16, `0x67` NV12, `0x68` P010.
- `🎨` = DDA capture format changed on a real desktop frame (should never appear).
- `✅ DDA duplication active … fmt=0x…` = the duplication's ACTUAL format, and says so when DXGI
  declines the requested one.

## Unmeasured (not defects, just never observed)
1. **`⏱️ WM_ENDSESSION handled in N ms`** — the sign-out fix (`a63b277`) logs it; nobody has signed
   out since it deployed, so the number is still unknown.
2. **RFI** stays inert: this HEVC client sends zero `0x0301` invalidation requests. Advertised and
   wired, harmless. Exercising it needs an H.264 client or a Moonlight build that does HEVC RFI.

## Guardrails
- Commit style: user commits/pushes to `main` directly (solo repo).
- Check the Windows event log before assuming a crash:
  `Get-WinEvent -FilterHashtable @{LogName='System'; Id=@(41,1074,6005,6006,109)}`.
- **Read the logs before theorising.** Every root cause across the last two sessions came from a log
  line; every guess made without one was wrong — including the cursor-blend theory that opened this
  session.
