# Echo — Downstream Game Audio and the A/V Sync Engine (2026-08-16)

Read this before touching Echo's audio path or the frame queue. It covers the
day host→client game audio went from silent to synchronised, and it records the
wrong turns, because most of them are re-tempting and two of them cost a full
round each.

Companion to `HANDOFF_ECHO_INPUT.md` (§8 covers the microphone, the upstream
mirror of everything here).

---

## 1. Status

**Downstream game audio is DONE and live-confirmed.** Steady state:

```
audio/10s: arrived 501/500, played 501/500 | 0 silence, 0 concealed, 0 held,
           0 drift-dropped, 0 shed, depth 3, 0 new track underruns
           | latency 190ms = jitter 60ms + output 130ms
             [track 40ms, decoder 20ms, fastpath true]
```

**A/V sync is DONE and live-confirmed** — user reports "absolute perfection,
zero visual hitching" with the toggle on.

**Not started:** software Opus PLC on the client (§6), gamepad over Echo.

---

## 2. The thing to understand before anything else

**Capture and encode were never the work.** Nova has captured the ghost sink and
encoded Opus since long before Echo existed, and a Moonlight client receives
exactly that on RTP port 48000. Echo was silent for one reason: **an Echo client
listens on nothing but its hole-punched path**, and port 48000 is a socket it
never opened.

So Phase 9 forked the bytes at the Master's `AudioFrame` hop and added framing.
It added no capture code, no encoder, and no codec.

This matters because the instinct on being told "the stream has no sound" is to
go build an audio pipeline. One already existed. **Audit before implementing** —
that mistake has now been made more than once on this project, and `HANDOFF_ECHO_INPUT.md`
§10 warns about it for the ghost sink specifically.

---

## 3. Architecture

```
Worker: WASAPI loopback (ghost sink) → Opus 48k stereo 128k, 20ms frames
   │  MediaMsg::AudioFrame over the media pipe
   ▼
Master: lib.rs AudioFrame hop — FORKS
   ├── existing RTP:48000 send, untouched          → Moonlight
   └── SessionManager::seal_audio → ECHO_AUDIO 0xE5 → Echo
   ▼
Client: rtp demux → own task → echo_client::audio::AudioBuffer (jitter, Rust)
   ▼
Kotlin: GameAudioPlayer — MediaCodec Opus decode → AudioTrack
```

`nova_core::audio_channel`, tag `ECHO_AUDIO = 0xE5`, sealed under
`STREAM_AUDIO = 2` — which `media_crypto` had already reserved and tested for
exactly this and never used.

**Rust owns *when*, Kotlin owns *what*.** The jitter buffer, sequence window,
drift correction and telemetry are in `echo-client/src/audio.rs`, unit-testable
on a desktop. Kotlin renders whatever step it is handed.

### Why decoding is in Kotlin

Not a preference — a wall, verified rather than assumed:

```
$ cargo ndk -t arm64-v8a build -p echo-android
error: could not find native static library `opus`
```

`audiopus` builds libopus from C through cmake and does not cross-compile for
Android. Same wall the workspace manifest documents for `aws-lc-rs`, and the
same reason the microphone's *encoder* is in Kotlin.

### Two deliberate reversals, inherited from the mic channel

- **No redundancy.** Audio carries no state; a lost 20 ms packet heals itself.
- **A 64-packet sliding window, not a high-water mark.** A packet that arrives
  late but ahead of playout is good audio; discarding it manufactures a gap the
  network did not cause.

`STREAM_AUDIO` being distinct from `STREAM_MIC` is what stops a captured
downstream datagram being replayed upstream into `SendInput` territory. Tested
both directions.

---

## 4. The four bugs, in the order they had to be fixed

Each one masked the next, and the first two were mine from the commit before.

### 4.1 Silence transposed ahead of decoded audio

Silence was written **straight to the AudioTrack**, while a packet's PCM only
emerged from the decoder an iteration later — and the drain ran at the *bottom*
of the loop:

```
step N    PACKET  → queued; drain → nothing yet, codec priming
step N+1  SILENCE → silence written; drain → packet N
          track receives [silence][packet N]     ← transposed
```

Two discontinuities per swap. **Fix: drain at the TOP of the loop.**

**No Rust test can catch this.** The buffer's ordering is correct and covered;
the transposition happens entirely downstream of it, between two different write
paths into one track. Worth remembering when a symptom survives a green suite.

