# Nova — Handoff to Next Chat (2026-08-09)

## TL;DR
Streaming pipeline is under a **CODE FREEZE** and is in great shape (smooth, resilient,
120–121 fps rock-solid, QoS + FEC eradicated the MoCA saturation). Next planned work was
**UI / QoL — a System Tray menu with live Server Stats**. One live regression was found and
partially fixed at the very end (lock-screen host kick loop) that **needs a live boot/lock test**.

- Git HEAD: `4b7edf9` on `main`, pushed. Remote: https://github.com/Zero19-85/Nova.git
- Working tree clean; live install matches HEAD (exe + dll hash-verified).

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
- `src/service.rs` — SCM service, spawn/upgrade reconcile loop (the lock-screen bug below lives here).

## OPEN ITEMS for next session
1. **[DONE — CONFIRMED LIVE 2026-08-09] Lock-screen kick fix (`4b7edf9`).** Bug was: at a locked
   session the fallback→interactive upgrade thrashed every ~5s, each `stop_host` kicking the remote
   client at the PIN screen. Fix arms a 45s cooldown when an interactive spawn fails despite a user
   token. **User verified it's squashed — connect-at-sign-in-screen + PIN entry now holds.** No
   action needed unless it regresses.
2. **"Sign OUT without dropping the connection" — STILL OPEN, UNTESTED.** The deferred Phase 16
   session-survival bug (CLAUDE.md has a 5-step plan). User has NOT tested it (I said it needs
   work). Full ask: boot → connect+PIN → stays → sign out without drop → sign into ANY profile →
   keeps running. Item 1 delivered the connect-at-PIN half; THIS is the remaining half and the
   natural first task next session. Start with the CLAUDE.md Phase 16 5-step plan (read
   nova-service.log + nova.log across the sign-out boundary first).
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
