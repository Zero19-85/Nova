# Nova Project Documentation & Instructions

## ⚠️ READ FIRST — this file covers the Nova HOST only

The repo is a **Cargo workspace**, not a single crate: `nova-core`,
`nova-server`, `nova-relay`, `echo-client`, `echo-android`, plus an `android/`
Gradle project. Everything below describes the Nova host (`nova-server`) and is
accurate for it.

**Echo — Nova's own native client — is not documented here.** It has its own
handoffs, and they are the authority for anything client-side:

| Doc | Covers |
|---|---|
| `HANDOFF_ECHO_P2P.md` | hole punching, relay signalling, sealed media, RUDP |
| `HANDOFF_ECHO_ANDROID.md` | the Android app, JNI surface, NDK toolchain |
| `HANDOFF_ECHO_INPUT.md` | mouse/keyboard, latency diagnosis, microphone passthrough |
| `HANDOFF_ECHO_AUDIO.md` | downstream game audio, ghost-sink isolation, A/V sync engine |

**Host-side changes Echo made that ARE in this file's territory:**

- **`audio_shim.cpp`'s ghost-sink resolver is now two lists** (2026-08-16).
  `kGhostSinkNames` is an ordered preference for where host audio may be *sent*
  (Steam Streaming Speakers → NVIDIA Virtual Audio); `kNotPlaybackNames` is a
  superset used only negated, for where crash recovery may restore the default
  output. **VB-CABLE is deliberately absent from the first and present in the
  second** — the Echo microphone renders into it, and a ghost sink on the same
  cable feeds game audio back to the remote user as their own microphone.
  Deleting it from one shared list would have moved the bug, not fixed it.
- **The Master forks audio at the `AudioFrame` hop** (`lib.rs`). The RTP:48000
  path Moonlight uses is untouched; a sealed copy additionally goes to any live
  Echo session. Capture and encode are shared wholesale — Echo added framing.
- **Echo sessions negotiate 20 ms audio frames** where Moonlight negotiates 5.
  This is per-Worker-session, so a Moonlight client sharing the pipeline gets
  20 ms too — accepted deliberately over transcoding the same audio twice.

## Project Scope
Nova is an ultra-low footprint, native Rust game-streaming host.
**Goal:** Flawlessly mimic GeForce Experience so Moonlight clients can connect.
**Architecture:**
- Async backend (`tokio` / `hyper`) for networking, mDNS, and pairing (HTTPS/XML).
- Native C++ FFI shim (`shim.cpp`) compiled to `nova_shim.dll` — zero-copy DXGI-to-NVENC hardware encoding.
- Architecture targets: High performance, ultra-low latency, and minimal portable `.exe` footprint.

## ⚠️ READ FIRST — Nova is THREE processes (Session-Survival Architecture, Phase 16)

Anything below describing Nova as "ONE interactive elevated process" is pre-Phase-16 history. Current process model — one binary, four modes:

| Mode | Identity / session | Owns |
|---|---|---|
| `--service` (**Master**, `NovaService`) | LocalSystem, Session 0 | ALL networking: `pairing.rs`, `rtsp.rs`, `control.rs`, `rtp.rs`, `session_negotiate.rs`, mDNS, `RtpSender`, audio TX, Worker supervision |
| `--worker` (**Worker**) | elevated USER, console session | `capture/`, `encoder.rs`, `virtual_display.rs`, `audio.rs` capture, `input.rs`, `tray.rs` |
| `--system-input-helper` | **SYSTEM primary token**, console session | secure-desktop mouse/keyboard injection ONLY; spawned per lock-screen interlude, killed on unlock |
| (no flag) | elevated USER | legacy monolithic `run()` — still intact as a fallback; **mirror Worker-loop changes here** |

- **IPC:** `\\.\pipe\NovaControl` + `\\.\pipe\NovaMedia` (Master↔Worker), `\\.\pipe\NovaInput` (Master→input helper). Hand-rolled `[u32 len][u8 tag][payload]` framing in `src/ipc.rs`. Toggle: `service.rs WORKER_SPLIT_ENABLED` (true).
- **Why three:** WGC's broker fails under SYSTEM (`0x80070424`) ⇒ capture must be the interactive user. The Winlogon desktop's ACL admits only SYSTEM ⇒ DDA capture impersonates a service-supplied SYSTEM token. **UIPI judges the injecting process's PRIMARY token** ⇒ input at the credential provider needs a SYSTEM-primary process. Session 0 can't `SendInput` into the console session at all ⇒ the Master can never inject on the Worker's behalf. Each boundary is load-bearing; do not "simplify" by merging processes.
- **Never re-litigate these (all live-confirmed dead ends):** host-as-SYSTEM (breaks WGC); Master-side input injection (session-local `SendInput`); a kernel-mode virtual HID driver (ViGEmBus is gamepad-only; FakerInput unmaintained; cf. the VAD attestation-signing wall).
- **Secure-desktop flags are ON and correct:** `dda.rs DDA_SECURE_DESKTOP_ENABLED` + `input.rs SECURE_DESKTOP_INPUT_ENABLED`. The 2026-07-30 "local physical input deadlock" that once justified disabling them was a **misdiagnosis** — the user's wired mouse/keyboard never froze; only REMOTE input did, which was the UIPI swallow. Physical HID input is never affected by desktop attachment. Do not re-quarantine on that basis.
- **Logs are per-process:** `nova-service.log` (Master), `nova.log` (Worker), `nova-input.log` (input helper). Read the right one — a Worker-side symptom is invisible in the Master's log and vice versa.

## Developer Rules for Claude
1. **Always verify:** Before executing changes, audit the Rust `Cargo.toml` and `build.rs` to ensure no hallucinated static links are injected into the NVENC pipeline.
2. **Performance First:** Keep dependencies minimal. Prioritize zero-copy transfers (DXGI to NVENC).
3. **Workflow:** I (the user) will use this chat to coordinate tasks. You have access to workspace files via Claude Code. Use this to audit code and apply edits directly. If a build fails, analyze the compiler output, identify the specific missing library or header, and fix the `build.rs` or shim pathing.
4. **Consistency:** Ensure pairing logic (port 47989) and discovery (mDNS) stay compliant with the GameStream protocol.
5. **Build output:** `cargo build --release` produces two files that must be deployed together: `nova-server.exe` and `nova_shim.dll` (both in `target/release/`). The DLL is built by `build.rs` via `cl.exe` + `link.exe /DLL` and copied automatically.

## Current Phase: Echo E9 — **two-way audio + A/V sync** (2026-08-16), live-confirmed

Nova's own client now carries game audio downstream and the phone's microphone
upstream, synchronised. Host-side impact is summarised at the top of this file;
the engineering record is `HANDOFF_ECHO_AUDIO.md`. Nova host work is unchanged
since Phase 16 below.