### 4.2 The loop ran below realtime

`dequeueInputBuffer` waited a full frame *and* the track write blocked — two
serial waits per 20 ms, so the loop could not sustain 50 Hz.

**Only the track write may block.** It is the one genuinely realtime clock in
the loop; anything else that waits spends the same 20 ms budget. The input
dequeue is now non-blocking, and a packet it cannot place is **held** in a
one-slot stash, never dropped — that packet has already left the jitter buffer,
so discarding it manufactures a gap.

Draining now waits a quarter-frame for its first output buffer. That wait is
load-bearing: retrieving an output buffer is also what **recycles the input
buffer**, so an instant give-up lets the codec saturate and blocks the input
dequeue — the same stall by another route.

### 4.3 The jitter window was too narrow for bursty arrival

**THE TELL, and the best diagnostic moment of the session:**

```
arrived 500/500, played 501/500 | 3 silence, 0 concealed, 3 drift-dropped, depth 5
arrived 506/500, played 506/500 | 2 silence, 0 concealed, 2 drift-dropped, depth 7
arrived 502/500, played 502/500 | 6 silence, 0 concealed, 6 drift-dropped, depth 6
```

Rates match exactly, nothing lost, and yet the buffer runs **dry and overflows
in the same ten seconds** — with `silence` *equal to* `drift-dropped` on every
line. **Two opposite failures in lockstep are one cause**: packets arrive in
bursts (Wi-Fi aggregation and power-save), so depth swings wider than the window
spans and clips at both ends, costing a pop at each end of every burst cycle.

40–160 ms → **80–280 ms**. The old numbers were inherited from the host's
microphone buffer, which is fed by a phone's own uplink and paces its own
packets. Nothing about that sizing survived a trip through an access point.

### 4.4 Playout started at the oldest buffered packet

The host's loopback goes quiet between tracks, so a track beginning delivers a
**burst**. Playout began at the *oldest* packet of it — hundreds of milliseconds
behind live — and `MAX_DEPTH` then clawed it back one audible 20 ms excision at
a time. That is "garbled at the start of every song, then it straightens out".

Playout now starts within `START_DEPTH` of the **newest** packet and discards the
backlog in one silent step. Same latency shed, inaudibly, because none of it has
been played yet.

---

## 5. Latency, and the API that actually controls it

Final budget on the reference device, all measured:

| Term | ms | Movable? |
|---|---|---|
| Jitter buffer (`START_DEPTH` 4 × 20 ms) | 60–80 | Only by giving up burst tolerance |
| AudioTrack buffer | 40 | At the device's burst floor |
| MediaCodec decoder | 20 | No |
| Device output stage (HAL) | ~70 | No — fast path already granted |
| **Total** | **190** | |

### `setBufferSizeInBytes` is a floor and an allocation hint, NOT a limit

We asked for 1920 frames (40 ms). The platform allocated **13440 frames
(280 ms)** and reported the fast path granted anyway. A small request is rounded
up and silently ignored.

**`setBufferSizeInFrames`, called after build, is the real lever.** It caps how
much of the *allocation* the client may fill, which is what the hardware drains
and therefore what the latency is. It only shrinks, so asking small is safe.

Floor it at the device's `PROPERTY_OUTPUT_FRAMES_PER_BUFFER` — below one burst
the mixer cannot be serviced without underrunning, and that property is the
platform stating where the line is.

`PERFORMANCE_MODE_LOW_LATENCY` is a **request**. Read `performanceMode` back;
never assume it was granted.

### Latency shedding — how the buffer gets back DOWN

Depth only ever ratchets up otherwise: arrival and playout rates are identical
in the steady state, so whatever depth a hiccup leaves is kept forever. A live
run sat at **depth 13 of 14 for eighty seconds** — 260 ms bought by one Wi-Fi
hiccup a minute earlier and never given back. `MAX_DEPTH` is a ceiling, not a
spring.

Above target, playout may **skip the packet due now if it is under 120 bytes**.
The host encodes `Application::LowDelay` with VBR left on, so libopus spends bits
on content and a near-silent frame collapses to a few dozen bytes against a ~320
byte average — **packet size is a decoder-free proxy for loudness**. Skipping at
playout is a true splice: no hole, nothing concealed.

Quiet-only was too strict on its own (`depth 12, shed 0` for a whole track —
music has no lulls), so after five seconds above target it sheds any packet. One
20 ms click, once, against a permanent quarter-second of lip-sync error.

