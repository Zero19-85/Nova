# Nova — Handoff to Next Chat (2026-08-10)

## TL;DR
Streaming pipeline is under a **CODE FREEZE** and is in great shape (smooth, resilient,
120–121 fps rock-solid, QoS + FEC eradicated the MoCA saturation). This session went after the
**session-transition (sign-in / sign-out survival)** bug — four separate defects found from the
2026-08-10 logs and all four fixed. **Needs the live boot → connect → sign-in → sign-out test
pass** (see "How to test the session-transition fix" below).

- Live install is running the fix (exe hash-verified, service healthy, 4K120 HDR session
  streaming on it right now).
- Remote: https://github.com/Zero19-85/Nova.git

## Key locations
| What | Where |
|---|---|
| **Source repo** (build here) | `c:\Users\nova-server` |
| **LIVE install** (hot-patch target) | `C:\Program Files\Nova Server` |
| Worker log (capture/encode/VDD/audio) | `C:\Program Files\Nova Server\nova.log` |
| Master log (networking/pairing/QoS/RFI signals) | `C:\Program Files\Nova Server\nova-service.log` |
| Input-helper log (secure-desktop injection) | `C:\Program Files\Nova Server\nova-input.log` |
| Runtime config | `C:\Program Files\Nova Server\nova.toml` (fec_percentage=5) |
| Sunshine reference source | `C:\Sunshine-2026.516.143833` |
| Apollo reference source | `C:\Apollo-0.4.6` |
| VDD driver source/package | `C:\VDD.Control.25.7.23` (and bundled `.\VirtualDisplayDriver\`) |

## Build + hot-patch the live install
The user is FINE with restarting NovaService to hot-patch (they reconnect after).
```
# from c:\Users\nova-server
cargo build --release          # produces target/release/{nova-server.exe, nova_shim.dll}
cargo test --lib               # expect 27 passed
# then (PowerShell), stop → copy → start:
sc.exe stop NovaService; Start-Sleep 8
Copy-Item target\release\nova-server.exe "C:\Program Files\Nova Server\" -Force
Copy-Item target\release\nova_shim.dll   "C:\Program Files\Nova Server\" -Force   # only if shim changed
sc.exe start NovaService; Start-Sleep 10; (Get-Service NovaService).Status
```
Always hash-verify the copy, and after start confirm `🎬 Master network stack ready` +
`worker control/media pipe connected` in nova-service.log.

## Architecture reminder (three processes — see CLAUDE.md "READ FIRST")
- **Master** (`--service`, LocalSystem, Session 0): all networking — pairing/rtsp/control/rtp,
  mDNS, RtpSender, audio TX, Worker supervision. Logs → nova-service.log.
- **Worker** (`--worker`, elevated user, console session): capture/encoder/VDD/audio/input/tray.
  Logs → nova.log.
- **Input helper** (`--system-input-helper`, SYSTEM primary token): secure-desktop KBM injection
  only, spawned per lock interlude. Logs → nova-input.log.
- IPC: `\\.\pipe\NovaControl` + `\\.\pipe\NovaMedia` (Master↔Worker), `\\.\pipe\NovaInput`
  (Master→helper). `service.rs WORKER_SPLIT_ENABLED = true`.
- **Cross-process gotcha:** anything the Master needs about the encoder/GPU it CANNOT query
  directly (Master has no encoder). This bit us 3× (input injection, QoS, RFI advertisement).
  For tray Server Stats: RTP stats are Master-side (`rtp.rs TxEngine` `📊 RTP/s`), encoder/QoS
  stats are Worker-side (`🎞 Encoder output`, `QosController` in lib.rs) — telemetry to the tray
  (Worker-side, `src/tray.rs`) will need the Master's RTP numbers pushed over IPC.

## Where recent work lives (for the tray/stats task and context)
- `src/tray.rs` — the system tray (Worker-side; also spawned in monolithic run()). Start here for UI.
- `src/rtp.rs` — `TxEngine` holds per-second RTP stats (frames, data/parity pkts, KB/s, buffer drops).
- `src/lib.rs` — both capture loops; `QosController` (AIMD-with-memory); `qos_tick`; frame pacing
  (`advance_frame_deadline`); media_supervisor / control_supervisor (Master); run_worker (Worker).
- `src/encoder.rs` — `RFI_ENABLED = true`, `set/get_stream_bitrate_kbps`, congestion signal.
- `src/control.rs` — ENet control; `idr_request_is_congestion` (QoS-via-IDR + 8s warmup grace).
- `src/config.rs` — nova.toml (`fec_percentage` default 5).
- `src/service.rs` — SCM service, spawn/upgrade reconcile loop; `session_is_unlocked()` (the
  upgrade gate), `spawn_host_in_session` vs `spawn_host_as_system_fallback`.
- `src/capture/mod.rs` — `DesktopManager::rebind` / `maybe_swap_backend`: which backend serves
  which (identity, desktop) combination. WGC is impossible under SYSTEM — see item 2(C).
- `src/ipc.rs` — `ConfigureStart` wire format (append-only: both sides are one binary, but a
  field added mid-struct silently shifts every later field, so add at the END and bump both
  `encode_into` and `decode` together).

## OPEN ITEMS for next session
1. **[PARTIAL — superseded by item 2] Lock-screen kick fix (`4b7edf9`).** Arms a 45 s cooldown when
   an interactive spawn fails despite a user token. User confirmed connect-at-sign-in + PIN entry
   holds, and that much is real — but the 2026-08-10 logs show the thrash was only *slowed*, not
   stopped: the cooldown could fail to arm, and on expiry it killed the host again anyway. See
   item 2(A) for the actual root cause (ARSO locked-session tokens) and the real fix.
1b. **[FIXED 2026-08-09 `24761dd`, deployed] Green-half / wrong-geometry at connect.** Connecting
   at the sign-in screen drove VDD activation on the Winlogon desktop where SetDisplayConfig is
   denied (error 5); the resolution force failed but WGC+encoder came up at 4K anyway = green half.
   Fix: `apply_configure_start` skips VDD activation while `desktop_is_secure()`; the worker loop
   re-runs the stored ConfigureStart (launch_app cleared) once the desktop returns to Default. Happy
   path (logged-in launch) unchanged. Same live-test caveat as 1 — confirm with a real sign-in-screen
   connect → login (should come up clean with no green half, no app-5 restart).
2. **Session-transition survival (sign-in / sign-out) — FOUR BUGS FIXED 2026-08-10, NEEDS LIVE
   TEST.** Symptoms reported: reboot → connect → wrong/garbled screen, physical monitor stays lit,
   and signing in kicks the client (had to quit app 5 and relaunch). The 2026-08-10 logs
   (nova-service.log ~line 66180+ and 70502+, nova.log ~305046+) showed four independent defects,
   each of which alone breaks the transition:
   - **(A) Upgrade thrash — the host was being killed in a loop.** `🔑 User token now available
     → upgrading` fired over and over (hundreds of times across the morning), each one a
     `stop_host` that kills the Worker and drops the client. Two causes, both fixed in
     `service.rs`: (i) **a user token exists at a LOCKED session** — Windows 11 ARSO (automatic
     restart sign-on after an update reboot) signs the user in and re-locks, so the token-only
     gate read "signed in!" at what is visually a lock screen; the gate now also requires
     `session_is_unlocked()` (`WTSQuerySessionInformationW(WTSSessionInfoEx)`, LOCK=0/UNLOCK=1);
     (ii) the 45 s anti-thrash cooldown **re-probed the token at spawn-result time** and could
     skip arming when that probe raced the login state — now armed unconditionally whenever an
     upgrade attempt lands back on a fallback (`upgrade_attempted`), and also on a failed spawn.
   - **(B) The interactive-spawn failure reason was never logged.** `spawn_host_in_session`'s
     error was swallowed unless the SYSTEM fallback ALSO failed, so "why did it fall back?" was
     undiagnosable. Now printed (`↳ interactive spawn failed (…) — trying SYSTEM fallback`).
     **Read this line first next time** — it names the real privilege/session problem.
   - **(C) A SYSTEM-fallback Worker tried to use WGC and died on every ConfigureStart.** WGC is
     impossible under SYSTEM (`0x80070424`, the long-known Phase 15.2c finding), but
     `DesktopManager::rebind` and `maybe_swap_backend` both force-route DDA→WGC whenever the
     input desktop is Default — which is exactly the state after someone signs in. Result:
     `❌ apply_configure_start failed: Capture rebind failed … 0x80070424` on repeat, the Worker
     never streamed, the VDD never activated (hence the physical monitor staying lit). Both paths
     are now gated on `!service::is_system_fallback()`; under SYSTEM, DDA stays the steady state
     on the interactive desktop too (Sunshine's model).
   - **(D) A replacement Worker restarted the wire frame index at 1 → permanent black.**
     moonlight-common-c discards any frame whose index is before the next one it expects
     (`isBefore32`), so a Worker adopted mid-session (sign-in upgrade, sign-out fallback, crash
     respawn) fed the client frames it threw away forever — the client stays connected and black
     until the app is quit and relaunched, which is exactly the reported symptom. Fixed with a
     new `ipc::ConfigureStart::start_frame_index`: `RtpSender` tracks `last_sent_index()` (session
     high-water mark, immune to keepalive retransmits, cleared by `reset()`), and Master's
     `control_supervisor` stamps `last + 1` on every outbound ConfigureStart — including the
     replay to a newly-connected Worker, which also now forces `launch_app = false` so an adoption
     can't start a second copy of the app. Regression test:
     `rtp::tests::last_sent_index_tracks_high_water_mark_and_resets`.
   - Also **removed the `host_has_connected_client()` gate** on the upgrade. It was dead in the
     split deployment (only the monolithic path ever set the event) AND backwards: the handoff to
     an interactive host is precisely how a just-signed-in client gets WGC/HDR/VDD back. With (D)
     fixed, the client survives the swap as a ~2–5 s freeze instead of a disconnect.

   - **(E) THE ONE UNDERNEATH — geometry pinning (`b3bcc1b`).** With A–D fixed, the 2026-08-10
     21:25 test showed the transition machinery working perfectly (one clean upgrade, replay at
     wire frame 4773, new Worker at 3840x2160 HDR10, zero rebind failures) — and the client STILL
     broke. Cause: **a Moonlight client fixes its decoder's resolution and HDR profile at session
     start; changing either mid-session corrupts it permanently** (black + green region, needs an
     app relaunch; the green is the shim's `CopyResource` no-op'ing on a size mismatch — the same
     mechanism as the old green-half bugs). And the change is unavoidable, because a
     SYSTEM-fallback Worker **cannot drive the VDD at all** — `SetDisplayConfig` is denied on the
     Winlogon desktop (error 5) regardless of token — so it serves the physical monitor's native
     size in SDR while the client negotiated 4K HDR10. Fix: geometry is now a property of the
     SESSION, not of whichever Worker serves it. New `ControlMsg::WorkerCapabilities`
     (`vdd_capable` + native size) → `session_negotiate::negotiate` pins a session started at the
     logon screen to the monitor's native size + SDR for its whole life (the full Worker just
     drives the VDD at that pinned size after sign-in), and a Worker that can't match a live
     session's geometry now REFUSES the ConfigureStart so the client freezes on its last good IDR
     (via the existing keepalive) instead of being handed a different stream.
     **Deliberate cost:** a session started at the logon screen stays at the monitor's native
     res/SDR until you reconnect, and Moonlight renders it inside the larger surface it allocated
     (image doesn't fill the frame). Making it fill needs encoder-size/capture-size decoupling —
     scaling + an SDR-into-HDR-session shader path — in the shim. That's the open follow-up if the
     framing matters; it would also permanently kill the "green" bug class.

   **How to test the session-transition fix** (the whole point — do this end to end):
   1. Reboot, do NOT sign in. Connect from Moonlight at the sign-in screen. Expect the login UI at
      the MONITOR's native size (2560x1440 here), SDR, not filling the client's frame — that is now
      correct-by-design, not a bug. nova-service.log should show `🧩 Master: worker capabilities —
      vdd=false native=2560x1440` and `📐 Master: session pinned to 2560x1440 SDR`.
   2. Type the PIN over Moonlight and sign in. Expect a brief freeze, then the SAME 1440p SDR
      stream continuing — **no kick, no green, no app-5 restart**, and the physical monitor should
      go dark (the full Worker drives the VDD at the pinned 1440p). Expect exactly ONE `🔑 Session
      N is signed in + unlocked — upgrading…` and `🔁 … (resuming at wire frame N)` with N ≫ 1.
   3. Sign out mid-stream. Expect the client to keep the connection and land back on the logon
      screen at the same geometry (the fallback Worker can serve 1440p SDR), still usable for a
      remote PIN.
   4. Sign in again — same profile and a DIFFERENT profile (the second one also exercises the
      `🔄 Console session X → Y` respawn path).
   5. Disconnect and reconnect while signed in → this session negotiates unconstrained, so expect
      full 3840x2160 HDR10. Then sign out DURING it: the client should FREEZE on its last frame
      (still connected, `⏸️ Worker: cannot serve this session`) and resume 4K when you sign back
      in — it will not show the logon screen in this state, by design.
   If it kicks again, the first thing to grep is `↳ interactive spawn failed` (B) — that now names
   the cause directly.
3. **RFI is ON but inert for this client:** advertised (`refPicInvalidation:1`) but the HEVC
   Moonlight client sends 0 invalidation requests (legacy H.264-centric feature). Harmless. The
   real streaming win was FEC 20→5 + AIMD-with-memory, NOT RFI — don't misattribute.
4. **Pre-existing shim-logging gap:** shim `ShimLog` (🧩/[NVENC]/[Shim] lines) doesn't reach
   nova.log (file-open/sharing issue). Only matters if you need shim-side visibility. Rust-side
   logs are the source of truth (e.g. the RFI signal is control.rs's `→ RFI recovery`, not 🧩).

## Guardrails
- Streaming pipeline is FROZEN — don't reopen capture/encode/RTP/QoS/RFI/pacing internals without
  a real regression + confirming intent first.
- Commit style: end messages with `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`; only
  push when asked. User commits/pushes directly to `main` (solo repo, no PRs).
- Check the Windows event log FIRST before assuming a "crash" (a suspected Master crash was once
  just a user reboot): `Get-WinEvent -FilterHashtable @{LogName='System'; Id=@(41,1074,6005,6006,109)}`.