Final measured audio latency on the reference device: **190 ms** — 60 jitter,
40 track buffer, 20 decoder, ~70 device output stage. Most of that is a hardware
floor, so **video is delayed to meet it** rather than audio hurried; the sync
engine tracks the measurement automatically and is off by default because it
buys sync with input latency.

---

## Zero-config LAN discovery + Echo client polish (2026-08-18) — LIVE-CONFIRMED

The phone finds the PC by itself. Host advertises `_echo._tcp`; the Android app
browses, and tapping a result fills the address and the relay pair. Live on the
dev box: discovered as **APEX / 10.0.0.205**, relay auto-filled, paired, streamed.

### `nova-server/src/echo/discovery.rs` — the `_echo._tcp` record

- **A SECOND service type, never extra keys on `_nvstream`.** That record
  advertises port 47989 and describes a GameStream host to every Moonlight
  client on the LAN; Echo is a different protocol on a different port. One
  daemon, two registrations.
- TXT: `txtvers`, `fp` (host cert fingerprint), `name` (COMPUTERNAME), and
  `relay`/`relaypin` — **the relay pair is emitted only when BOTH are set.** A
  URL without its pin is not a usable relay (the pin is what authenticates it),
  so publishing one alone would fill one field and send the user hunting for the
  other. Absent keys let the app say "LAN only" honestly.
- **Registration is spawned, not inline.** The fingerprint comes from
  `pairing::server_identity()`, which a fresh install has not generated yet;
  registering immediately would advertise a blank `fp` and the app would
  helpfully fill its field with the blank. Waits ≤60 s, same as
  `echo::signaling`'s `await_identity`.
- **Both registration sites** — Master (`start_master_network`) and monolithic
  `run()`. Miss the second and a manually-launched host is undiscoverable.
- `ServiceDaemon` has no `Drop` impl and its loop uses `try_recv`, so the daemon
  thread outlives every handle. The pre-existing `_nvstream` record relies on
  this too; it is a fact, not an accident.

**THE SECURITY BOUNDARY — do not erode it.** mDNS is unauthenticated: anything on
the LAN can advertise `_echo._tcp` and claim any fingerprint. `fp` is a *hint
that pre-fills a field*, and the app deliberately **does not** fill Nova's
fingerprint from it — that field is written only by a completed PIN handshake
(phase-3 signature check). The advertised `fp` is used only to label a host
"paired"/"not paired yet" by comparing against what pairing already stored. A
value published here must never let a client skip a check it would otherwise
perform.

### The loopback relay rewrite — `relay_reachable_from`

`[echo.signaling] url` is written from the PC's point of view, so
`https://127.0.0.1:8443/...` is correct *there*. Broadcast to a phone it resolves
to the phone's own loopback: `Connection refused (os error 111)`, live 2026-08-18.
The advertised copy is now rewritten to the host's LAN IP. **Only the
advertisement** — Nova's own signalling client still uses the configured URL
verbatim and keeps its loopback path.

Two measured facts make this sound rather than a guess, and both must stay true:
the relay listens on **`0.0.0.0`** (so the LAN address really answers), and relay
TLS is pinned by **certificate fingerprint** via a custom verifier
(`identity::client_config_pinned`), **never by hostname** — so moving the
authority cannot break the handshake. Echo carries the same rewrite client-side
for hosts that predate this.

### `android/.../HostDiscovery.kt` — why Kotlin, not Rust

`NsdManager`, deliberately, against the instinct to mirror the host's `mdns-sd`.
Receiving multicast on Android needs a `WifiManager.MulticastLock` held across
the browse, so a Rust listener would need a Kotlin dependency anyway and would
still owe Doze and network-change handling. Discovery is not on the latency
path — it runs once, before a session, driven by a human reading a list.

- **Serialized resolve queue below API 34.** The platform resolver handles one
  request at a time; a second `resolveService` fails `FAILURE_ALREADY_ACTIVE` and
  on several releases wedges the resolver outright. Two hosts, or one host on
  Wi-Fi and Ethernet, is the normal case.
- **API 34+ uses `registerServiceInfoCallback`,** guarded on
  `SDK_INT >= 34` **AND** T-extension 7 — the platform annotates it as needing
  the extension, so an API-level test alone is not the documented guard.
- Callbacks run on a **direct executor**, not `Context.mainExecutor`: that is an
  API-28 call this class does not need, and it is the one arrangement that lets
  publishing a `StateFlow` resume a Compose collector inline while a lock is held.

### Adaptive icon — zero PNGs

`mipmap-anydpi-v26/ic_launcher.xml` + two vector drawables + a `<monochrome>`
layer reusing the foreground. **minSdk is 26, so nothing ever consults density
buckets** — the absence of `mipmap-*dpi` PNGs is correct, and lint's
`ObsoleteSdkInt` warning about the `-v26` qualifier is the same fact stated as a
complaint. Content sits inside a 36-unit radius of centre, so no mask clips it.

### Two client bugs, and the lesson from each

- **"Failed: invalid session handle" on manual disconnect.** `nativeClose` zeroes
  the magic word *before* it finishes tearing down (seconds), and `pollLoop` only
  re-checked the handle on the **timeout** path — leaving the "handled an event,
  looped round" edge unguarded, which is exactly the edge a manual stop lands on.
  Fix: `nativePollEvent` answers `null` for an unrecognised handle, the loop
  checks before every call, and `stop()` posts a clean idle state clearing the
  stale `error`. **A bad handle is a RETURN VALUE on this bridge** —
  `nativeSendInput`→`false`, `nativeFillBuffer`→`FILL_BAD_HANDLE`,
  `nativePollAudio`→`AUDIO_BAD_HANDLE`; `nativePollEvent` was the sole outlier and
  the outlier was the bug. `nativeRelease` was never involved — it takes no handle.
- **"Stuck Searching…"** was a static `Found on this network` header above an
  empty list, read as a result. The header is state now, with an 8 s settle and a
  "Search again" button; the browse keeps running past the settle so a host that
  boots later still appears.

### Still open (deliberately)

- **LAN-direct is NOT built.** `session::open_path` is unconditionally
  relay-mediated: STUN gather → relay `lookup` → `offer` → punch. Discovery fills
  in fields; it does not remove the relay. `--control <ip>:48011` moves only the
  control tunnel. Removing the relay for same-LAN peers is a separate, larger task.
- **`network_security_config.xml` lists exact IP literals** (`10.0.0.0`, …), which
  match no real host and do not cover discovered addresses. Currently inert — the
  Rust pairing client is a raw socket that never consults the policy — but the
  declared intent is now misleading.
- The Kotlin handle model is still a raw `Box` pointer + magic word. The poller
  can in principle touch it between the staleness check and the call. Closing that
  for good needs an id-keyed `Arc` registry so an in-flight caller keeps the
  allocation alive.

---

## Teardown QoL (2026-08-17) — physical-mode baseline + End Stream across both client kinds