---

## 6. Known limitation: no PLC

`MediaCodec` exposes no packet-loss-concealment entry point, so a lost packet is
a frame of silence where the host's microphone path gets genuine Opus
extrapolation.

`AUDIO_CONCEAL` is nonetheless a **distinct step** from `AUDIO_SILENCE`, even
though both currently render the same way. That keeps `concealed` and `underran`
from collapsing into one meaningless number, and it is the single branch a real
PLC decoder slots into. Do not merge them to "simplify".

---

## 7. The A/V sync engine

Audio sits at a hardware floor and cannot be hurried; video renders as soon as
it decodes. **So video is delayed to meet audio** — the only direction with
slack, and what every media player does.

- The delay lives in `FrameQueue`, measured from each frame's `first_shard_at`
  (the same clock `last_frame_age_ms` uses), so it is a fixed offset from
  *arrival* and cannot compound with how often the feeder asks.
- `pop_timeout` waits for whichever comes first: the caller's timeout, or the
  head frame coming due. Sleeping the full timeout when a frame is due sooner
  would make the delay the caller's poll interval — the exact mistake
  `learn_ticker` made on the host (`HANDOFF_ECHO_INPUT.md` §3.5).
- It tracks the audio pipeline's measured latency automatically, because the
  device output term cannot be derived, only read back from the track.

### ⚠️ `CAPACITY` must scale with the delay — this one freezes the picture

`FrameQueue::CAPACITY` is 3 because a backlog normally means the decoder is
behind, and the answer is drop-and-re-gate. **A delay line makes a backlog the
normal state** — eleven frames in flight at 190 ms and 60 fps.

Left at 3, the overflow path fires on every frame, and overflow here **closes the
keyframe gate**. Under Nova's infinite GOP that is a permanently frozen picture
the moment sync is switched on. Capacity now scales with the delay, sized against
120 fps rather than the negotiated rate. There is a test named after the failure.

### The soft number

`VIDEO_PIPELINE_MS = 30` (EchoController) is subtracted from the audio
measurement, because sync needs the **difference** between the paths, not audio's
figure outright. It is an estimate — MediaCodec exposes no `AudioTimestamp`
equivalent for a Surface — and it is the one value here that was not measured.
**If sync lands consistently off in one direction, this is the constant to
correct.** Live-validated as "spot on" on the reference device.

Updates are gated on `arrived > 0` (see §8) and on a 25 ms hysteresis, since the
measurement breathes a few ms per report and re-timing video for that is a
visible hitch buying nothing perceivable.

**Off by default.** It buys sync with input latency; right for watching, wrong
for playing. The overlay toggle says so.

---

## 8. Diagnosis rules earned the hard way

1. **`played 386/500` with `arrived 0/500` is the IDLE loop, not a deficit.**
   `Thread.sleep(20)` really takes ~26 ms on a phone, giving 386 ticks per 10 s.
   A whole rate-mismatch theory got built on that number before anyone noticed no
   audio was flowing. **Check `arrived` first, always.**
2. **Cumulative counters read as "fine" when frozen.** Report rates and deltas.
   `299 drift-dropped` unchanged across five reports looks healthy and means a
   burst happened earlier and stopped.
3. **Report both sides of the link.** With only `played`, "host sending slowly"
   and "client consuming slowly" are indistinguishable. `arrived` next to
   `played` is what finally split them.
4. `lost` is a property of the **receive task** and says nothing about playout —
   they run in different tasks. Misreading this sent one round after the host.
5. **Two opposite symptoms in lockstep are one cause** (§4.3).
6. A symptom that survives a green test suite is probably in a seam the tests
   cannot reach (§4.1).

### Getting the logs at all

The device buffer rotates fast enough to lose a whole test:

```bash
adb logcat -c && adb logcat -G 16M
adb logcat -s EchoAudio:V > capture.log &     # background capture
```

Tag is `EchoAudio`. **The app must be relaunched after `adb install -r`** or the
old process is still what you are watching.

---

## 9. Files

