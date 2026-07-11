# Nova Project Documentation & Instructions

## Project Scope
Nova is an ultra-low footprint, native Rust game-streaming host.
**Goal:** Flawlessly mimic GeForce Experience so Moonlight clients can connect.
**Architecture:**
- Async backend (`tokio` / `hyper`) for networking, mDNS, and pairing (HTTPS/XML).
- Native C++ FFI shim (`shim.cpp`) compiled to `nova_shim.dll` — zero-copy DXGI-to-NVENC hardware encoding.
- Architecture targets: High performance, ultra-low latency, and minimal portable `.exe` footprint.

## Developer Rules for Claude
1. **Always verify:** Before executing changes, audit the Rust `Cargo.toml` and `build.rs` to ensure no hallucinated static links are injected into the NVENC pipeline.
2. **Performance First:** Keep dependencies minimal. Prioritize zero-copy transfers (DXGI to NVENC).
3. **Workflow:** I (the user) will use this chat to coordinate tasks. You have access to workspace files via Claude Code. Use this to audit code and apply edits directly. If a build fails, analyze the compiler output, identify the specific missing library or header, and fix the `build.rs` or shim pathing.
4. **Consistency:** Ensure pairing logic (port 47989) and discovery (mDNS) stay compliant with the GameStream protocol.
5. **Build output:** `cargo build --release` produces two files that must be deployed together: `nova-server.exe` and `nova_shim.dll` (both in `target/release/`). The DLL is built by `build.rs` via `cl.exe` + `link.exe /DLL` and copied automatically.

## Current Phase: Phase 15 — Secure-desktop capture (WGC+DDA dual backend), two-process privilege model, audio single-owner (2026-07-09)

Phase 15 is the "shippable, no-obvious-gaps-vs-Sunshine/Moonlight" push. The confirmed decision (do the full architecture, no half-measures): dual-backend capture (WGC primary + DDA secure-desktop fallback), a thin SYSTEM launcher service that spawns the interactive host, desktop-switch detection with seamless backend swap, and a single-owner audio lifecycle. Reference sources (patterns only, do not copy): Sunshine `C:\Sunshine-2026.516.143833` (`display_base.cpp` `syncThreadDesktop`, `display_ddup.cpp`, service arch in `misc.cpp`), Apollo `C:\Apollo-0.4.6`.

### Root causes driving Phase 15
- **Secure desktop is uncapturable today.** Nova runs as ONE interactive elevated process (`NovaServerBoot` task, `InteractiveToken`+`HighestAvailable`). WGC is bound to the interactive desktop and delivers black frames while the UAC/Winlogon secure desktop (`WinSta0\Winlogon`) is up. DDA (`IDXGIOutputDuplication`) keeps producing frames across the switch, but only from a thread that has `SetThreadDesktop(Winlogon)`, which needs the elevated/SYSTEM-derived token the launcher service provides. Hence WGC-primary + DDA-fallback, swapped on desktop-switch detection.
- **Audio lifecycle bug — dual ownership of the default render device (CONFIRMED via code audit).** TWO subsystems independently cache-and-restore the default endpoint: `audio::SinkGuard` (`audio.rs` — caches current default, swaps to virtual sink, restores on Drop) AND `virtual_display::VirtualDisplay` (`saved_audio_endpoint`, cached in `activate_for_stream`, restored in `deactivate_after_stream`, mirrored into `EMERGENCY_SNAPSHOT`). If the VDD cache runs while the sink swap is already engaged (the "already active" `/resume` re-activation at `lib.rs` ~830, or overlapping zombie sessions), VDD caches the *virtual sink* as the "real" endpoint and later restores the system TO the sink → host stuck silent. Separately the shim's `InitAudioCapture`/`CleanupAudio` are process-GLOBAL with no per-session guard, so an overlapping `/resume` can null the new session's capture client mid-start ("doesn't reliably start"). Fix = single ownership + a global init/cleanup mutex.

### 15.0 — Phase 0 (non-breaking scaffolding) — DONE (2026-07-09)
- **Capture abstraction layer.** `src/capture.rs` → `src/capture/` module: `mod.rs` (`trait DesktopCapture`, `enum BackendKind`, `enum CaptureBackend { Wgc, Dda }`, `struct DesktopManager`), `wgc.rs` (the existing `WgcCapturer`, moved verbatim — zero logic change), `dda.rs` (`DdaCapturer` — inert Phase-0 stub built from the shared `ID3D11Device`, real `IDXGIOutputDuplication` lands in Phase 2). `capture::WgcCapturer` is re-exported so `lib.rs` call sites are byte-for-byte unchanged; `DesktopManager` is defined and compiles but is NOT yet wired into `lib.rs` (Phase-0-inert items carry `#[allow(dead_code)]` with a Phase-2 removal note). Static-dispatch enum, not `dyn`, to keep the frame hot path zero-cost. `cargo check --lib` clean.
- **Secure-desktop UAC option (clean, reversible).** Installer choice: `nova.iss` `[Tasks]` `disablesecuredesktop` (UNCHECKED by default — explicit opt-in) + `[Registry]` write of `HKLM\...\Policies\System\PromptOnSecureDesktop = 0` with `Flags: uninsdeletevalue` (uninstall removes it → Windows default = secure desktop ON restored). Runtime counterpart `src/secure_desktop.rs` (`pub mod`): `is_prompt_on_secure_desktop()` / `set_prompt_on_secure_desktop(bool)` with native `RegSetValueExW` + elevated `reg.exe` fallback (same dual-layer idiom as the VDD `VDDPATH` writes), for a future tray toggle / diagnostics. Honest security framing in the module docs: the secure desktop defeats UAC-spoofing malware; disabling it is the documented trade-off RDP/AnyDesk/TeamViewer users routinely accept.

### 15.1 — Phase 1a: audio single-owner lifecycle — DONE (2026-07-09), not yet live-validated
- **`src/audio.rs` is now the SOLE owner of default-render-endpoint state.** New pieces:
  - `ORIGINAL_ENDPOINT` static (claim-once via `Mutex<Option<Vec<u16>>>::take`, poison-proof lock) — the ONLY place in the process that remembers the pre-stream endpoint. `pub fn arm_endpoint_restore()` (idempotent, earliest caller wins; refuses to arm the virtual sink itself) captures it; `restore_original_endpoint()` claims it, falling back to `recover_stuck_sink()`'s live-query recovery when nothing is armed — which also HEALS the old SinkGuard gap where "sink was already default at engage" left the host silent on stop. `pub fn emergency_restore_default_endpoint()` is the same claim for process-death paths.
  - **Arm-before-VDD-flip is load-bearing:** when the VDD becomes primary, Windows can auto-flip the default endpoint to the VDD's HDMI audio device (this was the real reason `VirtualDisplay` had its own cache). lib.rs calls `audio::arm_endpoint_restore()` immediately BEFORE both `vd.activate_for_stream` sites (pre-activation + PLAY-time fallback); `start_for_stream` arms again as fallback for non-VDD sessions.
  - `AudioCaptureManager` (replaces `AudioStreamer`; one instance in `run()`): `start_for_stream()` FIRST does a blocking `stop_and_release()` of any previous session — killing the `/resume`-over-zombie race where the old `audio_streamer = Some(AudioStreamer::start(..))` pattern evaluated the new start BEFORE dropping (and thus before joining) the zombie, letting the zombie's `CleanupAudio` null the new session's WASAPI state. `stop_and_release()` joins everything AND runs the endpoint restore even when no audio thread ever started (covers `/cancel` before PLAY after the VDD flip already happened); Drop = stop_and_release. In lib.rs the stop runs BEFORE `deactivate_after_stream`.
  - `SHIM_CAPTURE_ACTIVE` AtomicBool gate around the shim's process-global `InitAudioCapture`/`CleanupAudio` (compare_exchange with 2 s retry; released after `CleanupAudio` completes) — enforcement of the no-overlap invariant, loud error instead of silent corruption.
- **`VirtualDisplay` no longer touches audio:** removed `saved_audio_endpoint` field + `cache_default_audio_endpoint()` (and the `Win32_Media_Audio`/`Win32_System_Com` imports + local `SetDefaultAudioDevice` extern), removed `audio_endpoint` from `EmergencySnapshot`. `emergency_restore_for_shutdown` now calls `crate::audio::emergency_restore_default_endpoint()` — including on the no-snapshot early-return path (a non-VDD stream can have the sink swap engaged with no display snapshot armed).
- `cargo check` clean; all 8 runnable `cargo test --lib` tests pass (the 2 "unused BOOL" test-build warnings at virtual_display.rs:3231/3272 are pre-existing, in `#[ignore]`d GDI diagnostics).
- **Live validation needed:** stream start/stop audio on Xbox + Android, `/resume` after quitting Moonlight without disconnect (zombie path), `/cancel` before PLAY, and host-audio-restore after each.