Two operator-reported teardown bugs, both live-measured and fixed. Read this before
touching `deactivate_after_stream`, the tray, or `ControlMsg::EndSession`.

### Restoring the topology is NOT restoring the mode
**Symptom:** after every stream the physical monitor came back at **1024x768** (a
2560x1440 panel) while `restore_topology` logged success. The service log had the
Worker reporting it on 9 of 11 session boundaries in one day.

**Root cause:** every path that re-lights a physical output hands the mode decision
to Windows — `SDC_TOPOLOGY_EXTEND` derives its own, `SDC_ALLOW_CHANGES` lets
`SetDisplayConfig` re-resolve a supplied mode, and the snapshot `restore_topology`
replays comes from `QDC_DATABASE_CURRENT`, i.e. the CCD *database*, which inherits
whatever the last activation persisted with `SDC_SAVE_TO_DATABASE`. One bad guess
became the thing Nova faithfully restored forever after.

**Fix (`virtual_display.rs`, `PhysicalMode` section):** capture each physical
display's committed mode (`QDC_ONLY_ACTIVE_PATHS`, never the database) before the
stream, persist it to `nova_display_baseline.txt` beside the exe, and push it back
via `force_resolution` + read-back verification at teardown, the boot devnode cycle,
and the orphaned-VDD heal. Deployment note: `nova_display_baseline.txt` is new state
next to `nova.toml`/`nova_paired.json`; deleting it is safe (the next stream
recaptures).

**Two non-obvious rules that are load-bearing:**
1. **The mode change lands AFTER the calls return.** The devnode disable and phantom-
   monitor removal are PnP operations; Windows re-evaluates the display tree
   asynchronously and can re-pick the mode after both return. The first version read
   once, saw the right mode, exited, and the monitor changed seconds later. The
   re-assert now watches for a bounded 2 s and leaves only after **two consecutive**
   clean reads, treating "not in the topology yet" as not-settled rather than
   all-clear.
2. **A Worker start HEALS, it does not learn.** Adopting the observed mode at startup
   turned one lost race into a permanent baseline (1024x768 restored forever). Startup
   re-asserts the persisted baseline; only a machine with no baseline captures there.
   Trade: a hand-changed resolution is adopted at the next stream start, and a Worker
   respawn before that reverts it once.

Live diagnostic: `cargo test -p nova-server --lib physical_mode_restore_live --
--ignored --nocapture` knocks the real display to half size and asserts the restore.