| File | What |
|---|---|
| `nova-core/src/audio_channel.rs` | **new** — sealed host→client audio, sliding window |
| `nova-core/src/demux.rs` | `ECHO_AUDIO = 0xE5`, `Class::EchoAudio` |
| `nova-server/src/echo/session.rs` | `seal_audio`, per-session `AudioSender`, 20 ms `ConfigureStart` |
| `nova-server/src/lib.rs` | the fork at the `AudioFrame` hop |
| `nova-server/src/rtp.rs` | inbound `ECHO_AUDIO` blackholed (reflection guard) |
| `nova-server/shim/audio_shim.cpp` | ghost-sink resolver split into two lists |
| `echo-client/src/audio.rs` | **new** — jitter buffer, shedding, playout steps |
| `echo-client/src/receiver.rs` | `Class::EchoAudio` → its own channel |
| `echo-client/src/session.rs` | dedicated audio task, `Uplink.audio` |
| `echo-android/src/frames.rs` | the delay line + capacity scaling |
| `echo-android/src/lib.rs` | `nativePollAudio`, `nativeSetVideoDelay`, stats |
| `android/.../GameAudioPlayer.kt` | **new** — MediaCodec decode, AudioTrack, telemetry |
| `android/.../EchoController.kt` | sync engine, latency tracking, lifecycle |
| `android/.../MainActivity.kt` | the sync toggle |

**Tests:** 276 across the workspace.

---

## 10. Ghost-sink isolation (the prerequisite)

`audio_shim.cpp` used one list for two different questions. It now has two:

- `kGhostSinkNames` — **ordered** preference for where host audio may be *sent*:
  Steam Streaming Speakers → NVIDIA Virtual Audio. **VB-CABLE is absent**; the
  microphone renders there, and a ghost sink on the same cable feeds the game
  back to the remote user as their own microphone.
- `kNotPlaybackNames` — a **superset**, used only negated, for where crash
  recovery may restore the default output. Deleting VB-CABLE from a single shared
  list would have *moved* the bug here rather than fixing it.

Two latent bugs fell out of that split: list order was never preference order
(it iterated *endpoints* outermost, so WASAPI enumeration order was the real
tiebreak), and `CABLE In 16ch` matched no entry and was therefore already
eligible as a "real" output. Endpoint matching also reads
`PKEY_DeviceInterface_FriendlyName`, because NVIDIA names its render endpoints
after the attached display — the adapter name is the only place "NVIDIA Virtual
Audio" appears.

---

## The microphone cable is now per-session (2026-08-20)

Reported as: on detach, Nova held the Virtual Audio Cable instead of restoring
the physical endpoints, and the mic was broken after reconnecting. Half of that
was already handled and half was real. Both halves are worth recording, because
the already-handled half is the one most likely to be "fixed" again by mistake.

### Already correct — the ghost sink (speakers). Do not re-implement.

`WorkerMediaPlane::end` sends `Deactivate { cancelled }` for **both** end modes,
and the Worker's `deactivate_worker` (lib.rs) opens with an unconditional
`audio_manager.stop_and_release()` — which joins the capture thread and runs the
claim-once `restore_original_endpoint()`. So a detach already restores the host's
real default output.

The resume side is equally covered: `apply_configure_start` calls
`audio::arm_endpoint_restore()` on **both** branches — the fast
`resume_suspended` reclaim and the full activation — and the Worker calls
`audio_manager.start_for_stream(...)` after every `Configure`, which a reclaim
also sends. The ghost sink is therefore re-engaged on an instant reconnect
without anything new being written.

### Real, and fixed — the microphone renderer was process-lifetime

`mic::start()` was called once from the Master's `mic_supervisor` at startup, and
`render_loop` called `InitMicRender` before its loop and `CleanupMicRender` only
after it exited. The loop only exits when the sink is dropped, and the sink lives
as long as the process. **Nova therefore held VB-CABLE's render endpoint open
from service start to service stop** — across every detach, every network
switch, and every idle hour between sessions. Nothing in the session lifecycle
touched it.

Now: `mic::session_started()` / `mic::session_ended()` set a `WANTED` flag, and
the render loop acquires the endpoint when it goes true and releases it when it
goes false. The hooks sit in `WorkerMediaPlane::begin` and `::end`, which is the
choke point every ending passes through — an explicit `stop_session`, a detach on
silence, the reaper expiring a detached session, the tray's force-end, and a
restart ending the previous session. Putting them at any one call site would have
missed at least three of those.

**Two details that are load-bearing:**

1. **The transition happens on the render thread**, between playout steps, by
   polling the flag — not by another thread closing the device. Releasing a
   WASAPI render client underneath a live `RenderMicFrames` is the shape of race
   that produces an intermittent crash in a LocalSystem service.
