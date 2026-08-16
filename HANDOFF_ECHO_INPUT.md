# Echo — Input, Latency, and What Comes Next (2026-08-16)

Read this before touching Echo's input path, and before starting the microphone
work. It supersedes nothing in `HANDOFF_ECHO_P2P.md` or
`HANDOFF_ECHO_ANDROID.md`; it covers the day mouse and keyboard input went from
unusable to working, and it records why, because most of the wrong turns are
re-tempting.

---

## 1. Status

**Mouse input is fixed and user-confirmed** ("yes mouse is fixed!"). Keyboard
works. Latency on a healthy LAN path is now:

```
📱 Echo client "My Device": rtt 11ms (best 5ms), capture held, input peak 70/s x2 samples
⌨️  Worker inject/s: 31 packets (31 rel, 0 abs, 0 other), 5.2ms total, 168µs avg
video 4ms (worst 29ms)
```

**The white film over the video is also fixed** (§7).

**No known open bugs in Echo's input or video path.**

**Microphone passthrough is DONE and live-confirmed (2026-08-16)** — the user
held a conversation through it. See §8, which is now a record of what was built
rather than a plan.

**Host→client game audio and A/V sync are DONE (2026-08-16)** — see
`HANDOFF_ECHO_AUDIO.md`. §10 below is kept as the record of what was predicted
before that work started.

**Not started:** gamepad input over Echo.

---

## 2. ⚠️ Read this before "fixing" pointer capture

A report late in the session claimed capture was permanently failing, based on
the overlay reading `capture: off` and `last input source: touchscreen`. **That
reading is expected and does not mean capture is broken.** Two independent
host-side measurements contradict it:

- The client reports `capture held` in `📱 Echo client` every 2 seconds.
- `⌨️ Worker inject/s` shows `0 abs` on every line — the absolute path is not in
  use at all. Absolute packets are what an *uncaptured* mouse produces.

**Why the overlay lies: opening the control panel deliberately releases pointer
capture** (`MainActivity.kt`, `LaunchedEffect(state.streaming, view,
controlsVisible, captureMouse)`) because a captured pointer cannot tap the
controls. The panel is also the only place capture state is displayed, so it
*always* reads `off` when you look at it, and `last input source` reads
`touchscreen` because you just tapped to open it.

This cost a full round. `captureEverHeld` exists to defeat it — the panel shows
`capture WORKS — it has been granted this session`, which survives the release.

**General rule for this codebase: never display a state whose observation
changes it, without saying so in the same sentence.**

Also note the commonly-suggested fix (`isFocusable = true`,
`isFocusableInTouchMode = true`, `requestFocus()` inside the tap handler before
`requestPointerCapture()`) is **already implemented** — see
`StreamSurfaceView.attemptCapture()`, called from `onTouchEvent`'s
`ACTION_DOWN`. It checks each Android precondition separately and reports which
one failed by name, because `requestPointerCapture()` returns void and logs
nothing an app can see.

---

## 3. The five bugs that caused the input lag

They stacked. Each one capped the effective rate at roughly the same place, so
each hid the next, and fixing any one alone produced no felt improvement. In the
order they had to be fixed:

### 3.1 `onHoverEvent` was never overridden — the mouse never arrived

`View.dispatchGenericMotionEvent` routes `ACTION_HOVER_*` to `dispatchHoverEvent`
→ **`onHoverEvent`**. `onGenericMotionEvent` receives scroll and joystick events
and *never* hover. Only `onGenericMotionEvent` was overridden, so the entire
uncaptured-mouse branch of `handlePointer` was unreachable dead code.

**The tell:** the overlay read `last input source: touchscreen` while the user
was moving a mouse. The input reaching the host was the user's *finger* on the
absolute touch path — which is also what produced the "cursor keeps crawling
after I stop" symptom, since a flick that leaves the glass keeps feeding stale
positions.

### 3.2 `coalesce` summed relative deltas

Summing preserves total *distance* but destroys the *shape* of the motion. The
host turns each packet into exactly one `SendInput`, so N small sweeps became one
big teleport.

**`coalesce` now only supersedes ABS** (lossless — older positions are
meaningless) **and drops zero-motion. REL passes through untouched.**