### 15.1b — Phase 1b: desktop-switch detection — DONE (2026-07-09), detection only
- **`src/capture/desktop_switch.rs`** — `DesktopSwitchMonitor` (lifecycle handle) + process-global state (one input desktop per session; `WINEVENTPROC` has no user-data pointer, so state is atomics: `CURRENT` kind + `GENERATION` transition counter). Two detection layers on one background thread (`nova-desktop-switch`): `SetWinEventHook(EVENT_SYSTEM_DESKTOPSWITCH, WINEVENT_OUTOFCONTEXT)` (event callbacks delivered via the thread's message pump) + a 250 ms `MsgWaitForMultipleObjects`-timeout poll fallback that also covers hook-install failure. Every trigger re-queries `OpenInputDesktop`+`GetUserObjectInformationW(UOI_NAME)` — the DESKTOPSWITCH event doesn't say which desktop is active.
- **Load-bearing classification rule:** `OpenInputDesktop` failing (commonly `E_ACCESSDENIED`) is treated as `InputDesktop::Secure`, not an error — the Winlogon desktop's ACL only admits SYSTEM/winlogon, so "can't open the input desktop" usually MEANS the secure desktop is up. Phase 2's SYSTEM-derived token will open it for real.
- **Read API for Phase 2 (lock-free, hot-loop safe):** `current_input_desktop()` (`Default`/`Secure`/`ScreenSaver`/`Other`/`Unknown`) + `switch_generation()` (monotonic; swap logic compares generations so a fast Default→Secure→Default flip between reads is still visible). Transitions log once with source tag (`event`/`poll`/`startup`); steady state is silent. `Unknown` query results (mid-switch teardown) keep the last state — no flapping.
- **NO swap behavior:** nothing consumes the API yet (`#[allow(dead_code)]` with Phase-2 removal notes). lib.rs spawns the monitor at startup (named handle — kept alive for the whole `run()`), so live sessions log real UAC/logon transitions to validate detection ahead of Phase 2.
- New `windows` crate features (bindings only, zero new compiled/linked code — per rule #1): `Win32_UI_Accessibility`, `Win32_System_StationsAndDesktops`.
- Tests (`capture::desktop_switch::tests`): live query classifies the interactive desktop as `Default`; generation bumps exactly once per transition, never on repeat observation. Full suite: 10 passed / 0 failed / 7 ignored.
- **Live validation for Phase 2 readiness:** trigger a UAC prompt during a stream and confirm the log shows `Default → Secure` (likely via the poll path with an access-denied classification) and `Secure → Default` on dismissal.

### 15.3 — Pre-login "device connect/disconnect" boot loop — FIXED (2026-07-11), pending live boot validation
**Symptom:** from power-on until login, the Windows device connect/disconnect chime looped forever; stopped at login. **Diagnosis (from `nova-service.log` + `nova.log` of the 2026-07-11 14:40 UTC boot):** the service spawned the host pre-login and the host CRASHED within ~1 s every time; the service respawned it every 2 s reconcile tick, and each host start cycles the VDD devnode in `ensure_enabled_at_boot` (enable → ding, disable → dong) = the audible loop. Two independent pre-login crash modes, both fixed:
1. **`Instant` underflow panic (lib.rs:488/1044):** `Instant::now() - Duration::from_secs(30)` — `Instant` is QPC-since-boot on Windows, so a service-launched host starting <30 s after power-on panics ("overflow when subtracting duration from instant"). Only the service path ever runs that early (the old logon task couldn't), which is why it never hit before 15.2c. Fix: `checked_sub(...).unwrap_or_else(Instant::now)` at both sites.
2. **WGC init failure was fatal:** pre-login the input desktop is Winlogon and WGC's broker needs a real user session (`0x80070424`), so `DesktopManager::new_wgc` erred and `run()` exited → respawn loop. Fix: **DDA-first startup fallback** — `new_wgc` now falls back to `DdaCapturer` (built on a fresh `WgcCapturer::create_d3d11_device()`, now `pub(crate)`), which via the service's `--system-token` SYSTEM impersonation is exactly the backend that CAN capture the logon/lock screen. This is what makes the actual design goal work: Moonlight can connect at the lock screen and the user types their Windows PIN remotely. After login, the existing swap machinery returns to WGC; a new **idle heal** arm in the lib.rs frame loop (`else if backend_kind()==Dda { maybe_swap_backend() }`) hands the desktop back to WGC even when no client is connected (an idle DDA backend would otherwise hold the SYSTEM-impersonation thread + the output's single duplication slot forever). WGC→DDA swaps remain gated on `client_connected` so unwatched UAC prompts don't churn backends.
3. **Tray panic (tray.rs:80):** `Shell_NotifyIconW` needs the Explorer taskbar; pre-login the `.expect()` panicked the tray thread every spawn. Now retries every 10 s until the shell exists (menu/icon rebuilt per attempt since the builder consumes them), so the icon appears after login instead of never.
4. **Service crash-loop damper (service.rs worker):** any host exiting <30 s after spawn now triggers exponential respawn backoff (4→8→…→60 s, reset by a healthy run or session change; dead handle dropped before the backoff wait). Defense-in-depth: no future startup crash can ever be an audible 2 s loop again.
**Live validation:** reboot, do NOT log in — expect ONE devnode cycle, no chime loop, host alive on DDA showing the logon screen; pair/connect from Moonlight at the lock screen, type PIN, confirm WGC swap-back after login (`🔀 DDA → WGC`) and tray icon appearing post-login.

### Backlog
- **WGC capture border removal** — DONE (2026-07-10). `session.SetIsBorderRequired(false)` in `wgc.rs::open_session` (best-effort `let _ =` — needs Win10 20348+/Win11; older builds keep the border rather than failing capture). No explicit `RequestAccessAsync(Borderless)` consent call was needed for the unpackaged elevated host.
- **DDA cursor missing with HDR10** — DONE (2026-07-10). `blend_cursor` only handled BGRA8; HDR sessions duplicate the secure desktop as FP16 scRGB, so the blend no-op'd. Added `blend_cursor_fp16` (sRGB→linear via `srgb8_to_linear`, half-float read-modify-write via hand-rolled `f16_to_f32`/`f32_to_f16`) — all three shape types. SDR cursor white (sRGB 255) → scRGB 1.0 to match DWM's SDR composite.
- **AV1 (Main8/SDR) — ROOT CAUSE FOUND & FIXED (2026-07-11), CONFIRMED WORKING live on Pixel 9 Pro same day.** The failure that survived all 2026-07-10 fixes below (client never renders, endlessly re-requests IDRs, times out — while Apollo AV1 works on the same phone/GPU) was **the NVIDIA SDK sample class wrapping AV1 output in an IVF container**: `NvEncoder` defaults `bUseIVFContainer=true` and, for `NV_ENC_CODEC_AV1_GUID` ONLY, prepends a 32-byte "DKIF" IVF *file* header to the first packet and a 12-byte size+PTS IVF *frame* header to EVERY frame (`NvEncoder.cpp:657`). Moonlight passes the payload straight to the AV1 decoder — those non-OBU bytes make every frame undecodable (H264/HEVC never affected; the wrapper is AV1-only, which is why only AV1 broke). Proven with a local shim harness (dumps + OBU walker): pre-fix frame 1 began `44 4b 49 46` "DKIF"; post-fix the stream is clean `TD → SEQ_HDR → FRAME [→ PADDING]` on keyframes, `TD → FRAME` on P-frames, sizes exactly 44/12 bytes smaller. **Fix:** `NvEncoderD3D11` ctor takes/forwards `bUseIVFContainer`; shim.cpp passes `false`. Also (Apollo-parity audit vs `C:\Apollo-0.4.6\src\nvenc\nvenc_base.cpp`): removed the speculative forced `av1.level`/`tier` block (Apollo ships level AUTOSELECT and works — the "autoselect emits level 31" theory was wrong) and added `av1.chromaSamplePosition=1`. Verified-correct while diagnosing (do not re-suspect): `lastPayloadLen` = header-inclusive stream-tail length exactly as moonlight-common-c's non-NAL truncation path expects (no Sunshine gate), and `rtp.rs::av1_is_keyframe`'s OBU/LEB128 walk is sound.
- **AV1 — earlier layers (2026-07-10), all real but not the final blocker.** Prior symptom: stream starts, desktop never shows, disconnect on the bitrate watchdog. Root cause was NOT packetization (GameStream's NV_VIDEO_PACKET shard/FEC format is codec-agnostic) — it was **frame-type detection**: `detect_frame_type` did HEVC NAL parsing on AV1's OBU bytes, never matched an IDR, so every frame was marked P (1) and the client never got a keyframe. Fix:
  1. `codec_mode_support` `0x301` → `0x1301` (adds AV1 Main8 `0x1000`); `from_video_format` already maps `0x1000`→Av1 and rtsp.rs already offers `a=rtpmap:98 AV1/90000` + `bitStreamFormat=2`.
  2. `rtp.rs`: new `is_av1` flag (`set_codec(is_hevc, is_av1)`, no longer folds AV1 into is_hevc). `detect_frame_type` gains an AV1 path: `av1_is_keyframe()` walks the OBU stream (LEB128 `obu_size`) and returns IDR when an `OBU_SEQUENCE_HEADER` (type 1) is present — NVENC emits the seq header only with key frames, and Nova's IDRs are all on-demand with `NV_ENC_PIC_FLAG_OUTPUT_SPSPPS` (shim.cpp:1472) which inlines it for AV1 too. Unit test `av1_sequence_header_is_detected_as_idr`.
  3. **`shim.cpp` had NO AV1 config block** (only H264/HEVC) — AV1 ran on raw NVENC defaults, so frames encoded + IDRs were detected (`frame_type=2` confirmed live) but the client showed a **black screen** (undecodable stream). Added an `NV_ENC_CODEC_AV1_GUID` block mirroring H264/HEVC + Sunshine's AV1 config: `NV_ENC_AV1_PROFILE_MAIN_GUID`, `repeatSeqHdr=1`, `idrPeriod=NVENC_INFINITE_GOPLENGTH`, `outputAnnexBFormat=0` (low-overhead OBU — what Moonlight expects, NOT Annex-B), `chromaFormatIDC=1`, `enableBitstreamPadding=1`, 8-bit, `maxNumRefFramesInDPB=5`, `numFwdRefs=1`, BT709 color.
  - **Live validation needed:** stream AV1, confirm the desktop now DECODES (frame_type=2 was already confirmed; the black screen was the missing shim config). AV1 Main10/HDR (`0x2000`) is NOT enabled — the shim's AV1 path is 8-bit; that's a follow-up (needs the P010/Main10 AV1 config + `from_video_format` mapping 0x2000). **NVENC AV1 encode requires RTX 40-series/Ada** — on older GPUs the AV1 session fails to init.

### 15.2a/b — DDA backend + live WGC↔DDA swap — DONE (2026-07-09), not yet live-validated
- **`src/capture/dda.rs` — real `DdaCapturer`:** `IDXGIOutput5::DuplicateOutput1` with explicit format (FP16 for HDR sessions so the shim's FP16→P010 path is reused; BGRA8 SDR), falling back to `IDXGIOutput1::DuplicateOutput`. Output selection: match `DXGI_OUTPUT_DESC.DeviceName` against the session's GDI target (in true-headless the VDD IS the console primary, so the secure desktop renders on it), else the desktop-primary output, else first attached. `sync_thread_desktop()` (= Sunshine `syncThreadDesktop`: `OpenInputDesktop(GENERIC_ALL)`+`SetThreadDesktop`) runs before every duplication attempt — best-effort until 2c.
  - **Device topology:** duplication must run on the output's adapter. Same-LUID as the encoder (physical monitor on the NVIDIA GPU) ⇒ duplication on the encoder's own device, GPU-side `CopyResource` into a stable cache — zero-copy into NVENC (WGC parity). Different adapter (VDD's IddCx adapter / iGPU monitor) ⇒ private dup device + staging Map→`UpdateSubresource` bounce through system RAM — slow but only lives seconds at UAC-prompt duty cycle; logged once.
  - `AcquireNextFrame(0)`: WAIT_TIMEOUT⇒None, ACCESS_LOST⇒`access_lost` flag + dup dropped (manager restores or swaps back); `ReleaseFrame` immediately after the copy (a held frame blocks the compositor's next present).
  - **Documented limitation:** DDA doesn't composite the cursor (separate pointer metadata) — cursor is invisible during a secure-desktop interlude; clicks/motion still work. Cursor merge = possible later polish (Sunshine blends the shape buffer manually).
- **`DesktopManager` swap (`capture/mod.rs`):** owns ONE D3D11 device for the process lifetime (every backend it builds — including WGC sessions rebuilt after a DDA interlude via new `WgcCapturer::new_on_device` — binds to it, so the shim never sees a foreign-device texture; `new_excluding` now delegates to it). `maybe_swap_backend()` (once per capture-loop iteration, two atomic loads steady-state): WGC+Secure⇒`swap_to_dda`, DDA+Default⇒`swap_to_wgc`, DDA+ACCESS_LOST while still Secure⇒in-place restore. 5 s cooldown after failed DDA activation (expected `E_ACCESSDENIED` until 2c ⇒ stays on WGC, client sees last frame frozen = exactly pre-Phase-2 behaviour, never worse), 1 s cooldown on WGC-restore races. `rebind()` records target/is_hdr (swap-back memory) and routes: interactive desktop ⇒ always lands on WGC (heals a stale DDA latch); secure desktop ⇒ retargets the live duplication.
- **lib.rs migrated to `DesktopManager`** (trait `DesktopCapture` in scope): all `capturer.width/height/origin_x/origin_y/device` field accesses → accessor methods; swap check in the frame loop gated on `client_connected` — on swap: resized ⇒ `recreate_encoder_for_capture` (extracted from `rebind_capture_and_encoder`, shared), same-size ⇒ `enc.request_idr()` so the client decodes from the first swapped frame; input rect re-synced either way.
- Zero warnings; 10/10 tests pass; release build clean.
- **Live validation (needs a stream):** UAC prompt mid-stream ⇒ expect `WGC → DDA` attempt, `E_ACCESSDENIED` + stay-on-WGC (until 2c), clean `DDA → WGC`-path no-op on dismissal; confirm no encoder glitch on prompt dismissal, and `/resume`+HDR sessions unaffected by the manager migration.

### 15.2c-impersonation — secure-desktop capture via thread impersonation (2026-07-10, LIVE-DIAGNOSED, capture-test pending)
- **CRITICAL FINDING (live):** running the whole host as SYSTEM-in-session (Sunshine's model) **breaks WGC** — `WgcCapturer::new` fails with `0x80070424` (ERROR_SERVICE_DOES_NOT_EXIST: WGC's WinRT/broker infra requires a real interactive USER, not SYSTEM). Sunshine gets away with SYSTEM because its primary backend is DDA; Nova's is WGC (for HDR). So host-as-SYSTEM is NOT viable for Nova. Confirmed: elevated USER token is ALSO denied `SetThreadDesktop(Winlogon)`/`DuplicateOutput` (E_ACCESSDENIED) — the secure desktop admits only SYSTEM.
- **Resolution — split identity:** host runs as the elevated USER (WGC/HDR/audio all work), and only the DDA capture thread assumes a SYSTEM **impersonation** token for the secure-desktop grab:
  - `service.rs`: host spawned with the elevated USER token (reverted from the SYSTEM-in-session attempt). Additionally `create_inheritable_system_token()` duplicates the service's own LocalSystem token as an **inheritable impersonation token** and passes its handle value to the host via `--system-token <n>` (child inherits the handle at the same value; `bInheritHandles=true`). `set_system_impersonation_token`/`system_impersonation_token` (AtomicIsize) store it host-side.
  - `bin`: `--system-token <n>` arm stashes the handle then runs the host normally (shared `run_host()`).
  - `lib.rs run()`: parses clap from a **filtered** arg list (strips `--system-token`+value) — clap aborts on unknown args otherwise.
  - `dda.rs`: `SecureDesktopGuard` (RAII) = `ImpersonateLoggedOnUser(system_token)` + `OpenInputDesktop`+`SetThreadDesktop(input desktop)`, held for the DDA session in `DdaCapturer.desktop_guard`; drop reverses both (reattach original desktop → close input desktop → `RevertToSelf`). `DdaCapturer::release()` drops the guard; `try_restore`/`rebind` release before rebuilding (no double-impersonation stack).
  - `mod.rs`: `swap_to_wgc` + the session-rebind DDA→WGC path call `d.release()` BEFORE `WgcCapturer::new_on_device` — WGC creation fails while the thread is impersonating SYSTEM / on the secure desktop.
- **Logging fix (load-bearing for diagnosis):** the service and host both opened `nova.log` with `FILE_SHARE_READ` only → the host got a sharing violation and ran with ALL logging silently discarded (invisible crash cause). Now: service → `nova-service.log` (`init_service_logger`), host → `nova.log`, both opened `FILE_SHARE_READ | FILE_SHARE_WRITE`.
- **Self-kill fix:** `--install-service`/`--install`/`--uninstall` ran `taskkill /F /IM nova-server.exe` which killed the installing process itself (install self-terminated before `CreateServiceW`). Now `kill_other_nova_instances()` = `taskkill … /FI "PID ne <self>"`.
- **Live iteration (2026-07-10), three sequential blockers found + fixed:**
  1. Host-as-SYSTEM breaks WGC (0x80070424) → host runs as elevated USER, DDA thread impersonates SYSTEM (above).
  2. Session-0 SYSTEM token denied `OpenInputDesktop`(secure) → `create_inheritable_system_token(session_id)` now `SetTokenInformation(TokenSessionId)`-retargets the token to the console session (SYSTEM-in-session-N), matching Sunshine. After this, OpenInputDesktop SUCCEEDS under impersonation.
  3. `SetThreadDesktop(Winlogon)` failed `0x800700AA` ERROR_BUSY — the main capture thread has windows/hooks (COM/WGC message windows), and SetThreadDesktop refuses any thread with windows. **Fix: dedicated capture thread.**
- **Final DDA architecture (`dda.rs` rewritten, dedicated-thread model):** `DdaCapturer::new` spawns thread `nova-dda-secure`. That FRESH thread (no windows ⇒ SetThreadDesktop works): `ImpersonateLoggedOnUser(system_token)` → `OpenInputDesktop`+`SetThreadDesktop(Winlogon)` → creates its OWN D3D11 device on the output's adapter → `DuplicateOutput1` → acquire loop copying each frame into a CPU staging buffer → shared `Mutex<Option<CpuFrame>>`. The MAIN thread's `try_get_frame` `take`s the CPU frame and `UpdateSubresource`s it into an encoder-device cache texture (only the main thread ever touches the encoder device context — no cross-thread D3D). Thread exit auto-releases impersonation + desktop association, so the main thread's identity/desktop are NEVER touched and WGC is unaffected. `new()` blocks ≤3 s for the thread to report duplication-created (Ok geometry) or Err (→ manager cooldown, stays WGC). `release()` = stop+join; `mod.rs` swap-back calls it before building WGC. Also fixed: clap in `run()` parses a filtered arg list (strips `--system-token`); logger split (service→nova-service.log, host→nova.log, both FILE_SHARE_READ|WRITE); `kill_other_nova_instances()` self-exclude.
- **CONFIRMED WORKING LIVE (2026-07-10):** stream + Ctrl+Alt+Del → the Windows secure screen is VISIBLE on the Moonlight client. Log shows `🔐 DDA capture thread: impersonating SYSTEM=true, attached to input desktop=true` → `✅ DDA duplication active` → `🔀 WGC → DDA (secure desktop active)` → clean `🔀 DDA → WGC (interactive desktop restored)` on dismissal. NovaService set back to AUTO_START. Phase 15 secure-desktop capture is DONE.
- **Tray "Quit" under the service (fixed):** the service respawns the host on exit by design, so a user Quit would just relaunch. Fix: the tray-Quit path (`lib.rs` `shutdown_rx` arm) calls `service::request_service_stop()` (`sc stop NovaService`) BEFORE its graceful teardown, so the worker won't respawn; the service worker then grace-waits `HOST_GRACEFUL_EXIT_MS` (6 s) for the host to finish its own display/audio teardown before force-terminating. No-op when not launched by the service.

### 15.2c — thin SYSTEM launcher service (launcher plumbing) — DONE (2026-07-09), not yet live-validated
- **`src/service.rs`** — no separate binary; the service is a MODE of `nova-server.exe` (smaller footprint, no duplicated deps). Subcommands (bin/nova-server.rs): `--service` (SCM dispatcher entry), `--install-service`, `--uninstall-service`. Hand-rolled with the `windows` crate (consistent with the rest of the codebase, zero new crates).
  - **SCM plumbing:** `StartServiceCtrlDispatcherW` → `service_main` (registers `handler_ex`, creates stop[manual-reset]/wake[auto-reset] events, reports START_PENDING→RUNNING, runs worker, reports STOPPED). Control handler accepts STOP/SHUTDOWN (⇒ signal stop) + SESSIONCHANGE (⇒ wake). Global SCM state (status handle, event handles) in `AtomicIsize` because the SCM callbacks are bare fn-pointers with no owned user-data slot.
  - **Worker** keeps exactly ONE host alive in the active console session: `WaitForMultipleObjects([stop, wake], 2000ms)` reconcile loop — spawns if none/exited; on console-session change (fast user switch / RDP, `WTSGetActiveConsoleSessionId` differs) terminates the old-session host and respawns in the new one; on stop, terminates the host.
  - **Token/spawn (the whole point):** `WTSQueryUserToken(session)` → filtered user token → `GetTokenInformation(TokenElevationType)`; if `TokenElevationTypeLimited`, `TokenLinkedToken` → full elevated token → `DuplicateTokenEx(TokenPrimary)` → `CreateEnvironmentBlock` (user env) → `CreateProcessAsUserW` with `lpDesktop="WinSta0\\Default"`, `CREATE_UNICODE_ENVIRONMENT`. Using the elevated linked token means the requireAdministrator host starts with NO UAC prompt, matching the task's HighestAvailable. RAII `HandleGuard`/`ScHandleGuard` close every token/SC handle.
  - `install_service` registers LocalSystem + AUTO_START and **removes the scheduled task first** (the two must never both spawn a host); idempotent on ERROR_SERVICE_EXISTS. `uninstall_service` stops+deletes, idempotent.
- **Idempotent install (upgrade-safe):** on `ERROR_SERVICE_EXISTS`, `install_service` updates the binary path in place via `ChangeServiceConfigW` (race-free — no delete/recreate churn), so a reinstall to a different directory re-points correctly. `--install-service` also runs the Ghost Protocol stale-DLL purge (parity with the task installer).

### 15.2c installer migration — DONE (2026-07-09): service is now the default deployment
- **`nova.iss` migrated** from scheduled-task to service:
  - `[Run]`: devcon install → `nova --install-service` (registers NovaService, removes the task) → `sc start NovaService` (starts it now; the service spawns the host into the installer's own console session — exercises the real production path, not a one-off direct launch).
  - `[UninstallRun]`: `--uninstall-service` (stop+delete) → `--uninstall` (task belt-and-suspenders for upgraded/fallback boxes) → taskkill → devcon remove.
  - **`[Code] PrepareToInstall` upgrade guard** (runs before `[Files]`): `sc stop NovaService` + `schtasks /end` + `taskkill` + 1.5 s settle, so the running host releases its lock on nova-server.exe / nova_shim.dll before the copy — without this, upgrades hit the "files in use / reboot" path.
- **Task path is the documented fallback**, still fully functional in the binary (`--install`/`--uninstall`) for environments that don't want a service — but it does NOT grant secure-desktop capture (no SYSTEM token). Recorded in the `.iss` header comment.
- `cargo check` clean (lib+bin); installer not compiled here (needs Inno Setup on the build box).
- New `windows` features (bindings only): `Win32_System_Services`, `Win32_System_RemoteDesktop`, `Win32_System_Environment`.
- `cargo check` (lib+bin) clean, 10/10 tests, release build clean.
- **CRITICAL live-validation items (target hardware):** (1) does the elevated user token actually permit `SetThreadDesktop(Winlogon)` for real DDA secure-desktop capture? If not, the refinement is to run the CAPTURE THREAD under the service's SYSTEM token (host stays user-session for DWM/WGC). (2) `CreateProcessAsUserW` may need `SeAssignPrimaryTokenPrivilege`/`SeIncreaseQuotaPrivilege` explicitly enabled on the service token — add if it returns ERROR_PRIVILEGE_NOT_HELD. (3) host graceful shutdown on service STOP currently uses `TerminateProcess` (the host self-heals display on next boot + its own OS-shutdown hooks fire on real shutdowns; a cross-session graceful signal is a possible refinement). (4) consider a single-instance named-mutex guard in the host so task+service overlap can't double-launch.
- **VDD + secure desktop note (from 15.2a design):** in true-headless the VDD is the console primary and the secure desktop renders on it — DDA duplicates the VDD output (cross-adapter path). With physical displays active, DDA duplicates the physical primary. Both degrade gracefully; document exact behaviour after live validation.

### Edge cases to keep in view (Phase 2 testing)
Fast user switching, multi-monitor primary selection, device removal DURING a desktop switch, VDD-active-vs-secure-desktop mismatch, and the `/resume` zombie-session overlap (already handled for control/video via session generations — audio must join that model).

---

## Phase 14 — Per-client cert trust, ghost-monitor cleanup, emergency display restore (2026-07-06)

Phase 14 closes three architectural lifecycle holes (Apollo-parity, referenced against `C:\Apollo-0.4.6\src\nvhttp.cpp` / `main.cpp`). Not yet live-validated with a Moonlight client — all paired devices must RE-PAIR (nova_paired.json entries without a cert are dropped at load).

### 14.1 — Strict per-client pairing (`src/pairing.rs`)
- **Bug:** pairing state was keyed by `uniqueid` alone and HTTPS 47984 did no client-cert auth (`with_no_client_auth`). moonlight-qt and derived clients hardcode `uniqueid=0123456789ABCDEF`, so once ANY device paired, every Moonlight client appeared paired ("global open"), and all devices resolved to one stored name.
- **Fix — the client TLS certificate is now the device identity:**
  - `nova_paired.json` v2: keyed by SHA-256 fingerprint of the client cert DER — `{ "<fp>": { "name", "uniqueid", "cert": "<hex-PEM>" } }`. Legacy cert-less entries dropped at load with a re-pair warning; fingerprints recomputed from the stored cert at load (a hand-edited key cannot remap trust).
  - 47984 now REQUIRES a client cert (`AcceptAnyClientCert` verifier = Sunshine's `SSL_VERIFY_PEER|SSL_VERIFY_FAIL_IF_NO_PEER_CERT`): any self-signed cert passes the handshake, but the TLS CertificateVerify signature is verified for real (key possession), then the accept loop matches the peer cert fingerprint against the trust store → per-connection `VerifiedClient` (Apollo `get_verified_cert`). Unmatched ⇒ every request on that connection is 401 XML ("The client is not authorized").
  - Endpoint gating: HTTP 47989 serves ONLY `/serverinfo` (limited: PairStatus=0, currentgame=0), `/pair`, `/ping`; all else 404. `/applist`, `/appasset`, `/launch`, `/resume`, `/cancel`, `/unpair` are HTTPS+verified only. `/unpair` removes the REQUESTING device's own cert entry (uniqueid-keyed unpair would let one client unpair everyone).
  - Pairing handshake hardened to Apollo's `fail_pair` model: phase-order enforcement (out-of-order kills the session), `clientcert` required at getservercert, and BOTH final MITM checks implemented: `same_hash` (SHA-256(serverchallenge‖client-cert-sig‖secret) vs the hash committed in serverchallengeresp) and RSA-PKCS1-SHA256 verification of the secret signature via `rustls-webpki` `EndEntityCert::verify_signature` (Apollo `crypto::verify256`). Wrong PIN / MITM ⇒ `paired=0`, nothing persisted.
  - New direct dep `rustls-webpki` (already in tree via rustls — zero new compiled code).
  - Regression tests `pairing::tests`: base64↔PEM round-trip, hex-PEM→DER, store round-trip + legacy migration + tampered-key healing.

### 14.2 — Device identity + phantom monitor cleanup (`src/pairing.rs`, `src/virtual_display.rs`)
- `/launch`/`/resume` take `device_name` from the connection's verified cert — never from the shared uniqueid — so each device's virtual-monitor rename shows ITS pairing name (Pixel vs Hisense fixed).
- **Ghost monitors:** every `DICS_ENABLE` cycle spawns a fresh `MONITOR\MTT1337` monitor child devnode; the disable leaves the old one behind as a hidden non-present ("phantom") device — verified live on this box via `Get-PnpDevice -Class Monitor`. New `cleanup_phantom_monitors()`: enumerates monitor-class devnodes WITHOUT `DIGCF_PRESENT`, filters hardware-ID == `MONITOR\MTT1337`, presence-checks via `CM_Get_DevNode_Status` (phantom ⇒ ≠ CR_SUCCESS), removes via `SP_REMOVEDEVICE_PARAMS` + `DIF_REMOVE` (the devcon removePhantoms dance). Physical monitors' phantom entries are never touched. Runs in `deactivate_after_stream` (after the devnode disable) and as a boot sweep in `ensure_enabled_at_boot`. (Apollo avoids this class of bug because SudoVDA destroys its monitor object per session; MttVDD needs the SetupAPI sweep.)

### 14.3 — Emergency display restore on process death (`src/virtual_display.rs`, new `src/shutdown.rs`)
- **Bug:** tokio's console-ctrl handlers cover console paths (its handler parks the thread for CLOSE/LOGOFF/SHUTDOWN so `run()`'s teardown can finish), but the tray thread owns windows ⇒ on logoff/shutdown Windows delivers WM_QUERYENDSESSION/WM_ENDSESSION and may TERMINATE the process as soon as its windows answer — before any teardown runs. Result: headless topology stuck in the CCD DB, black physical monitor until Phase 13.2's boot healing repairs it at the NEXT boot.
- **Fix (Sunshine `SessionMonitorWindowProc` + `ConsoleCtrlHandler` parity — both funnel into one synchronous, claim-once restore):**
  - `EMERGENCY_SNAPSHOT` static (saved topology + VDD GDI name + audio endpoint), armed by `activate_for_stream`, disarmed by `deactivate_after_stream`. `virtual_display::emergency_restore_for_shutdown()`: `restore_topology` (error-87 path falls back to `SDC_FORCE_MODE_ENUMERATION`) → 250 ms DWM settle → `DICS_DISABLE` devnode → restore default audio endpoint. Idempotent (`Mutex<Option>::take` claim); the graceful teardown skips its own restore if the emergency already ran (`EMERGENCY_FIRED`).
  - `shutdown::install_console_hook()` — registered AFTER tokio's watchers (handlers run LIFO ⇒ ours first): CLOSE/LOGOFF/SHUTDOWN ⇒ synchronous emergency restore, then chain to tokio's handler so the graceful teardown also runs. Plain CTRL_C passes through untouched.
  - `shutdown::spawn_session_monitor()` — dedicated thread owning an invisible top-level window (`NovaSessionMonitorClass`; NOT message-only — HWND_MESSAGE windows never receive ENDSESSION), `SetProcessShutdownParameters(0x100, SHUTDOWN_NORETRY)` (low level ⇒ notified after ordinary apps, so the restore is the last word on topology). WM_ENDSESSION(wParam=TRUE) ⇒ blocking emergency restore before returning.

### 14.4 — HDR10 + 120 Hz negotiation fixes (2026-07-08) — CONFIRMED WORKING live (Xbox 4K@120 HEVC Main10/HDR10)
- **HDR bug:** `ServerCodecModeSupport=259` (0x103) was built on a wrong SCM bit map. Correct map (moonlight-common-c Limelight.h): H264=0x1, HEVC **Main8**=0x100, HEVC **Main10**=**0x200** (0x2 = H264_HIGH8_444, unsupported). With no 0x200 bit, moonlight-common-c NEVER sets `dynamicRangeMode:1` in ANNOUNCE — every client silently declined HDR (live log: `/launch hdrMode=1` + `clientSupportHevc:1` but `dynamicRangeMode:0`). Fix: advertise **0x301** (lib.rs `codec_mode_support`). Phase 14.1's HTTPS/serverinfo path forwards it unchanged.
- **120 Hz bug:** `force_resolution` set `targetInfo.refreshRate={120000,1000}` but left `targetInfo.Anonymous.modeInfoIdx` pointing at the OLD target-mode entry (60 Hz videoSignalInfo). With `SDC_ALLOW_CHANGES`, Windows silently kept 60 Hz while returning success — log claimed `@120Hz` but live CIM query showed the VDD at 4K@60, so WGC delivered max 60 unique fps into the "120fps" stream. Fix: invalidate the target-mode index (`0xffffffff` = DISPLAYCONFIG_PATH_MODE_IDX_INVALID, Sunshine libdisplaydevice pattern) so SetDisplayConfig derives a fresh target mode honoring the path's refreshRate; new `query_ccd_target_refresh()` reads back the COMMITTED refresh after apply and logs `(committed NHz)` / a ⚠️ on mismatch — no more false-success.
- **Log fix:** the connect-time `📐 Encoder:` line printed `enc_name` captured BEFORE the ANNOUNCE-driven codec switch (showed "h264" for actual-HEVC sessions). Now prints the live `enc.config.codec`.
- Note: Xbox Moonlight (moonlight-xbox-dx) on this network reports `clientSupportHevc:1` and decodes 4K@120 HEVC Main10 fine — the old "1.18.0 has no HEVC" note below is stale for this device (kept for the H264 Level 5.2 cap rationale).

### Task 3/Task 4 audit result (no code change needed)
- **HDR10 auto-activation on `/launch hdrMode=1`** — already implemented (Phase 13.1): pre-activation `force_hdr_reconnect_cycle()` + 2 s FP16 settle + FP16 WGC rebind during the /launch→PLAY gap; P010/HEVC-Main10 NVENC recreated when ANNOUNCE confirms `dynamicRangeMode=1`. The historical "starts SDR until the user toggles HDR" symptom was the missing installer elevation (fixed in 13.1).
- **Clean boot hook** — `--install` already registers exactly one `NovaServerBoot` scheduled task via Task XML (`InteractiveToken` ⇒ Session 1, `HighestAvailable`, logon trigger + 5 s delay) and sweeps legacy task names; the only registry write (VDDPATH) is read-first/write-only-if-different — no per-run registry spam exists.

---

## Phase 13 — /resume + frameIndex fixes (2026-07-03/05)

Phase 13 fixes (a) the "black screen → ~10 s → Moonlight says reduce your bitrate" failure that became 100% reproducible with the release build on a clean network — **confirmed fixed, streaming works** — and (b) /resume kicking the client back to the app list when Moonlight was quit without disconnecting (Xbox behavior).

### Phase 13.2 — Boot VDD isolation "error 87" fix (2026-07-05):

**Symptom:** on a fresh install / first boot Nova took over the physical desktop and ran headless immediately, before App 5 was ever launched (blank host screen; you could still pair blind over Moonlight). The boot log showed `Atomic VDD isolate+restore failed (… error 87) — falling back to deactivate-only` immediately followed by `ccd_deactivate_vdd_path also failed (… error 87)` — so the VDD was never removed from the active topology at boot and stayed the primary display.

**Root cause:** when the CCD database has the persisted "true headless" topology saved from a previous stream (VDD primary, physical paths inactive — the state an unclean shutdown mid-stream leaves behind), the devnode-enable at boot restores THAT topology, making the VDD the only active display. Both `ccd_isolate_vdd_and_restore_primary` and `ccd_deactivate_vdd_path` then tried to deactivate the VDD's path while it was the *sole* active path → a supplied config with zero active displays, which `SetDisplayConfig` rejects with `ERROR_INVALID_PARAMETER` (87). They also queried `QDC_ALL_PATHS`, whose per-(source×target)-permutation entries are independently 87-prone.

**Fix (`src/virtual_display.rs`):** both isolate helpers now query `QDC_ONLY_ACTIVE_PATHS` (the exact committed topology — round-trips reliably, same as `force_resolution`), and when the VDD is detected as the only active display they first re-light the physical outputs via new `extend_topology_and_wait_for_physical` (`SDC_TOPOLOGY_EXTEND`, then poll `query_active_topology` until a non-VDD active path appears, re-resolving the VDD's possibly-renumbered GDI name via `find_vdd_attached_to_desktop`), then deactivate the VDD path on the fresh topology and `SDC_SAVE_TO_DATABASE` the healed "physical primary, VDD inactive" state so the next boot starts clean. New `path_is_device` helper dedups the GDI-name match. **Confirmed 2026-07-05:** boot log now shows `\\.\DISPLAY9 dormant — physical display(s) restored to primary position`, no error 87, physical `\\.\DISPLAY1` remains primary at (0,0); VDD only activates on App 5.

### Phase 13.1 — Install & driver preflight (2026-07-05):

**Installer elevation fix (`nova.iss`):**
- **Root cause of "auto-runs without admin":** the final "Launch Nova now" `[Run]` entry used `postinstall` without `runascurrentuser` — Inno Setup deliberately runs postinstall entries as the ORIGINAL unelevated user. Unelevated, the VDD devnode enable (SetupAPI `DICS_ENABLE`) and HDR10 Advanced Color switching fail silently → no virtual monitor, no HDR, black stream. The HDR10-on-VDD auto-enable itself was already implemented (pre-activation + connect-time `set_active_display_hdr(true)` when the client's ANNOUNCE confirms `dynamicRangeMode=1`) — it was the missing elevation that broke it on installed copies.
- **Fix:** `runascurrentuser` added to that entry — Nova now inherits the installer's interactive admin token, matching the elevation the `NovaServerBoot` task provides at every logon. (The manifest embedded via build.rs already declares `requireAdministrator`; RT_MANIFEST is compiled into the exe by rc.exe, so manual launches also elevate.)

**Elevation guard (`src/lib.rs`):**
- Startup preflight logs "🛡️ Elevated token confirmed" or a loud ❌ + on-screen MessageBox (background thread, non-blocking) when running unelevated — an unelevated start can otherwise only fail silently with a black screen. Uses `IsUserAnAdmin()` (Win32_UI_Shell, already a dependency).

**ViGEmBus (virtual Xbox 360 controller) preflight (`src/input.rs`, `src/lib.rs`):**
- `input::check_vigem_driver_at_startup()` — background thread probes `vigem_client::Client::connect()`. If the driver is missing: Yes/No MessageBox offering to download + run the official installer (pinned `ViGEmBus_1.22.0_x64_x86_arm64.exe` from nefarius/ViGEmBus GitHub releases, via the same PowerShell `Invoke-WebRequest` pattern as the RetroArch bootstrap). Download failure falls back to opening the releases page in the browser.
- Declining writes `vigem_install_declined.flag` next to the exe so the logon-autostart doesn't nag every boot (delete the flag to be asked again). The missing driver is still logged each start.
- `GamepadManager` connects per-session, so a mid-run install works on the next stream without restarting Nova.

### Phase 13 changes (2026-07-03):

**Zombie-proof /resume (`src/control.rs`, `src/pairing.rs`, `src/rtsp.rs`):**
- **Symptom:** quit Moonlight on Xbox mid-stream (no ENet disconnect is ever sent), reopen, tap Resume on app 5 → full RTSP handshake succeeds but the client waits on a dead session and bails back to the app list after ~7 s. Quit-app + relaunch worked; resume never did.
- **Root cause (two layers, from the 7/3 log):**
  1. The old session's ENet control peer lingers as a zombie until its 10–30 s timeout, which lands right after the /resume PLAY; `handle_event`'s Disconnect arm indiscriminately set `streaming_active=false`, tearing down the freshly resumed session. With `peer_limit: 1` the new control connection also couldn't even land until the zombie died.
  2. Deeper: /resume never restarted the session state machine at all — lib.rs's session-start block is gated on `!client_connected`, which was still true from the zombie session, so the new rikey/codec/audio were never applied ("Moonlight connected" never fired).
- **Fix:**
  - `ClientInfo.session_generation: u64` — bumped by every /launch **and** /resume (pairing.rs).
  - /launch and /resume now both arm the session with `streaming_active=false` (until PLAY) and reset `cancelled`/`hdr_mode_sent`/`dynamic_range_mode`/`bit_stream_format`. The capture loop therefore suspends a still-connected zombie session immediately and latches the new session cleanly at PLAY — new rikey, codec renegotiation, audio restart all run. Only /launch resets `activated` (resume reattaches to the live VDD with no topology flicker).
  - control.rs: `peer_limit: 2` so the resume's control connection lands instantly beside the zombie; every Connect stamps the peer with the current session generation and evicts all other peers via `Peer::reset()` (immediate slot free, no Disconnect event); the Disconnect arm ignores any peer whose stamp ≠ current generation ("stale peer — ignoring") — only the live session's peer can end the session.
- Also fixes the latent launch-over-zombie bug where pre-activation was skipped because `streaming_active` was still true from the dead session.

**RTP frameIndex must start at 1 (`src/rtp.rs`):**
- **Symptom:** full handshake succeeds, HEVC frames flow at the negotiated bitrate, but the client renders nothing, sends zero loss-stats and zero IDR re-requests, and terminates after ~10 s with `ML_ERROR_NO_VIDEO_FRAME` ("Your network connection isn't performing well. Reduce your video bitrate…").
- **Root cause:** `RtpSender.frame_index` started at 0. moonlight-common-c (`VideoDepacketizer.c`) initializes `nextFrameNumber = 1` and discards any packet with `isBefore32(frameIndex, nextFrameNumber)` — so Nova's session-start forced IDR (frame 0) was **always** discarded by every Moonlight client. Subsequent P-frames are dropped ("Waiting for IDR frame"), and the client only calls `LiRequestIdrFrame()` when `waitingForNextSuccessfulFrame` is also set — which requires a mid-frame packet loss. On a loss-free link the recovery never fires → permanent black screen.
- **Why it ever "worked":** every previously working session (incl. the Phase 12 validation on 7/2) started only because early WiFi packet loss tripped the client's recovery IDR request (visible in the 7/2 debug log as a second "client requested IDR frame" ~350 ms in). The slower debug-build pacing made loss likely; the release build's clean delivery removed the loss and exposed the bug deterministically.
- **Fix:** `frame_index` starts at 1 in `RtpSender::new()` and `reset()` — Sunshine parity (`video.cpp: int frame_nr = 1`). Regression test `first_frame_carries_frame_index_1_and_reset_restarts_at_1` locks the wire format (first frame = index 1, restarts at 1 after session reset).
- Also fixed: `#[cfg(test)]` GDI import list was missing `CDS_SET_PRIMARY` + `DM_POSITION` — `cargo test` had been broken since the Phase 12 import cleanup.
- **Status: CONFIRMED WORKING 2026-07-03** — user reports streaming works perfectly (Xbox 4K@120 H264/SDR and Android 720p HEVC sessions in the log).

## Phase 12 complete — IddCx CCD-Native VDD Resolution Fix (2026-07-02)

All previous phases (1–11) confirmed working. Phase 12 fixes VDD resolution not snapping to client-requested dimensions when using the MttVDD IddCx driver (resolution was stuck at native 2560×1440 regardless of Moonlight's requested mode).

### Phase 12 changes (2026-07-02):

**CCD-native VDD resolution (`src/virtual_display.rs`):**
- **Root cause:** MttVDD is an IddCx driver. `ChangeDisplaySettingsExW` always returns `DISP_CHANGE_FAILED (-1)` on IddCx; `EnumDisplaySettingsW(ENUM_CURRENT_SETTINGS)` always returns 0×0. All legacy GDI mode-set APIs are no-ops against IddCx.
- **`force_resolution` rewritten** to use `QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS)` + `SetDisplayConfig(SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_APPLY | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE)`. Modifies `DISPLAYCONFIG_SOURCE_MODE.width/height` and `DISPLAYCONFIG_PATH_TARGET_INFO.refreshRate` in-place before committing. Apollo-pattern refresh rate formula: `{Numerator: refresh_hz * 1000, Denominator: 1000}`.
- **`wait_for_display_resolution` rewritten** to poll `query_ccd_source_size` (CCD) instead of `EnumDisplaySettingsW` (broken for IddCx). Times out after 3 s, proceeds anyway with a warning.
- **`query_ccd_source_size` new helper:** scans `QDC_ONLY_ACTIVE_PATHS` for the named GDI device, matches the source mode entry by adapter LUID + source ID, returns `(width, height)`.
- **SDC_TOPOLOGY_EXTEND settle loop fixed:** was polling `EnumDisplaySettingsW` (always 0×0 on IddCx). Now polls `find_vdd_attached_to_desktop()` (`DISPLAY_DEVICE_ATTACHED_TO_DESKTOP` flag via `EnumDisplayDevicesW`) — set by DWM exactly when the device is live in the active topology.
- **GDI imports moved to `#[cfg(test)]`:** `ChangeDisplaySettingsExW`, `EnumDisplaySettingsW`, `CDS_*`, `DEVMODEW`, `ENUM_CURRENT_SETTINGS` — no longer used in production code path. Zero unused-import warnings.
- **Confirmed working** (2026-07-02): VDD snaps to 1280×720@60Hz, NVENC rebinds at 720p, HEVC stream at 7.5 Mbps client-negotiated, video loads in Moonlight without "reduce bitrate" error.

**Known remaining issues:**
- ~~`ccd_isolate_vdd_and_restore_primary error 87`~~ — **fixed in Phase 13.2** (2026-07-05). Root cause was querying `QDC_ALL_PATHS` and trying to deactivate the VDD while it was the sole active display; both isolate paths now use `QDC_ONLY_ACTIVE_PATHS` and re-light physical outputs via `SDC_TOPOLOGY_EXTEND` first. See the Phase 13.2 section at the top.

---

All previous phases (1–10) confirmed working. Phase 11 delivers static-desktop Video Encode flatline (0% GPU utilisation matching Apollo/Sunshine), per-frame heap elimination in the RTP hot path, MMCSS audio scheduling, process power-throttling exemption, DSCP EF socket tagging, dynamic HDR luminance config, and thin-LTO binary hardening.

### Phase 11 changes (2026-06-25):

**Static-frame gate + IDR keep-alive (`src/lib.rs`):**
- `None =>` WGC branch no longer re-submits cached texture to NVENC every frame interval. NVENC hardware-idle on a static desktop → **0% Video Encode** in Task Manager, matching Apollo/Sunshine's flatline signature.
- `IDR_KEEPALIVE_INTERVAL = 1000 ms`: when the screen has been static, one forced IDR pulse per second keeps Moonlight's connection watchdog alive without engaging the encode engine.
- Gate: `client_connected && video_learned` — no encoding while no client is receiving.
- WGC `None` log spam reduced to first occurrence + every 300 frames (~5 s).

**`shim.cpp` hot-loop GetDesc() elimination:**
- `g_encWidth` / `g_encHeight` / `g_captureFmt` cached once in `InitColorConversion`, reset in `CleanupEncoder`. Eliminates per-frame COM `GetDesc()` round-trip from `EncodeFrame`. Removed the now-unused `vpSourceTexture` local.

**RTP shard-pool pre-allocation (`src/rtp.rs`):**
- `stream_buf: Vec<u8>` and `shard_pool: Vec<Vec<u8>>` added to `RtpSender` struct. Grow to session high-watermark and are reused/zeroed every frame. Eliminates ~36 `Vec::new()` + dealloc cycles per frame at 60–120 Hz.
- `send_packet` converted from method to free function to allow clean split-borrow access to `socket` and `shard_pool` simultaneously.
- Socket SO_SNDBUF raised from 4 MB to 8 MB (covers worst-case 4K IDR burst).

**MMCSS Pro Audio (`src/audio.rs`):**
- `AvSetMmThreadCharacteristicsW("Pro Audio")` registered on the WASAPI loopback capture thread immediately after `SetThreadPriority(TIME_CRITICAL)`. Matches Apollo/Sunshine. Elevates scheduler quantum and protects the audio thread from background preemption without REALTIME privilege.

**Process power-throttling exemption (`src/lib.rs`):**
- `SetProcessInformation(ProcessPowerThrottling, {ControlMask=1, StateMask=0})` at startup. Disables Windows 11 Efficiency Mode for the nova-server process — prevents E-core scheduling and CPU power-capping during active streaming.

**DSCP EF socket tagging (`src/rtp.rs`, `src/lib.rs`):**
- `socket2::set_tos(0xB8)` (DSCP EF = 101110 00, Expedited Forwarding) applied to both the video RTP UDP socket (port 47998) and the audio UDP socket (port 48000). Best-effort prioritisation honoured by DSCP-aware managed switches and Windows QoS Group Policy rules.

**Dynamic HDR luminance from `nova.toml` (`src/config.rs`, `src/encoder.rs`, `shim/shim.cpp`):**
- New `[hdr]` table in `nova.toml`: `max_luminance_nits` (default 1000), `max_cll_nits` (default 1000), `max_fall_nits` (default 400). BT.2020 primaries are standard constants; only luminance varies per panel.
- `encoder::set_hdr_metadata()` → `SetHdrMetadata()` FFI → `BuildHdrMetadata()` uses globals. Call injected in `lib.rs` immediately after `NovaConfig::load()`, before the first `Encoder::new()`.
- Operators can now tune HDR SEI to match their TV's actual spec (HDR600 / HDR1000 / HDR2000).

**Cargo release profile (`Cargo.toml`):**
- `[profile.release]`: `lto = "thin"`, `codegen-units = 1`, `strip = "symbols"`. Thin LTO gives ~90% of fat-LTO runtime benefit with ~10% of the link-time cost. Binary: **7.76 MB** exe + **0.08 MB** DLL.

---

### Working end-to-end (confirmed):
- Pairing (RSA/AES-ECB). **Critical:** `plaincert` must hex-encode the **PEM** bytes (not DER).
- RTSP handshake (port 48010), ENet control (UDP 47999), H.264 RTP + RS-FEC (UDP 47998), WASAPI→Opus audio (UDP 48000), mouse/keyboard/gamepad input, cursor compositing.
- **Universal VDD (all apps):** every Moonlight app routes through the Virtual Display Driver. Controlled by `nova.toml → headless_for_all_apps` (default `true`). Set `false` to restrict headless mode to App 5 only.
- **VDD hardware-disabled at boot (Phase 10):** `DICS_DISABLE` via SetupAPI leaves the devnode `CM_PROB_DISABLED` — invisible to DXGI, CCD, and PnP. Cannot steal primary on a graphics-stack crash or Safe Mode reboot. `activate_for_stream` calls `DICS_ENABLE` on client connect; `deactivate_after_stream` calls `DICS_DISABLE` on disconnect. `ensure_enabled_at_boot` cycles the devnode once to flush `vdd_settings.xml`, then disables it. CCD guard (`ccd_deactivate_vdd_path`) fires immediately after the devnode appears in GDI to prevent arrival-order primary hijack before `set_primary_display` runs.
- **Dynamic monitor naming:** after `activate_for_stream`, `SetupDiSetDeviceRegistryPropertyW(SPDRP_FRIENDLYNAME)` renames the VDD devnode to the connected client's paired name (e.g. "Xbox"), visible in Device Manager and Display Settings.
- **HDR10 pipeline:** WGC FP16 scRGB → typed-RTV pixel shaders → P010 BT.2020 PQ → HEVC Main10 NVENC. SEI (MDCV type 137 + MaxCLL type 144) injected manually via `seiPayloadArray`. VUI: BT.2020 / SMPTE ST 2084 / NCL / full-range.
- **Known limit:** Xbox Moonlight 1.18.0 reports `x-nv-clientSupportHevc:0`; H.264 decoder crashes at 4K@120fps (Level 5.2). Use 1080p@60fps or 1080p@120fps on Xbox.

---

### Phase 9 fixes (2026-06-23):

**Graceful shutdown / Dead Man's Switch:**
- `impl Drop for VirtualDisplay` — on any exit path (Ctrl+C, OS shutdown, logoff, panic), `deactivate_after_stream()` fires automatically, restoring physical monitors before the process dies.
- Explicit ordered teardown at the end of `run()`: `enc.cleanup()` → `vd.deactivate_after_stream()` → function returns. D3D texture references are freed before the VDD's CCD path is torn down.
- OS signals already handled via `tokio::signal::windows::ctrl_close/ctrl_shutdown/ctrl_logoff`.

**VDD boot isolation fix:**
- **Root cause:** `isolate_virtual_display_at_boot` used `ChangeDisplaySettingsExW(0×0)` which MttVDD rejects with `DISP_CHANGE_BADMODE (-2)`. The early-return meant the VDD stayed active in the CCD topology, was saved to the database, and became a monitor on every reboot.
- **Fix:** `ccd_deactivate_vdd_path()` — queries `QDC_ALL_PATHS`, clears `DISPLAYCONFIG_PATH_ACTIVE` on the VDD's path entry, applies with `SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_SAVE_TO_DATABASE | SDC_ALLOW_CHANGES`. Mirrors the proven `deactivate_other_paths` pattern.

**Performance sweep (camera-pan stutter):**
- **REL mouse input:** `inject_mouse_move_rel` now sends raw wire deltas as `MOUSEEVENTF_MOVE` (no ABSOLUTE flag). The old path called `GetCursorPos` + 4×`GetSystemMetrics` per packet — 5 kernel transitions at 100–200 Hz during a camera pan. Games read relative input via `WM_INPUT / GetRawInputData`, not absolute cursor position.
- **RTP pacing:** `PACE_GAP` is now `300µs × (60 / fps)` — at 120fps it halves to 150µs, keeping total pacing overhead under 10% of the 8.33ms frame budget for large IDR frames.
- **Socket buffer:** UDP send buffer raised from 2MB to 4MB.
- **WGC miss sleep:** reduced from 2ms to 1ms in the `try_get_frame` None branch.

**`nova.toml` runtime config:**
- `serde` + `toml` crates added. `src/config.rs` defines `NovaConfig` with `StreamConfig`, `AudioConfig`, `NetworkConfig`.
- `nova.toml` auto-generated in the exe directory on first run with all defaults documented inline.
- Priority chain: **CLI arg → nova.toml → built-in default**. All `--width/--height/--bitrate/--codec/--fps/--fec` args now override config rather than hardcode defaults.
- Key fields: `bitrate_kbps`, `fps`, `codec`, `enable_hdr`, `headless_for_all_apps`, `fec_percentage`, `audio.endpoint_override`.
- `enable_hdr = true` bypasses `is_advanced_color_supported()` check — useful when HDRPlus is set in `vdd_settings.xml` but the CCD query is slow.

**Dynamic monitor naming:**
- `/launch` handler dumps all Moonlight parameters to `nova.log` (rikey redacted) for diagnostics.
- `uniqueid` from `/launch` is looked up in `nova_paired.json` to resolve the device's friendly name.
- `ClientInfo.device_name` carries the name through to `lib.rs`.
- `VirtualDisplay::rename_devnode(name)` calls `SetupDiSetDeviceRegistryPropertyW(SPDRP_FRIENDLYNAME)` after `activate_for_stream` succeeds.

**Headless mode toggle (`nova.toml`):**
- `headless_for_all_apps = true` (default) — all apps route through VDD.
- `headless_for_all_apps = false` — only App 5 activates headless; other apps stream the physical primary.
- `app_launcher::uses_virtual_display(app_id, headless_for_all)` implements the gate in both the pre-activation and connect-time paths.

**Installer — `nova.iss` (Inno Setup) + `nova-server.exe --install`:**
- `nova.iss` at project root. Bundles `nova-server.exe`, `nova_shim.dll`, and the full `VirtualDisplayDriver\` package.
- **`[Run]` step 1:** `devcon.exe install MttVDD.inf Root\MttVDD` — runs under the installer's live admin token. `WorkingDir` set to the INF directory so Windows PnP resolves `MttVDD.dll` and `mttvdd.cat`.
- **`[Run]` step 2:** `nova-server.exe --install` — registers the **`NovaServerBoot`** scheduled task via `schtasks /create /xml` with `<LogonType>InteractiveToken</LogonType>` + `<RunLevel>HighestAvailable</RunLevel>` + 5-second startup delay. Task runs in Session 1+ (never Session 0). Migrates/removes legacy task names. All child processes use `CREATE_NO_WINDOW`.
- **`[Run]` step 3:** `nova-server.exe` — launches Nova for this session.
- Build pre-requisite: copy `C:\VDD.Control.25.7.23\` → `<project root>\VirtualDisplayDriver\` before compiling the installer.
- Architecture-aware: x64 and ARM64 paths handled via `Check: IsARM64`.

**Dead code cleanup:**
- `UpdateCursorShape` / `UpdateCursorPosition` FFI declarations and Rust wrappers removed — superseded by WGC `SetIsCursorCaptureEnabled(true)` cursor compositing.
- `Direct3D11CaptureFrame` unused import removed from `capture.rs`.
- `CDS_NORESET` / `CDS_TYPE` moved to `#[cfg(test)]` (only used in `#[ignore]`d diagnostic tests).

---

### Deployment checklist:
```
target/release/nova-server.exe   ← main binary
target/release/nova_shim.dll     ← C++ encoder shim (must be alongside .exe)
```
- `nova.toml` is auto-generated on first run — no manual copy needed.
- `nova.log` is written to the exe directory — tail it for diagnostics.
- `nova_paired.json` persists across restarts — per-device trust store keyed by client-cert SHA-256 fingerprint (name + uniqueid + hex-PEM cert). Deleting an entry (or the file) un-pairs the device(s).

### Inno Setup build steps:
```powershell
cargo build --release
Copy-Item -Recurse "C:\VDD.Control.25.7.23" ".\VirtualDisplayDriver"
# Open nova.iss in Inno Setup Compiler → Compile
# Output: Output\NovaSetup-0.1.0.exe
```

---

### Tray UX (current state):
- Right-click context menu:
  - **Pair Device** — auto-opens two-field dialog on pairing request (triggered during `getservercert`); user also can pre-open via tray menu.
  - **Quit Nova** — graceful shutdown via `watch::Sender<bool>`.
- `global_pin: Arc<Mutex<(String, String)>>` — tuple of (PIN, device_name).
- `TrayCmd::OpenPairDialog` — new command that opens the dialog proactively from `getservercert`.

---

### Phase 10 fixes (2026-06-25):

**VDD On-Demand Lifecycle:**
- `ensure_enabled_at_boot`: cycles devnode to flush XML, calls `isolate_virtual_display_at_boot` (CCD DB consistent), then `DICS_DISABLE` — fully hardware-dormant. Returns `None` so WGC capturer binds physical primary.
- `activate_for_stream`: `DICS_ENABLE` before `wait_for_virtual_display_device_name`; immediate `ccd_deactivate_vdd_path` guard after GDI name acquired — prevents arrival-order primary steal; `SDC_TOPOLOGY_EXTEND` re-adds VDD as secondary only.
- `deactivate_after_stream`: `DICS_DISABLE` after `restore_topology` — devnode hardware-dormant between sessions.
- `VirtualDisplay::drop()` calls `deactivate_after_stream()` (existing) → also disables devnode on graceful shutdown.

**NVENC Quality Fixes (shim.cpp):**
- Cached `g_compositeSRV` — per-frame `CreateShaderResourceView` alloc eliminated from hot loop.
- `enableFillerDataInsertion=1` for H264+HEVC — prevents CBR QP oscillation on static frames ("pulsing text").
- `intraRefreshPeriod=fps, intraRefreshCnt=fps` — continuous rolling refresh; no off-gap between cycles.

**Congestion Control (encoder.rs / control.rs / lib.rs):**
- `STREAM_BITRATE_KBPS` + `CONGESTION_BITRATE_KBPS` atomics; `signal_congestion_reduction()` fires on `PT_LOSS_STATS` loss>0.
- Main loop: 2s cooldown, 20% cut on loss, 10%/5s ramp-back. `set_stream_bitrate_kbps()` tracks current CBR target.

**Thread Priority (lib.rs / audio.rs):**
- `SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL)` on capture/encode thread + both audio threads.

**WGC Stale-HMONITOR Fix (capture.rs):**
- `new_excluding()` outer retry re-resolves HMONITOR on each attempt — fixes E_INVALIDARG after VDD devnode topology cycle at boot.

**Crash-to-exit Hardening (lib.rs / nova-server.rs):**
- WGC and NVENC init failures now propagate via `?` instead of `.expect()` panic — `run()` returns `Err`, main exits with code 1.

### Phase 11 candidates:
- HDR10 colour verification on Android Moonlight (HEVC Main10 + TV)
- AV1 end-to-end test (advertised in `ServerCodecModeSupport`, shim implemented, not yet confirmed live)
- Xbox HEVC: currently reports `x-nv-clientSupportHevc:0` in v1.18.0 — investigate
- `audio.endpoint_override` in `nova.toml` wired into WASAPI pipeline (`audio.rs`)
- Monitor rename visible in Display Settings via monitor child-devnode (`CM_Set_DevNode_Property`)

See `memory/project_nova_state.md` for full session history.