2. **The jitter buffer is reset on release.** `next_seq` from a finished session
   would make every packet of the next one look like a late arrival and be
   dropped — which is exactly "the mic is broken after reconnecting". Carrying
   the buffer would also play the departing client's last words to whoever
   connects next.

`begin` runs for a reclaimed session as well as a fresh one, so an instant
reconnect re-acquires the cable with no extra path.

### NOT fixed, because Nova cannot: the default *capture* device

If the host's microphone is silent in other applications, that is the documented
VB-CABLE behaviour (see CLAUDE.md): installing the cable adds a **capture**
endpoint, "CABLE Output", and Windows makes a newly-arrived capture device the
default. Every app then reads digital silence from a device that is working
exactly as designed.

Nova has never called `SetDefaultAudioDevice` with a capture endpoint — only
render endpoints, for the ghost sink — so there is nothing for it to restore, and
adding an automatic "fix" would mean silently overriding a device choice the
operator may have made deliberately. Diagnose and repair it with the shipped
tool:

```
nova-server.exe --mic-probe listen default 5 <log>
nova-server.exe --mic-probe default "Microphone" 0 <log>
```

---

## Microphone endpoint routing — Nova now sets the default CAPTURE device (2026-08-20)

**This reverses a decision recorded in CLAUDE.md as deliberate.** The old rule
was "Nova has never called `SetDefaultAudioDevice` with a capture endpoint, and
doing so would mean silently overriding a device choice the operator may have
made on purpose." That reasoning was sound *at the time* for a specific reason:
nothing on the capture side had a restore path, and an override with no way back
is a setting the user has to repair by hand — worse than doing nothing.

What changed is not the judgement about overriding, it is that the way back now
exists, built to the same standard the render side has had since Phase 15.1.

### What it does

`audio::engage_default_capture()` finds the `CABLE Output` **capture** endpoint
(the recording half of the cable `mic.rs` renders into), records the operator's
current default recording device, and points Windows at the cable via the same
`IPolicyConfig` swap the ghost sink uses. `restore_default_capture()` puts it
back, claim-once.

New shim export: `GetDefaultEndpointId(is_capture, out, cch)`.
`GetDefaultAudioDeviceId` is render-only and stays that way — it has callers
that would silently start answering a different question if it grew a parameter.
`FindEndpointByName` already took an `is_capture` flag and needed no change.

### Four guards, each load-bearing

1. **It engages on TRAFFIC, not on session start.** The hook is in `mic.rs`'s
   render loop, the first time a session's jitter buffer is non-empty. A user
   who never switched their microphone on never has their recording device
   touched, and "packets are arriving" is the only evidence this side of the
   wire has that the switch is on — the client sends, or it does not.
2. **It refuses to arm the cable as a restore target.** If the default is
   already `CABLE Output` — an unclean previous exit, or the operator's own
   choice — nothing is armed and nothing is swapped. Arming it would make
   "restore" mean "put the cable back", which is the stuck-silent bug the render
   side documents.
3. **The restore target is stored only after the swap SUCCEEDS.** Arming first
   would leave a restore target for a change that never happened, and the
   restore would then move the operator's default for no reason.
4. **There is no live-query fallback, deliberately.** The render side falls back
   to `recover_stuck_sink()` when nothing is armed, because "any real playback
   device" is a safe guess. The equivalent guess here is not: picking "some real
   capture device" could hand the operator's default to a webcam they never use.
   Nothing armed means we never changed it, so the right action is none.

### The cross-process net

`nova_mic_restore.txt`, next to the exe, written when the swap engages and
removed when it is undone. The in-memory arm covers every ending this process
runs code for; it does not cover the Master being *terminated* — an installer
upgrade, `sc stop` landing mid-call, a crash — where a detached render thread
never reaches its tail. `audio::heal_capture_endpoint_at_startup()` reads the
file at Master startup and restores **only if the default really is the cable**,
so an operator who chose `CABLE Output` themselves after a crash keeps their
choice. Same reasoning as `nova_display_baseline.txt`; deleting it is safe.

### Still true, and still not Nova's to fix

If the host microphone is silent in other applications *while no Echo session is
running*, that remains the VB-CABLE default-capture behaviour described in
CLAUDE.md, and `nova-server.exe --mic-probe` is the tool. What is fixed here is
the opposite direction: the host now hears the client without anyone selecting
the cable by hand.