Note the deliberate reversal: a flick out and back that nets to zero used to be
dropped as a no-op. It is two real movements and both are sent
(`motion_that_cancels_out_is_still_two_movements`).

### 3.3 A fixed coalescing window in `session.rs`

`INPUT_COALESCE_WINDOW` was a second rate cap stacked on an already-broken
delivery rate, and it hid how slow that rate was. It was only ever justified for
the reliable control channel's 8-message send window. **Removed — the input task
is now self-clocking:** send immediately, and whatever arrives during the send
coalesces into the next batch.

### 3.4 `relativeAxis` summed `getHistoricalAxisValue`

The same mistake as 3.2, one layer earlier and much better hidden. Android packs
many motion samples into one `MotionEvent`; the extras are reachable only through
`getHistorical*`. Adding them together collapsed an entire batch into a single
host `SendInput`.

**Rule: never sum `getHistoricalAxisValue` for relative input — iterate it.**
Summing is correct only for absolute positions, where only the newest matters.
`emitRelativeSamples` emits one packet per sample, carrying the sub-pixel
remainder across samples (the axes are floats, and truncating each sample
independently discards a large share of slow movement).

### 3.5 The decisive one — the host drained its media socket every 500 ms

`media_supervisor`'s `learn_ticker` (`nova-server/src/lib.rs`) was
`interval(500ms)`, and **in the split architecture that ticker is the only thing
that drains the media socket.** `try_learn_target()` is where
`nova_core::demux`'s Echo hook lives, so it is also the only thing that delivers
Echo's control *and input* datagrams.

Every input packet waited up to half a second in the receive buffer, then arrived
with ~30 neighbours, which the injector applied in ~4 ms at 120 µs each. A burst
of movement, then nothing. The host log showed a healthy 60–90 packets/sec — the
*average* was right, the *distribution* was catastrophic.

500 ms was correct for its original job (GameStream clients ping every 500 ms and
only the newest matters). It became wrong the moment the same drain carried
real-time input. **Now `interval(2ms)`.**

**THE TELL: the measured control RTT was ~500 ms and did not change when the
client moved from carrier NAT to `LAN (direct)`. A latency that is independent of
the network is a polling interval, not a path.**

⚠️ `media_reader_loop` exists because `learn_ticker` racing `recv_media`'s
non-cancellation-safe `read_exact` inside `select!` corrupts frames. At 2 ms that
hazard is far more likely. **Never move the pipe read back into that `select!`.**

---

## 4. Two real findings that were *not* the cause

Both are worth keeping; neither fixed the lag.

**A VPN on the phone.** The punch log showed the phone's own local candidate as
`100.71.118.193` — in `100.64.0.0/10`, carrier-grade NAT — while its public
address differed from the host's. Killing the VPN produced
`10.0.0.188 … LAN (direct)`. **Android shows a WiFi icon while routing over
something else**, so "am I on WiFi?" is not the diagnostic question; "what is the
peer's local candidate?" is. `nova_core::punch::describe_path()` now labels every
punch `LAN (direct)` / `CARRIER NAT (mobile data — not your LAN)` /
`public internet`.

**Windows pointer ballistics.** `inject_mouse_move_rel` uses plain
`MOUSEEVENTF_MOVE` (correct — games read raw input via `WM_INPUT`, so
`SetCursorPos` would be invisible to them), which runs through the pointer-speed
slider and the "Enhance pointer precision" curve.
`input::suppress_pointer_ballistics()` / `restore_pointer_ballistics()` now
disable it for the session, claim-once, restored in `stop_session` **and**
`virtual_display::emergency_restore_for_shutdown` (outside its snapshot
early-return — pointer state is armed even for non-VDD streams). **On the dev box
this is a no-op:** the log showed `pointer acceleration off speed 10` *before*
suppression, i.e. the user already had it disabled. Keep it for machines that
don't; credit it with nothing here.

---

## 5. The transport: input has its own unreliable channel

`nova_core::input_channel`, tag `ECHO_INPUT = 0xE3`, sealed with the session's
`SessionKeys` under `STREAM_INPUT = 3`.

Input used to ride the reliable control tunnel. `nova_core::rudp` is reliable and
**ordered** with `MAX_IN_FLIGHT = 8`, and both properties are actively harmful
for a pointer: eight messages per round trip is a rate ceiling a mouse exceeds,
and one lost datagram head-of-line blocks everything behind it for
`RETRY_INITIAL` (150 ms).