### "End Stream" only knew about Moonlight
`ControlMsg::EndSession` judged "is anything streaming?" from `ClientInfo` alone,
which describes the GameStream session and is empty during an Echo one — so with a
phone mid-stream the tray logged *"no active session — nothing to end"* and the
stream carried on. It now ends both: `SessionManager::force_end(why, EndMode)` for
Echo (returning whether anything was ended) plus the `ClientInfo` path, and clears
`last_configure` explicitly for both so no respawned Worker resurrects an
operator-ended session (Phase 16.1's invariant, no longer riding on `cancelled`).

**One press goes all the way down** — stream stopped AND physical display restored
(`EndMode::TearDown` / `cancelled: true`). A two-stage version was built and rejected
after use: "End Stream" reads as one intention. The tray item still relabels itself to
**"Release Display"** for the state that genuinely produces it — a client that
vanishes without a `/cancel` leaves the display suspended for a fast reconnect — driven
by `stats::teardown_pending`, published from the display itself rather than from intent.

**Echo's `stop_session` is now idempotent:** with no session it used to return
`NotTheOwner` (wrong — no owner exists to not be), which made the app's "End Stream"
button look dead on a second press. It now succeeds and releases the display, while
still refusing when *another* device holds the session or Moonlight is streaming.
**Client-side note:** `EchoController.stop()` zeroes its handle and only calls
`nativeClose` when non-zero, so a second press sends nothing at all — the app's button
is inert when idle by construction. Unnecessary now that press one tears down fully;
making it live again needs a reconnect-and-release path in `echo-android`.

### Tray "Quit Nova" — reported broken, did NOT reproduce
Measured clean twice (1.5 s, both processes gone, full graceful teardown). The Phase 9
threads suspected of hanging exit live in `echo-client`/`echo-android` — on the phone —
and both host entry points end in `process::exit`. What was real: the path was entirely
silent, `request_service_stop()` shelled out to `sc.exe` (whose stdout landed in
nova.log) and discarded the exit code. It now uses native SCM `ControlService`, returns
a `ServiceStopRequest` verdict, logs each step, and **refuses to exit on a refused
stop** — exiting there is a service respawn, not a quit, which is indistinguishable
from "nothing happened".

---

## Session persistence + flow control (2026-08-17) — code complete, **live validation owed**

Detached sessions with hot reconnect, on both client kinds, plus a per-session bitrate
budget. 121 lib tests. Nothing here has been exercised against a real client yet.

### The state model: DETACHED is not ENDED

A client that vanishes without saying goodbye (network drop, backgrounded app, phone in a
pocket) no longer ends its session. **Encoding and transmission stop immediately** — the
GPU and the uplink are not spent on someone who is gone — while the virtual display, the
desktop arrangement and whatever is running on it are **held**, so a reconnect resumes
into them. After `[stream] detach_grace_secs` (default 600) with nobody back, Nova tears
down as if `/cancel` had arrived.

`Deactivate { cancelled: false }` was already exactly this state; what was missing was a
bounded clock, a fast resume, and any equivalent at all on the Echo side.

**Invariants — do not break these:**

1. **The grace clock lives in the MASTER** (`session_watcher`'s `suspended_generation:
   Option<(gen, Instant)>`), never the Worker. The Worker dies on every sign-out, taking
   any Worker-side clock with it, and a sign-out is precisely when a detached session must
   keep counting rather than restart. This also fixed a live bug: `[stream]
   idle_teardown_secs` had been **dead in the deployed split** — `deactivate_worker` calls
   `mark_suspended()`, but only the monolithic `run()` ever polled `suspended_idle_secs()`,
   so a vanished Moonlight client left the virtual display up forever. General lesson,
   third time now (cf. dynamic bitrate): **when a feature moves into the split, check the
   Worker loop has a CONSUMER, not just a producer.**
2. **`grace == 0` means hold indefinitely, NOT expire immediately** — on both paths. A
   configured zero is an operator saying they will end sessions themselves; inverting it
   would make the opt-out the most aggressive setting available.
3. **An explicit end always bypasses the clock.** `EndMode::TearDown` / `cancelled: true`
   tears down at once — that is yesterday's "one press goes all the way down".
4. `detach_grace_secs` supersedes `idle_teardown_secs`, which still works. **Both are
   `Option<u32>`** so "absent" is distinguishable from "explicitly set to the default";
   read only through `StreamConfig::detach_grace()`. An upgraded install that had tuned
   the old name must keep its tuning.

### Display-plane caching + CCD read-back (`VirtualDisplay::resume_suspended`)

`activate_for_stream` rewrites `vdd_settings.xml`, may cycle the devnode, waits for GDI
enumeration, snapshots the CCD database and commits a topology change — seconds of visibly
rearranging desktop. Paying that to arrive at the arrangement already on screen is pure
latency at the exact moment a returning user is watching. `resume_suspended(w,h,fps) ->
Resume::{Reused, Mismatch(why)}` skips it.

- **The committed mode is READ BACK out of CCD (`QDC_ONLY_ACTIVE_PATHS`), never assumed.**
  `active_resolution` is only what this process last *asked* for. Windows re-derives modes
  on its own schedule, `SDC_ALLOW_CHANGES` lets `SetDisplayConfig` substitute one, and PnP
  settles asynchronously after the call returns — the same family of failures that had a
  2560x1440 monitor come back at 1024x768 while the log said success.
- **Anything unknown is a mismatch.** Falling through to full activation is always safe
  (it is what every session did before); guessing wrong streams a display that is not the
  shape the client was promised. Size matches exactly; refresh gets the ±1 Hz tolerance
  `PhysicalMode::matches` uses, because Windows reports 59.951 Hz for a "60 Hz" mode.
- **Never reclaims a display that is not detached** — that would be two sessions sharing
  one display with neither told.
- Only ever succeeds **within one Worker's lifetime**: a respawned Worker's
  `VirtualDisplay` has `active == false`, so a resume across a sign-out correctly takes the
  full path. Wired into both `apply_configure_start` and the monolithic pre-activation.
- The encoder is still rebound and the monitor still renamed on the fast path — only the
  display *construction* is skipped. The returning client may have negotiated a different
  codec/HDR/bitrate, and a detached session can be reclaimed by a **different** device.

### Echo detach + idle reaper (`SessionManager::sweep`)

What Echo had before this: the tunnel sweep in `transport.rs` reclaimed the tunnel *slot*
at `TUNNEL_IDLE_TIMEOUT` (30 s), which dropped the sink, ended the driver, unblocked the
TLS read and returned from `serve_tunnel` — whose cleanup called `release_session_of` →
`stop` → **full teardown**. So a phone that lost signal cost the operator their monitor
arrangement 30 seconds later. That is the behaviour being replaced, and it is *why* both
halves had to change together:

- The sweep now calls `sessions.sweep(media_idle, TUNNEL_IDLE_TIMEOUT)` every 5 s, which
  **detaches** (`KeepDisplay`) and starts the grace clock.
- `release_session_of` now calls `detach_on_disconnect` rather than `stop`. **This is
  load-bearing**: the sweep drops the tunnel in the same tick it detaches, so the
  tunnel-closed path lands microseconds later. Left as a teardown it silently un-did every
  detach the instant it happened — the feature would have been a no-op in production, and
  no unit test saw it because the sweep and the transport were only ever tested apart.
- The distinction on the wire is **"said goodbye" versus "went quiet"**: a client that
  deliberately stops sends `stop_session` over the tunnel BEFORE closing it, and that path
  still tears down in full. Only a silent vanish detaches.

**Invariants — each of these is a real hazard, not a style preference:**

1. **The reap must NOT end the plane if Moonlight has taken the pipeline.** `echo_active`
   goes false at detach precisely so a Moonlight client can claim an idle pipeline — but
   the detached session's grace clock keeps running, and its expiry would send a cancelling
   `Deactivate` that tears down *that* session. It would present as a Moonlight stream
   dying at random, ten minutes after an unrelated phone lost signal. The reap forgets the
   record and touches nothing. Mutation-tested.
2. **Lock order is ALWAYS session → RTP.** Every existing path does this (`start` →
   `plane.begin`, `force_end` → `plane.end`, and `media_supervisor` releases the session
   lock in `seal_video` before taking the RTP lock). The sweep therefore reads
   `rtp.idle_since` FIRST, unlocked, and passes the result in — `live_peer()` exists for
   exactly that. A sweep that queried RTP while holding the session lock would be the sole
   inversion in the system and the sole deadlock opportunity.
3. **`rtp.idle_since` returning `None` means "no news", never "dead".** It tracks only the
   single most recent sender (deliberately — a per-address table on an internet-reachable
   port is a memory footgun), so one stray datagram from a scanner erases the reading.
   Detaching on `None` would let a port scan kill a healthy stream.
4. **Liveness is authenticated or it is a ping, never control traffic alone.** Input and
   mic datagrams that open under the session key are *proof* only the holder sent them; the
   other source is the 500 ms media-socket ping. Judging by control idle alone declared a
   perfectly healthy stream dead 30 s in, live on 2026-08-15.
5. **Detached takeover semantics: another device MAY take over a DETACHED session; a LIVE
   one is still refused** with `HeldByAnotherDevice`. Holding the seat for the whole grace
   period would lock every other paired device out of the host for ten minutes to protect a
   stream nobody is watching. The newcomer inherits the same display; keys are freshly
   minted either way.
6. **Reclaiming calls no `plane.end` at all** — nothing is running, and `end(TearDown)`
   would destroy the very display being reused. **Keys are always minted fresh**, never
   cached across a detach, so a datagram sealed for the old session cannot be replayed into
   the new one.

### Bitrate budget (`src/qos.rs`)

`QosController` moved here from lib.rs unchanged, joined by `video_budget()`. Two layers:
the **budget** is decided once at negotiation from facts that cannot change mid-session;
the **controller** walks up and down underneath it.

- **Cap first, then reserve** — reserving from the raw request would let a client dodge the
  reservation by asking for more.
- `resolution_ceiling()` interpolates a tier table (720p 20 / 1080p 40 / 1440p 70 /
  4K 120 Mbps at 60 fps, ~2x Moonlight's recommendation) on pixel count, scaled by
  `(fps/60)^0.75` — sub-linear because inter-frame prediction improves as cadence rises.
  Tune the table, not the call sites.
- Applied at **both** negotiators (`session_negotiate::negotiate` and
  `SessionRequest::validate`) and computed against the **negotiated** fps, not the
  requested one — an H264 session capped to 24 fps by Level 5.2 must not be budgeted for
  120.
- `[network] audio_reserve_kbps = 512` against a measured ~140 Kbps cost (128 Kbps Opus +
  framing). **Never takes more than ¼ of a session's ceiling**, so raising it for a fat
  link cannot starve a thin one. Echo's mic is upstream and deliberately not counted.
- **The budgeted number IS the QoS ramp ceiling** — `NegotiatedParams::bitrate_kbps`
  becomes both NVENC's CBR target and `QosController`'s `ramp_target`, so recovery
  converges on the cap and never on the raw request. That was free, and it is the reason
  the cap belongs at negotiation rather than in the encoder.
- A **zero** request (no `maximumBitrateKbps` in ANNOUNCE) passes through untouched;
  inventing a bitrate would hide a negotiation failure behind a plausible number.

### "End Stream did nothing — I had to close the app" — the tunnel slot lockout (2026-08-17)

**Symptom:** ending an Echo stream works the first time; a later attempt does nothing, and
only closing and reopening the app makes it register.

**What the log showed:** the session was never ended by an RPC at all — it aged out
(`⌛ tunnel … silent for 30s`) and detached. Just before that:
`🚧 {new port} wants a tunnel but {old port} holds the slot`. The client had moved to a new
source port and was locked out for the remainder of the 30 s timeout, so its
`stop_session` had nowhere to go. Closing the app cannot help — closing it is what starts
the wait.

**Two compounding causes, both fixed:**

1. **`RtpSender::last_rx` remembers exactly ONE sender**, so any other datagram on the media
   port erased the liveness reading for the live tunnel — and in practice that "other"
   address is the client's own re-punch from a new port. The incumbent then looked silent
   on media, `idle_for` fell back to control idle, and a healthy tunnel read as dead.
   Fix: `last_rx_pinned`, one `Instant` for the pinned peer specifically, cleared by
   `reset`/`pin_target`. Not a map — the footgun `last_rx` warns about is a table keyed by
   whatever a stranger sends; this is keyed by the peer Nova itself pinned.
2. **Contention did not shorten the wait.** `TUNNEL_IDLE_TIMEOUT` (30 s) is the right
   patience when nobody is waiting, and the wrong patience when a user is staring at a
   phone that will not connect. New `CONTENDED_IDLE_TIMEOUT` (5 s): when another peer is
   actively asking, an incumbent silent that long hands the slot over.

**Why 5 s is safe only because of fix 1:** a granted session pings every 500 ms, and those
pings are now recorded against the pinned peer no matter who else sends to the port. Before
fix 1, a contender's own packets erased the incumbent's liveness — this shortcut would have
handed away healthy tunnels. **Do not lower `CONTENDED_IDLE_TIMEOUT` further, and do not
reintroduce a control-idle-only test** (that is the 2026-08-15 black-screen-at-30 s bug).

The handover releases the slot but `busy` clears asynchronously, when the served task
unwinds — so the triggering datagram is still dropped and the newcomer arrives on a
retransmit. The win is 30 s → 5 s, not a saved round trip.

### The microphone that "stops working" — VB-CABLE steals the default (2026-08-17)

**Symptom:** the host's microphone registers no sound in any application, with no
error anywhere and no Echo session running.

**Cause, measured:** installing VB-CABLE for microphone passthrough adds a *capture*
endpoint, "CABLE Output", and Windows readily makes a newly arrived capture device the
system default. That endpoint carries **nothing** unless an Echo client is actively
sending microphone audio into the other end of the cable — so every app that uses the
default microphone gets digital silence from a device that is present, healthy and
working exactly as designed. On the dev box: real "Microphone" peaked **0.65**, "CABLE
Output" read **0.0000**.

**Nova does not cause this and cannot restore it.** `SetDefaultAudioDevice` is only ever
called with *render* endpoints (the ghost sink), verified by grep — Nova has never chosen
anyone's microphone. There is nothing for it to put back.

**Diagnose and fix with the shipped tool** (both halves of one question):
```
nova-server.exe --mic-probe listen default 5 <log>   # what does the DEFAULT mic hear?
nova-server.exe --mic-probe listen "Microphone" 5 <log>   # what does a named one hear?
nova-server.exe --mic-probe default "Microphone" 0 <log>  # make that one the default
```
`listen` with the device named `default` (or empty) probes
`GetDefaultAudioEndpoint(eCapture)` — the endpoint applications will actually be handed,
which is the only one whose silence matters. **Shells drop an empty argument**, so the
word `default` exists as the sentinel: PowerShell turns `listen "" 5 log` into
`listen 5 log` and probes an endpoint named "5".

**Note the return convention:** `SetDefaultAudioDevice` returns **0 for success** and a
negative step code for failure — the opposite of the `!= 0` convention the probe entry
points use. `audio.rs::recover_stuck_sink` is the reference caller.

**Deliberately NOT automated.** Nova auto-heals a stuck *output* at startup
(`recover_stuck_sink`) because Nova is what moved it. Doing the same to a *microphone*
Nova never touched would mean silently overriding a device choice the operator may have
made on purpose.

### Echo client: ending a session the app no longer has a handle for

Reported as an RPC failure; it was not. `EchoController.stop()` frees the native handle and
zeroes it, so a second press found `handle == 0`, skipped `nativeClose`, and **sent nothing
at all** — no stale channel to re-latch, the whole native session object was gone.

That matters more now than it did, because a session **outlives the app**: swiped away or
network lost means no `stop_session`, so the host detaches and holds the display for the
grace period with no client left to ask for it back.

**The wrong fix, and what it cost.** First attempt drove a real connection and stopped it
the instant the grant arrived. It landed roughly one press in three, and the log says why:
`⚡ reclaiming its detached session N` → `🎬 session N+1 started` → silence. The teardown
raced the session it had just created, and when it lost, the host learned nothing and the
fresh session detached again — so each press walked the session id up by one. Live
2026-08-17: **four presses for two teardowns.**

**The right one:** `stop_session` is an RPC on the control tunnel. It needs an
authenticated channel and nothing else — no media socket, no keys, no grant. `session::
release` (echo-client) stops after `hello` and asks; `nativeRelease` (echo-android) is the
blocking JNI entry point, the only one here without a handle or a poll loop, because one
request-response has no ongoing state to manage. One press, one round trip, no session
churn, nothing to race.

**Ending a session never needed a session.** If a future change is tempted to reuse the
streaming path for a control-plane action, that is the lesson.

---

## Phase 16 — **ALPHA 2.0** — Session-Survival Architecture: Master/Worker split, lock-screen streaming + remote PIN entry (2026-08-06)

Alpha 2.0's headline: **reboot → connect from Moonlight → see the Windows login screen → type the PIN with the remote mouse/keyboard.** All items below are live-confirmed on the dev box unless marked otherwise.

### 16.1 — Master/Worker split (networking survives sign-out)
Master (SYSTEM service) owns every socket and the client session; Workers come and go beneath it on sign-out, session swap, or crash. `control_supervisor` replays the live session's `ConfigureStart` to each newly-connected Worker, so streaming resumes without the client reconnecting. `session_watcher` (Master) polls `ClientInfo` for the PLAY edge and the active→inactive edge, sending `ConfigureStart`/`Deactivate`. Key invariant: **a non-cancelled `Deactivate` must NOT clear `last_configure`** — a sign-out's suspend would otherwise leave the post-login Worker unconfigured forever ("video never resumes after the session swap").

### 16.2 — Strict frame pacing (static-screen blur) — FIXED
Phase 11's static-frame gate (encode nothing until a 5 s keep-alive) starved the client decoder and let CBR degrade a motionless screen until the mouse moved. Now every missed slot re-submits `capturer.cached_texture()` as a duplicate P-frame ⇒ uninterrupted constant-fps bitstream, idle bitrate spent refining the static image. Gated on `client_connected` so idle-with-no-client keeps NVENC at 0%. `IDR_KEEPALIVE_INTERVAL`/`last_frame_sent` are gone from both loops; the Worker loop also gained the monolithic path's ~1 ms spin-finish dispatch. **Deliberate trade:** Phase 11's "0% encode while streaming a static desktop" signature is intentionally abandoned.

### 16.3 — Login-handoff ghosting (frozen PIN screen after sign-in) — FIXED
`maybe_swap_backend` had no `(Wgc, Default)` arm, so a WGC session bound to the pre-switch desktop survived the Winlogon→Default login and served stale ghost buffers. `DesktopManager` now tracks `wgc_built_generation` (stamped on every path that builds a WGC session); a generation mismatch while back on Default triggers `rebuild_wgc_after_switch()` — drop session+pool, fresh `new_on_device`, 1 s retry cooldown (generation left unstamped so it re-arms). The rebuilt capturer has `has_frame() == false`, so the loop cannot encode a stale surface.

### 16.4 — Universal VDD app routing + Worker-side app launch
`uses_virtual_display`: apps 2/3/4/5 (Steam/Xbox/RetroArch/Virtual Desktop) are always headless; only app 1 (Desktop) mirrors the physical primary. **`/launch` no longer starts apps in the Master** — that spawned them into the service's Session 0, invisible to the stream. Chain: `app_launcher::LAUNCH_VIA_WORKER` (set in `start_master_network`) ⇒ pairing skips its direct launch ⇒ `ClientInfo.pending_app_launch` (set by `/launch`, never `/resume`) ⇒ `NegotiatedParams.launch_app` ⇒ `ConfigureStart.launch_app` ⇒ Worker launches AFTER VDD activation (VDD is primary ⇒ the window lands on it). Master clears the flag post-send so a Worker respawn can't double-launch.

### 16.5 — Fresh-install pairing hang — FIXED
Pairing runs in Master (Session 0, no tray) so `getservercert` blocked forever at "waiting for PIN"; "exit and restart Nova" appeared to fix it only because a manual launch runs the monolithic path. Full relay now: pairing `tray_tx` → Master `nova-pair-dialog-fwd` thread → `ControlMsg::OpenPairDialog` → Worker tray dialog → `ControlMsg::PinRelay` → `control_supervisor` writes pairing's `global_pin`. Manual "Pair Device" from the tray relays the same way. (Pre-pairing `47984 TLS CertificateUnknown` spam is normal client probing — it stops once paired.)

### 16.6 — Lock-screen streaming + remote PIN entry: the **UIPI "silent swallow"** — FIXED
**The lesson worth keeping.** With SYSTEM impersonation + a successful `SetThreadDesktop(Winlogon)`, the Worker's `SendInput` returned SUCCESS and the PIN field stayed empty. Reason: the checks impersonation satisfies (`OpenInputDesktop`, `SetThreadDesktop`, DXGI duplication) are kernel-object ACL checks, which honour a thread's impersonation token — but injected input reaching the credential provider is gated by UIPI/integrity in **win32k, which evaluates the injecting process's PRIMARY token**. The Worker's is the interactive user (High) < Winlogon (System) ⇒ accepted at the API boundary, dropped before the UI, nothing to log. **Signature: capture works, input silently doesn't.**
Fix — `--system-input-helper`: `service::spawn_input_helper()` builds a SYSTEM **primary** token (`create_inheritable_system_token` used AS the process token, like `spawn_host_as_system_fallback`) and `CreateProcessAsUserW`s a minimal helper into the console session with `lpDesktop="WinSta0\Default"` (deliberately NOT Winlogon — that would bind to the secure desktop object live at spawn time). `lib.rs::input_helper_supervisor` owns its pipe/process/`ready` flag; `control_supervisor` detours `InjectInput` there while `secure_desktop && ready`, else falls through to the Worker (spawn/connect failure ⇒ never worse than before). `ControlMsg::SecureDesktopChanged` (Worker→Master, 250 ms poll task) drives the lifecycle — only the Worker can observe the boundary. **Gamepad packets stay on the Worker** (`input::is_gamepad_packet`): ViGEmBus is a kernel bus device, never UIPI-blocked, and routing it would materialise a second virtual pad on every lock. Helper runs on a **current-thread** runtime — desktop attachment is thread-affine thread-local state, so the recv→inject loop must never migrate threads.
Injection hardening in `input.rs`: `send_input_synced` funnels all mouse/keyboard injection and, if `SendInput` inserts 0 events, force-resyncs the desktop and retries once (Sunshine's `send_input` + `syncThreadDesktop` model); `sync_desktop_for_input(force)` now records the desktop generation only once the desired state is REACHED (stamping it before a failed attach meant input stayed dead for the whole interlude); `OpenInputDesktop` uses `DF_ALLOWOTHERACCOUNTHOOK` (Sunshine parity); `ALWAYS_FOLLOW_INPUT_DESKTOP` mode for the helper (no desktop-switch monitor in that process).

### 16.7 — Pre-login Worker crash-loop — FIXED
WGC `0x80070424` pre-login with no usable backend made `run_worker` exit via `?`; the service's 4→60 s respawn backoff then also delayed post-login recovery. The Worker now retries `DesktopManager::new_wgc` in place every 3 s, staying alive with pipes and tray retry intact. Also: any `AcquireNextFrame` error (live: `0x887A0001` at dismissal) now sets `access_lost` so the manager rebuilds/swaps instead of the capture thread dying silently.

### 🐛 Open bug for the next polish pass — sign-out stream recovery
**Symptom:** signing out mid-stream drops the client to a permanent black screen; it never falls back to showing the logon/PIN screen. Manual disconnect + reconnect in Moonlight recovers it. Reboot → lock screen → remote PIN works fine, so this is specific to the *interactive → sign-out* transition. Deliberately deferred (user's call, "Alpha 2.0" ships with it).
**Plan of attack (start here, in order):**
1. **Read `nova-service.log` + `nova.log` across the sign-out boundary first.** Establish whether (a) the replacement SYSTEM-fallback Worker ever spawned, (b) it reported `WorkerReady`, (c) `🔁 Master: replaying ConfigureStart to newly-connected worker` fired, and (d) frames resumed (`📊 RTP/s`). That single question splits the whole search space.
2. **Prime suspect — `last_configure` cleared or never replayed.** `stop_host` on session change sends a graceful stop; verify the sign-out path's `Deactivate` is `cancelled: false` (suspend) and that nothing clears `last_configure` (16.1's invariant). If the replay fires but no frames flow, suspect the Worker applying `ConfigureStart` while the VDD/CCD calls fail under the SYSTEM-fallback token (`is_system_fallback` skips VDD activation by design — confirm capture still binds *something*).
3. **Second suspect — RTP/session continuity.** `session_watcher` only sends `ConfigureStart` once per `session_generation`; if the sign-out produced a `Deactivate` that reset `active_generation` without a new PLAY, the new Worker gets configured but `rtp_sender` may be reset/unlearned (`frame_index`, learned client target). Check whether `📦 frame 1` / `🎯 learned client video target` reappear.
4. **Third — the media pipe.** Confirm `🔗 Master: worker media pipe connected` for the new Worker; a Worker that connects control-only would encode into a dead pipe (silent black).
5. Only then consider forcing an IDR + `rtp_sender.reset()` on Worker adoption, which is the likely one-line fix if the client is simply waiting on a keyframe whose reference chain died with the old Worker.

---

## Phase 15 — Secure-desktop capture (WGC+DDA dual backend), two-process privilege model, audio single-owner (2026-07-09)

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

### 15.4 — HEVC/HDR10 pan micro-stutter batch (2026-07-13) — code complete, live pan-test pending
Symptom: H264/SDR buttery smooth, subtle rhythmic micro-stutter on heavy camera pans at 10-bit HEVC HDR10. Audit found FOUR compounding hot-path costs, all of which scale with exactly the HDR/pan workload (bigger frames ⇒ more packets ⇒ more send time; FP16 ⇒ 2× copy bytes):
1. **RTP send ran ON the capture/encode thread (the big one).** `send_frame` built shards, computed RS-FEC parity, and spin-paced 10-packet batches synchronously — a 60+-packet HEVC pan frame cost 1.5–3 ms of the 8.33 ms 120 fps budget, every frame, rhythmically. Fix (`rtp.rs` restructured): dedicated `nova-rtp-send` worker thread (THREAD_PRIORITY_HIGHEST — one notch under the capture thread) owns a `TxEngine` with ALL per-frame state; the capture-thread `RtpSender` handle queues frames over an ordered command channel with **recycled payload buffers** (≤3 in flight, zero steady-state alloc) and NEVER blocks: if the worker is ≥3 frames behind, `send_frame` returns false and lib.rs drops the frame + `request_idr()` (a silent drop would break the P-reference chain; log rate-limited). Ping-learning (`try_learn_target`) stays on the capture thread via a cloned socket handle (recv side only); learned targets/`configure`/`reset`/codec flags travel through the same channel as frames so ordering vs. session boundaries is exact. Frame-type classification is now passed INTO `send_frame` (lib.rs already computes it for the first-IDR gate) — the old code full-scanned every payload twice.
2. **Pacing gap now bounds burst rate independently of frame size.** Old: fixed fps-scaled gap (150 µs at 120 fps) between 10-packet batches ⇒ a big pan frame still blasted ~750 Mbps micro-bursts at the AP. New (`TxEngine::send_frame`): the frame's batches spread across ≤40% of the frame interval — `gap = clamp(0.4·interval/batches, 40–300 µs)` — capping peak transmit rate at ~2.5× the nominal stream bitrate for ANY frame size; small frames keep a gentle 300 µs ceiling. Off-thread, so the spread costs the capture loop nothing.
3. **shim.cpp EncodeFrame: per-frame GPU event-query busy-spin + double full-frame copy removed.** The `g_cleanBgTex` intermediate + `g_copyFence` `GetData` spin existed to stop `IDXGIOutputDuplication::ReleaseFrame` letting DWM overwrite the source mid-copy — obsolete since WGC/DDA: `dxgiFrame` is Nova's OWN stable cache texture (wgc.rs `cache_frame` / DDA staging upload), written only by the capture thread on the same immediate context, so same-context ordering + NVENC's driver-side mapped-resource sync make the fence and the extra copy pure overhead (at 4K FP16 the removed copy alone was ~66 MB/frame of GPU traffic; the spin stalled the CPU for a full GPU-queue drain each frame). Single `CopyResource(g_compositeTex, dxgiFrame)` remains — refreshed from source every call, so static-desktop replays still can't stamp cursor trails, and the (DDA-only, currently dormant) cursor overlay keeps its render-targetable surface.
4. **Timer/dispatch jitter:** `timeBeginPeriod(1)` at startup (Windows default ~15.6 ms tick is COARSER than the 8.33 ms 120 fps budget — every tokio pacing sleep rounded into multi-ms jitter; Sunshine/Apollo do the same; `Win32_Media` feature was already implied by `Win32_Media_Audio`, bindings-only). While `client_connected`, the frame loop sleeps to ~1 ms before the slot and spin-finishes the remainder for exact dispatch; idle keeps the plain sleep (no CPU burn with nobody watching). Also gated the previously-unconditional per-frame `[ENC]` println (a blocking WriteFile to nova.log at 120 Hz on the hot path) to first-10-frames + IDRs.
- Thread-contention audit result (no further change needed): capture→convert→encode stays single-owner on the TIME_CRITICAL main thread (the D3D11 immediate context is not thread-safe — do NOT move conversion off it); audio has its own MMCSS threads; control/RTSP/pairing are separate; the only cross-thread hot-path state is now the ordered command channel.
- Verified: `cargo check` clean, 11/11 runnable `cargo test --lib` pass (RTP wire-format tests exercise the threaded sender over loopback: frame_index=1 first frame, reset semantics, stale-ping relearn), release build clean.
- **Live validation needed:** 4K@120 HEVC Main10/HDR10 heavy-pan session — expect flat frame cadence (no rhythmic stutter), `📊 RTP/s` unchanged, no "RTP send queue full" lines on a healthy LAN; regression-check H264/SDR smoothness and static-desktop 0% Video Encode idle. **15.4 CONFIRMED WORKING live (2026-07-13)** — user reports the micro-stutter is gone.

### 15.5 — Batch-2 close-out: graceful service→host stop + single-instance guard (2026-07-13) — code complete
Closes the two refinements left open in 15.2c (its live-validation items 3 and 4). The service architecture itself (SYSTEM launcher, Winlogon DDA capture, WGC↔DDA handoff) was already built and live-confirmed — do NOT rebuild it.
- **Cross-process graceful stop (`Global\NovaHostShutdown`, manual-reset event).** TerminateProcess skips every destructor/console handler, so a service-initiated stop (installer upgrade `sc stop`, manual stop, fast-user-switch respawn) used to strand the VDD headless topology + virtual audio sink until the next boot's healing pass. Now: the HOST creates the named event at startup (`service::spawn_host_shutdown_watcher`, thread `nova-host-shutdown`) and funnels a signal into the same `shutdown_tx` watch channel as the tray "Quit" → full graceful teardown. The SERVICE's `stop_host()` signals the event, grace-waits `HOST_GRACEFUL_EXIT_MS` (6 s), then TerminateProcess as backstop — used on BOTH the final-stop path and the session-change respawn (old session's host now restores topology before the new session's host spawns). Design notes: manual-reset + `ResetEvent` on create so a stale signal (host crashed before consuming) can't instantly kill the next host; `Global\` namespace crosses Session 0↔1 (elevated host has SeCreateGlobalPrivilege; SYSTEM opens anything with default SD). Host-initiated tray-Quit is unchanged (`request_service_stop` → the service's extra event-signal lands on an already-exiting host, harmless; the reverse extra `sc stop` on a service-initiated shutdown errors on STOP_PENDING and is ignored).
- **Machine-wide single-instance mutex (`Global\NovaServerHostSingleton`).** Claimed at the very top of `run()` (right after logger init, BEFORE any VDD/port/audio state is touched): a second host (task+service overlap, double manual launch) now logs `🚫` and exits 0 cleanly instead of cycling the VDD devnode and crash-fighting over ports. Creation failure (unelevated ⇒ no SeCreateGlobalPrivilege) proceeds unguarded with a warning — port-bind conflicts remain the backstop. Guard held in a named local for all of `run()`; kernel releases it at process exit, so the service's respawn after a Quit acquires it cleanly.
- `cargo check` clean; 11/11 tests; release build clean.
- **Live validation (Batch 2 test pass):** (a) reboot without login — one devnode cycle, no chime loop, Moonlight connects at the lock screen (DDA), PIN login → `🔀 DDA → WGC` + tray icon appears; (b) `sc stop NovaService` mid-stream — nova.log shows `🛑 Service requested shutdown` + graceful teardown (display/audio restored) instead of a hard kill; (c) start a second host manually while the service host runs — expect `🚫` clean exit; (d) tray Quit still stops service without relaunch. — **(b) and (c) already verified live on the dev box during deployment (2026-07-13): singleton 🚫 exit 0 confirmed; sc stop produced the full graceful teardown log.**

### 15.6 — "Ghost" audio orchestration close-out: sink override + mid-session routing watchdog (2026-07-13) — code complete
User's "Batch 3" ask (IPolicyConfig endpoint swapping + state preservation + teardown) was ALREADY BUILT in Phase 15.1 (audit confirmed: `arm_endpoint_restore`/`ORIGINAL_ENDPOINT` state preservation armed pre-VDD-flip; `IPolicyConfig::SetDefaultEndpoint` with manual vtable/GUIDs in audio_shim.cpp across all 3 ERoles; claim-once restore on every teardown path incl. crash recovery + 15.5 service stops). Do NOT re-implement. The user-reported symptom ("audio plays out loud on host / sometimes missed by capture") mapped to two REAL gaps, both fixed:
1. **`[audio] endpoint_override` (nova.toml) finally wired** — was parsed + printed since Phase 9 but never consumed. New shim export `FindAudioDeviceByName` (case-insensitive friendly-name substring OR exact endpoint-id match over ACTIVE render endpoints); `audio::set_sink_override()` stores it at startup (lib.rs calls it right after config load, then re-runs `recover_stuck_sink` since a stuck custom sink is only now recognisable); new resolver `find_virtual_sink_id()` = override-first → built-in list (Steam Streaming Speakers / VB-CABLE) — used by ALL sink-identity consumers (`recover_stuck_sink`, `arm_endpoint_restore`'s refuse-to-arm check, `SinkGuard::engage`). Lets the VDD's own HDMI audio endpoint (or VoiceMeeter etc.) serve as the ghost sink with zero extra drivers. recover_stuck_sink also now refuses the degenerate "restore target == the sink itself" case possible with an override.
2. **1 Hz mid-session routing watchdog** (in `send_pcm_loop`; `CaptureRoute` enum): WASAPI loopback binds ONE device at init and never follows default changes. (a) `PinnedSink` (client-only): if the default drifts off the sink (late device arrival — e.g. the VDD's HDMI audio endpoint enumerating seconds AFTER activation — or user fiddling), RE-ASSERT the sink via SetDefaultAudioDevice (throttled log; armed pre-stream endpoint still restores at session end). (b) `FollowDefault` (host_audio, or no sink): a default change returns `SendExit::DeviceChanged` → `audio_send_loop` tears down capture (own per-capture stop flag — new `CaptureSession` struct, SHIM_CAPTURE_ACTIVE released) and REBINDS onto the new default, 300 ms settle, same session. `AudioTxState` (target/seq/timestamp) lives OUTSIDE the rebind loop — seq/timestamp continuity across rebinds (a reset to 0 would look like a stream restart to moonlight-common-c). SinkGuard engages once per session (restore semantics unchanged).
- `cargo check` clean; 11/11 tests; release build clean (new shim export links).
- **Deployment notes (2026-07-13, dev box):** Nova was UNINSTALLED on the box mid-session (service deleted, `C:\Program Files\Nova Server` wiped — pairings/certs LOST, all devices must RE-PAIR against the regenerated cert). Redeployed manually: fresh exe+dll+`VirtualDisplayDriver\` payload, `--install-service`, both drivers reinstalled via bundled devcon (`devcon install MttVDD.inf 'Root\MttVDD'` from the INF's own dir — NOTE the HWID backslash needs shell-quoting or devcon gets `RootMttVDD` and fails exit 2). VDD preflight healthy after reinstall.
- **VAD (Virtual Audio Driver by MTT) — bundled but CANNOT LOAD (problem code 52):** the payload ships `SignedDrivers\x86\VAD\VirtualAudioDriver.{inf,sys,cat}`, HWID `ROOT\VirtualAudioDriver`, endpoint name "Virtual Audio Driver by MTT". Its .sys/.cat are validly CODE-signed (SignPath Foundation/GlobalSign) but NOT Microsoft-attestation-signed — kernel-mode audio drivers under Secure Boot require attestation, so Windows blocks it (CM_PROB 52). (MttVDD loads fine because IddCx = user-mode.) Do NOT enable test-signing/disable Secure Boot to work around it; get an attestation-signed VAD build upstream. Until then: **Steam Streaming Speakers is present on the dev box and serves as the ghost sink via the built-in list**; live nova.toml has `endpoint_override = "Virtual Audio Driver by MTT"` armed for the future — the resolver warns "matched no active render endpoint" and falls through to Steam's sink by design (clear the override to silence the warning).
- **Live validation:** (a) client-only stream → host speakers silent (Steam Streaming Speakers sink), audio on client; (b) change the default output in Windows sound settings mid-stream → client-only: log shows re-assert + audio keeps flowing; host_audio: log shows rebind + audio follows; (c) confirm the pre-stream endpoint still restores on disconnect//cancel/tray-quit/sc-stop; (d) RE-PAIR Pixel + Xbox first (fresh cert).

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