Reliability is replaced by **redundancy + deduplication**: every datagram repeats
the last `REDUNDANCY = 3` packets; the receiver drops any sequence at or below
its high-water mark. The dedup **is** the replay defence (a replayed datagram is
all old sequence numbers), so there is no separate replay window.

**Authorization is the key, not the address.** These datagrams land on the media
socket, which anyone who has seen the punched path can write to, and they end at
`SendInput` under a LocalSystem Master. `SessionManager::inject_sealed_input`
never consults the source address — opening under the session key is the entire
owner check. Tests cover a foreign key, a replay, and a *previous* session's key.

The client repeats the tail 2× at 25 ms when idle, because redundancy only
protects a packet while *later* datagrams carry it — and the last packet of a
burst is exactly the key-up or release-all sent as the app backgrounds.

`inject_sealed_input` releases the session mutex **before** injecting:
`seal_video` takes that same mutex on every video frame, so holding it across the
Worker link would couple the frame path to injection.

---

## 6. Telemetry — use it, don't re-derive it

Every question this session had half its answer on each machine. The client now
reports itself to the host every 2 s:

```
📱 Echo client "<name>": rtt <n>ms (best <n>ms), capture <held|off>,
   input peak <n>/s x<n> samples, batch <n> (worst <n>)
```

Host side, `nova.log`:
```
⌨️  Worker inject/s: <n> packets (<n> rel, <n> abs, <n> other), <n>ms total, <n>µs avg
```
Host side, `nova-service.log`:
```
⌨️  Echo input/s: <n> datagrams, <n> applied, <n> repeats, <n> refused, last inject <n>µs
✅ Punch succeeded: path open to <addr> after <n> round(s) (<proof>) — <path kind>
```
Client overlay: `video <n>ms (worst <n>ms)`, `NETWORK round trip <n>ms`,
`input batch <n>`, `mouse PEAK <n>/s x<n>, touch=<bool> focus=<bool>`.

`client_stats` is a **notification**, trusted for nothing — printed and
discarded, never used for a decision.

### Diagnosis rules earned the hard way

1. **A pointer trailing the hand feels identical whether input is late or the
   video showing the result is late.** Never debug it without separating them.
   `DecodedFrame.first_shard_at` → `QueueStats.last/worst_frame_age_ms` measures
   the video half on one clock with no host time sync.
2. **An input rate equal to the display refresh rate is VBlank buffering**, not a
   network or queue problem.
3. **A latency that doesn't change when the network changes is a polling
   interval.**
4. **When all local telemetry is green and the user still reports lag, measure
   the path** — and check the peer's *local* candidate first.
5. **For relative input, packet COUNT is the signal, not summed magnitude.**
   Anything that reduces the count reduces smoothness one-for-one.

---

## 7. FIXED: the white film — ancestor backgrounds paint over a SurfaceView

**Symptom.** A translucent white/bright wash over the video. Appears when the
mouse is used; **clears the instant the screen is touched**; returns on the next
mouse movement. Also disappears while the control panel is open and returns when
it closes. Colour is accurate when it is absent.

**What is established.**
- It tracks Android's **touch mode** exactly (touch clears it, mouse restores
  it). That is the precise condition under which Android paints a focus
  highlight: a focused view *outside* touch mode.
- It is **not** a cursor layer — it occurs while `capture held`, so no cursor is
  drawn.
- It is **not** a colour-space, HDR, or hardware-overlay-plane problem. An
  earlier hypothesis blamed compositing; the touch-mode correlation rules it out.
- It is **not** Compose `LocalIndication`. The container is
  `Box(Modifier.fillMaxSize())` wrapping `AndroidView(Modifier.fillMaxSize())` —
  no `clickable()`, no `focusable()`, no `indication`. Passing `indication = null`
  would be a no-op; verified before suggesting it.

**THE FIX (confirmed): clearing ancestor `background` drawables.**

A `SurfaceView` does not draw into the window — it gets its own layer *beneath*
it and punches a transparent hole in the window above. So anything an ancestor
paints, **including a background**, lands in the window layer **over** that hole,
i.e. over the video. A container background carrying a `state_focused` entry
therefore washes the entire picture the moment the view takes focus outside touch
mode, and clears the instant a touch returns it to touch mode.

This was the last thing tried because of an intuition that is correct everywhere
*except* over a SurfaceView: that a background renders harmlessly behind its
content. **Over a SurfaceView, "behind the view" and "behind the video" are
different places.** The strip skips the view itself and the DecorView (whose
background is the window's own).

**Everything below was attempted first and did not fix it.** Kept because each
one rules out a mechanism, and because leaving them in place costs nothing and
prevents a regression from a different direction.
1. `defaultFocusHighlightEnabled = false` on the view.
2. The same on **every ancestor** — the Compose `AndroidView` container is a
   focusable `ViewGroup` that decorates independently.
3. `onHoverChanged` overridden to refuse the hovered drawable state
   (`View.onHoverEvent`'s default calls `setHovered(true)`, a drawable state
   change).
4. `foreground = null` on the view and all ancestors.
5. `onDrawForeground` overridden to draw nothing — that pass is the *only* one
   painting scrollbars, foreground, and the default focus highlight, so this
   removes the question of which was responsible.
6. Re-running the ancestor strip on every focus gain, since a Compose container
   can set a foreground *after* attach.
**The lesson worth keeping.** Six of those seven attempts were guesses about
*which view* was decorating, when the question that mattered was *which drawing
pass reaches the video*. Over a SurfaceView that is a different question, and it
has one answer: everything an ancestor paints. If a similar artifact ever appears
again, walk the ancestors logging `background`/`foreground`/class and observe it
rather than disabling candidates one at a time.

---

## 8. Microphone passthrough (Phase 7) — DONE

Phone mic → Opus → sealed UDP → Nova Master → decode + jitter buffer → WASAPI →
**VB-CABLE**, selectable in Windows as "CABLE Output".

**Live result, real conversation (2026-08-16):** 6601 packets, 13 lost (0.2%),
every loss concealed, 0 late, 0 reordered, 0 refused, **`dropped 0`** (the
clock-drift correction never fired in over two minutes), buffer depth 1–2
packets (20–40 ms).

### 8.1 The constraint that shaped it

The bundled Virtual Audio Driver (VAD by MTT, `ROOT\VirtualAudioDriver`)
**cannot load — CM_PROB 52.** Its `.sys`/`.cat` are validly code-signed
(SignPath/GlobalSign) but **not Microsoft attestation-signed**, and kernel-mode
audio drivers require attestation under Secure Boot. MttVDD loads because IddCx
is user-mode. **Do not enable test-signing or disable Secure Boot to work around
this.** VB-CABLE is attestation-signed and installs normally; it is the endpoint.

### 8.2 THE MEASURED FACT — do not re-litigate

**A LocalSystem process in Session 0 CAN render WASAPI audio that a
user-session application captures at full amplitude.**

This is the one place Session 0 did *not* cripple a subsystem, and it is the
opposite of what this project's history would lead you to guess — WGC's broker
refuses SYSTEM, `SendInput` is session-local, and UIPI silently swallowed
injected input while returning success. It was **measured, not assumed**:
`--mic-probe render` under a SYSTEM scheduled task, with `--mic-probe listen` in
the interactive session, plus a user-session control run. Both heard the tone at
peak 0.3500, identical.

So the renderer lives in the **Master**, with no Worker IPC hop — and it inherits
a real benefit: the Master never restarts, so the microphone survives the Worker
respawns that sign-out and lock-screen transitions cause.

The probe is kept, deliberately: `--mic-probe <render|listen|pipeline>` tests the
whole audio stack — endpoint resolution, Opus decode, jitter buffer, WASAPI —
with **no phone, no session, and no network**. `pipeline` drives the real
renderer with real Opus. It costs nothing unless invoked.

### 8.3 Transport: a sibling of `input_channel`, not a copy

`nova_core::mic_channel`, tag `ECHO_MIC = 0xE4`, sealed under `STREAM_MIC = 4`.
Two of `input_channel`'s decisions are **deliberately reversed**:

- **No redundancy.** Input repeats the last 3 packets because a lost key-up
  strands a key held on the host. Audio carries no state — a lost 20 ms packet
  is 20 ms of concealment and the next one is already correct. Tripling the cost
  of a phone's uplink forever to insure a loss that heals itself in a twentieth
  of a second is a bad trade.
- **A 64-packet sliding window, not a high-water mark.** Dropping anything
  behind the mark is right for a pointer (an older delta describes a superseded
  position) and wrong for audio: a packet that arrives late but ahead of playout
  is perfectly good, and discarding it manufactures a gap the network did not
  cause. `reordered`, `late`, and `duplicates` are reported separately.

**Window placement runs only after authentication.** Advancing it on an
unverified sequence would let anyone who can write to the socket push `highest`
forward and make every genuine packet read as late — a whole-session denial of
service for one forged datagram. There is a test for it.

`STREAM_MIC` being distinct from `STREAM_INPUT` is what stops a captured mic
datagram being replayed into the input path, where it would reach `SendInput` on
a LocalSystem service. Tested in both directions.

### 8.4 The codec-config trap — the AV1/DKIF bug in another codec

**MediaCodec emits the Opus identification header, pre-skip, and seek pre-roll as
`BUFFER_FLAG_CODEC_CONFIG` buffers before any audio.** Those are container
metadata, not audio. The host feeds what it receives straight to an Opus decoder,
so passing one along corrupts the stream with nothing in any log to say why.

This is the *identical shape* to the AV1 failure that cost days: the NVIDIA SDK
wrapper prepended an IVF file header and a 12-byte frame header to every AV1
frame, Moonlight handed the payload to its decoder unchanged, and every frame was
undecodable while H.264 and HEVC worked perfectly. The filter lives in
`MicCapture.drainEncoder`, next to the flag, and drops with a log line.
**General rule: check what a codec wraps its output in before trusting the
bytes.**

### 8.5 Why Opus is encoded in Kotlin, not Rust

`audiopus` builds libopus from C via cmake, and cross-compiling that under
`cargo-ndk` is the same fight the workspace manifest documents about
`aws-lc-rs` — for a codec Android has had in MediaCodec since **API 29**.
Encoding in Kotlin keeps `libecho.so` free of a new native dependency. The host
decodes with `audiopus`, which it already links for GameStream audio.

Note the floor: `minSdk` is 26 but the Opus **encoder** needs API 29. On an older
device `MicCapture` reports the refusal in the overlay and the stream carries on
without a microphone.

### 8.6 The jitter buffer — three failure modes, three answers

- **Gap** (packet lost, later ones present) → Opus concealment. Better than
  silence, much better than skipping ahead.
- **Starved** (nothing at all) → silence, deliberately *not* concealment.
  Extrapolating through a long absence smears into a robotic artefact; the honest
  rendering of "they stopped talking" is quiet.
- **Clock drift** → the one with no natural bound. Phone and endpoint disagree by
  parts per million, so the buffer walks one way until it is empty or seconds
  deep. Corrected at the bound by dropping the **oldest** packet: the newest is
  what the speaker just said, and the backlog in front of it is pure latency.

The renderer is a plain OS thread, not a tokio task — it holds COM state with
thread affinity and sleeps in fixed steps, and putting that on the shared runtime
would let a stalled render step delay unrelated network work. That is the same
coupling `learn_ticker` caused in §3.5.

### 8.7 Ownership and routing

`audio.rs` remains the sole owner of default-endpoint state
(`ORIGINAL_ENDPOINT`, claim-once). The mic path **never touches it** — it opens
its endpoint *by name*, so there is no second owner by construction. The shim's
mic render holds its own COM state (`g_mic*`) and never touches
`SHIM_CAPTURE_ACTIVE`, which guards *capture*: a stream starting must not tear
down the microphone, or vice versa.

`rtp.rs` routes `Class::EchoMic` to **its own channel**, never the control inbox.
Two reasons, and both matter: `echo::transport`'s accept loop treats an
unrecognised datagram from a new address as the opening of a TLS tunnel, so every
mic packet from an unlatched peer would attempt a handshake; and putting a
50-per-second decode-and-render job on the loop that carries input would make the
microphone's cost part of input's latency budget — §3.5 again.

`SessionManager::open_sealed_mic` **returns** the packet rather than rendering
it, so the session mutex — the one `seal_video` takes for every video frame — is
released before the handoff by construction.

### 8.8 Telemetry, and a metric that lied

`nova-service.log`, every 10 s while audio flows:
```
🎤 Echo mic: <n> applied, <n> lost, <n> late, <n> reordered, <n> refused
   | rendered <n>, concealed <n>, underran <n>, paused <n>, dropped <n>, depth <n> (worst <n>)
🎤 Echo client mic: <n> packets, <n> KiB sent, <n> refused, worst gap <n>ms
```

Both halves on purpose: network counters say what arrived, renderer counters say
what was done with it, and the client line says whether the phone produced it at
all. Silence with healthy network counters is a renderer problem; the reverse is
a path problem; a large client-side `worst gap` is the phone's encoder stalling,
which looks *identical* to packet loss from the host.

**The first version counted `starved` as one number and a healthy two-minute
conversation reported 1115 of them** — because an empty buffer is exactly what a
pause between sentences looks like. Now split: **`underran`** (empty steps
followed by more audio — the client was still talking and the buffer ran dry;
**this is the one that means something is wrong**) and **`paused`** (the run
lasted until the renderer idled — somebody stopped speaking). A run cannot be
classified while it is happening, only by how it ends, which is why
`SilenceRun` exists.

### 8.9 Footguns guarded in code

- **`[audio] endpoint_override` pointing at CABLE Input is refused at startup**,
  with the reason. It is a reasonable-looking choice — VB-CABLE *is* a virtual
  sink — and it would render every sound the host makes into the same cable the
  microphone feeds, so the remote user hears the game as their own mic input.
- **No VB-CABLE installed** → one log line, no renderer, no inbox, and mic
  datagrams are dropped in `rtp.rs` for the cost of a byte comparison. The
  feature is optional in every direction.
- **Mute fully releases `AudioRecord`** rather than gating the send: Android
  keeps the mic indicator lit for an open recorder, and a muted app that keeps
  the light on is lying to the user.

---

## 9. Files touched today

| File | What |
|---|---|
| `nova-core/src/input_channel.rs` | **new** — sealed unreliable input datagrams |
| `nova-core/src/demux.rs` | `ECHO_INPUT = 0xE3`, `Class::EchoInput` |
| `nova-core/src/media_crypto.rs` | `STREAM_INPUT = 3` |
| `nova-core/src/punch.rs` | `describe_path()` |
| `nova-server/src/lib.rs` | `learn_ticker` 500 ms → **2 ms**; Worker inject telemetry |
| `nova-server/src/echo/transport.rs` | input dispatch before tunnel logic; `⌨️ Echo input/s` |
| `nova-server/src/echo/session.rs` | `inject_sealed_input`, `InputRejection`, per-session `InputReceiver` |
| `nova-server/src/echo/rpc.rs` | `client_stats` |
| `nova-server/src/echo/wan.rs` | path kind on punch success |
| `nova-server/src/input.rs` | pointer-ballistics suppress/restore; batch stats |
| `nova-server/src/rtp.rs` | route `Class::EchoInput` |
| `echo-client/src/input.rs` | `coalesce` no longer merges REL; batch + UI stats |
| `echo-client/src/session.rs` | self-clocking input task; RTT probe; `client_stats` |
| `echo-client/src/receiver.rs` | `DecodedFrame.first_shard_at` |
| `echo-android/src/*` | `nativeReportUiState`, frame-age stats |
| `android/.../StreamSurfaceView.kt` | `onHoverEvent`, `emitRelativeSamples`, capture diagnosis, film attempts |
| `android/.../MainActivity.kt` | capture state that survives the panel, telemetry line |

### Microphone (Phase 7)

| File | What |
|---|---|
| `nova-core/src/mic_channel.rs` | **new** — sealed unreliable mic datagrams, sliding window |
| `nova-core/src/demux.rs` | `ECHO_MIC = 0xE4`, `Class::EchoMic` |
| `nova-core/src/media_crypto.rs` | `STREAM_MIC = 4` |
| `nova-server/shim/audio_shim.cpp` | `FindEndpointByName`, `InitMicRender`/`RenderMicFrames`/`CleanupMicRender`, probe capture |
| `nova-server/src/mic.rs` | **new** — Opus decode, jitter buffer, drift correction, render thread |
| `nova-server/src/mic_probe.rs` | **new** — `--mic-probe render\|listen\|pipeline` |
| `nova-server/src/echo/session.rs` | `open_sealed_mic`, per-session `MicReceiver` |
| `nova-server/src/rtp.rs` | `mic_inbox`, `Class::EchoMic` routing |
| `nova-server/src/lib.rs` | `spawn_mic_passthrough`, `🎤 Echo mic` telemetry |
| `nova-server/src/echo/rpc.rs` | client-side mic stats in `client_stats` |
| `echo-client/src/mic.rs` | **new** — client counters, worst inter-packet gap |
| `echo-client/src/session.rs` | `Uplink` struct, mic send task |
| `echo-android/src/lib.rs` | `nativeSendMic` (direct ByteBuffer), mic channel |
| `android/.../MicCapture.kt` | **new** — AudioRecord + MediaCodec Opus, codec-config filter |
| `android/.../EchoController.kt`, `MainActivity.kt`, `EchoService.kt`, `AndroidManifest.xml` | lifecycle, toggle, permission, FGS type |

**Tests:** 241 green across the workspace (13 echo-android + 51 echo-client +
89 nova-core + 88 nova-server).

⚠️ **Correction to an earlier note in this file.** It used to say
`cargo test --workspace` fails on `echo-android` because an Android-target cdylib
was being run on the Windows host. **That was wrong.** The real cause was a stale
test fixture — `DecodedFrame.first_shard_at` was added to `echo-client` and
`echo-android/src/frames.rs`'s helper was never updated. One line; its 13 tests
now run on the host like any other crate. The crate is still excluded from
`default-members`, but for the feature-unification reason the workspace manifest
gives, not this one.

**Deploy:** `cargo build --release -p nova-server`, stop `NovaService`, copy
**both** `nova-server.exe` and `nova_shim.dll`, start it — **and assert the
deployed timestamps match the source**, which has silently shipped a stale binary
once. When the shim changes, also grep the built DLL for the new export names
before trusting it.

⚠️ **Do not deploy while a session is live.** Deploying restarts `NovaService`,
which kills the stream — including a microphone conversation in progress.

---

## 10. Host game audio — the "House Party" bug — **DONE (2026-08-16)**

> **This section is now history.** Downstream game audio, ghost-sink isolation
> and the A/V sync engine all shipped and are live-confirmed — see
> **`HANDOFF_ECHO_AUDIO.md`**, which supersedes everything below and records the
> four bugs, the 190 ms latency budget, and the sync engine.
>
> The prediction below turned out correct on both counts: the ghost-sink
> machinery *did* already exist and needed auditing rather than rebuilding, and
> `CABLE Input` *was* still in `kVirtualSinkNames` and had to come out. What the
> note did not anticipate is that removing it from that one list would have
> **moved** the bug — `FindRealAudioDevice` selected by negating the same list,
> so the cable would have become somewhere crash recovery restores the default
> output onto. The list is now two lists.

The physical PC speakers blast game audio during a stream. The fix is a ghost
sink: switch the default render endpoint so Windows migrates application streams
off the speakers, and loopback-capture that sink for the stream. **This machinery
already exists** (Phase 15.1/15.6: `audio::SinkGuard`, `arm_endpoint_restore`,
`ORIGINAL_ENDPOINT` claim-once, `IPolicyConfig::SetDefaultEndpoint` across all
three ERoles). Audit before re-implementing — that mistake was already made once.

**The one hard rule this phase adds: the ghost sink must NOT be VB-CABLE.**
The microphone renders *into* CABLE Input. Routing host audio there too puts the
game into the same cable the phone's voice feeds, so the remote user hears the
game as their own microphone — and with "Listen to this device" enabled anywhere,
a feedback loop. `mic.rs::collides_with_ghost_sink` already refuses this at
startup with an explanation; do not weaken it, and do not work around it.

**Planned sink preference, in order:** `Steam Streaming Speakers` →
`NVIDIA Virtual Audio` → anything the operator names in `[audio]
endpoint_override`. Both of the first two are present on the dev box; the
built-in list in `audio_shim.cpp` (`kVirtualSinkNames`) already carries Steam's
and would need `CABLE Input` **removed** or the resolver taught to exclude the
mic's endpoint, or the ghost sink will pick the cable by itself.
