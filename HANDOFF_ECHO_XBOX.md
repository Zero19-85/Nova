# HANDOFF — Echo for Xbox (UWP)

**Status (2026-09-07): LIVE ON HARDWARE.** The console discovers the host over
mDNS, pairs with a PIN, hands itself off into a stream, and decodes 4K60 HEVC in
hardware onto a headless virtual display, with mouse, keyboard and controller
reaching the PC. Clean picture, low latency, no flashing, no frame backlog.

What is settled, in one place:

| | |
|---|---|
| Stream | 3840x2160 **@60** HEVC, app 5 (headless Virtual Desktop) |
| Frame rate | from a **pixel-rate budget**, never from the panel refresh — §4b.2 |
| Resolution | the console's **HDMI output** size, never the swap chain's — §4b.1 |
| Present | `SyncInterval 0`, `FLIP_DISCARD`, frame-latency 1, never queue — §4b.3 |
| Pointer | `CreateCoreIndependentInputSource` on a high-priority pool thread — §6.1 |
| Pad | 250 Hz on a high-resolution timer — §6.2 |
| Mouse mode | **client-side**, Menu+View, chord swallowed — §6.3 |
| Overlay | hold VIEW 700 ms; nothing on screen otherwise — §6.4 |
| Live re-mode | `echo_set_display` → the Worker's `apply_hot_display_mode` — §4c |

**Not done:** audio in either direction (§5), HDR10, and visual parity with the
Android "Ion" dashboard — the behaviour matches, the styling does not.

**Before touching the video or input path, read §4b.** Four separate blank
screens paid for the rules in it, and three of them looked like something other
than what they were.

### Milestone 1, kept because it is the linkage evidence

```
0 abi          ok
1 heap/crt     ok
2 filesystem   ok      LocalState is writable
3 udp socket   ok      UDP binds inside the container
4 identity     ok      d0493a41…cb2789   ← this console's Echo identity
```

**Stage 4 is the one that mattered**, and it passed: `ring` + `getrandom` reach
entropy through `ProcessPrng`/`RtlGenRandom` inside the app container, and
rcgen wrote an RSA-2048 key pair into `LocalState`. That was the only remaining
failure mode that would have forced a design change rather than a fix.

That fingerprint is **real, persistent client identity** — the same one pairing
puts into the host's `nova_paired.json`. It is generated once per install and
survives app restarts and upgrades.

This is the authority for the Xbox client, the way `HANDOFF_ECHO_ANDROID.md` is
the authority for the Android one. `CLAUDE.md` covers the **host**; almost
nothing in it applies here.

---
## 1. The shape of it

Echo on Xbox is the third shell around one core. It is not a new client.

```
        ┌──────────────────────── shared, already written ────────────────────┐
        │  nova-core     protocol + identity: STUN, RUDP, AES-GCM, relay      │
        │  echo-client   session, control, FEC, decrypt, gate, handover,      │
        │                frames (the queue), audio playout, input builders    │
        └────────────────────────────────────────────────────────────────────┘
             ▲                        ▲                          ▲
    echo-client/main.rs        echo-android              echo-xbox   ← NEW
       (headless CLI)         (JNI → Kotlin)          (C ABI → C++/WinRT)
                                     │                          │
                              MediaCodec, AudioTrack     Media Foundation,
                              AudioRecord, NsdManager    XAudio2, AudioGraph,
                                                         Windows.Gaming.Input
```

**Everything protocol-shaped is already done.** The Xbox port writes no
networking, no FEC, no crypto, no reassembly, no jitter buffering, and no
reconnect logic. It writes a decoder, a renderer, two audio paths, an input
reader, and a UI.

That is not an aspiration — `echo-xbox/src/lib.rs` is 1,200 lines of which zero
are protocol. It is the same file as `echo-android/src/lib.rs` with JNI swapped
for a flat C ABI, which is exactly how much a new platform should cost.

### What the port gets for free by being an Echo client

Worth stating plainly, because `xbox-port-open-questions` in memory raised it as
an open question and the answer is now known:

| Feature | Moonlight on Xbox | Echo on Xbox |
|---|---|---|
| Loss repair | a full 4K keyframe, every time | **RFI → LTR → IDR ladder** |
| Reconnect across a network change | reconnect from scratch | handover supervisor, decoder untouched |
| Media encryption | none | whole-frame AES-GCM |
| Audio frame size | 5 ms | 20 ms (fewer packets, less overhead) |
| Client feedback to the host | none this client sends | `feedback_channel` acks, tag `0xE6` |

The Tier 1 LTR work (`7b8d25a`) delivers on Echo and **not** on Moonlight,
because Moonlight-on-Xbox sends zero `PT_INVALIDATE_REF_FRAMES` against 127 IDR
requests. The acknowledgement watermark that makes LTR safe is emitted from
`echo-client/src/receiver.rs:663` — inside the shared core. So this port
inherits keyframeless recovery by existing, and **Tier 1 finally gets exercised
here.** That is the single strongest argument for Echo-on-Xbox over pointing
Moonlight at Nova.

---

## 2. Decision: C++/WinRT, not C#

Four reasons, in the order they mattered.

1. **The frame path.** `echo_fill_buffer` writes a frame directly into the
   pointer from `IMFMediaBuffer::Lock`. That is one copy, into memory the
   decoder already owns. From C# the same path is a pinned array plus marshalling
   per frame, at 120 fps, forever.
2. **`ISwapChainPanelNative` is a COM interface.** Binding a DXGI swap chain to a
   `SwapChainPanel` from C# needs a hand-written interop shim anyway — so the
   C# route ends up with a C++ component in it regardless.
3. **.NET Native.** UWP C# on a console compiles through the .NET Native
   toolchain, with its own build times and its own quirks around P/Invoke and
   generics. C++/WinRT has none of that.
4. **Precedent.** `moonlight-xbox-dx` is C++/WinRT with a hardware decoder and a
   swap chain on Xbox. It is the proof that this combination works on the target.

The bridge is a flat C ABI rather than a WinRT component, so if the UI half ever
does move to C#, it moves with a `DllImport` and no rewrite.

---

## 3. Decision: how the Rust links — and the measurement behind it

This was the riskiest unknown and it is now settled empirically rather than by
argument.

**`echo-xbox` builds as a `cdylib` with a statically linked CRT**
(`-C target-feature=+crt-static`), ships inside the `.appx`, and is bound at
load time through its import library.

### The two-CRT rule

A UWP app links the Store CRT (`vcruntime140_app.dll`). A Rust `cdylib` with the
default dynamic CRT links `vcruntime140.dll` — the non-app twin. Both in one
process means two heaps, and a pointer allocated by one and freed by the other is
a corruption bug that reproduces on a console and nowhere else.

Static linking removes the question entirely, and the ABI is designed so it can
never come back:

- **Text out** is `(char* out, int32_t cap)`, returning bytes written or a
  negative code. Never a pointer for C++ to free.
- **Frames out** are written into the caller's locked buffer.
- **Data in** is `(const uint8_t*, int32_t)`, copied before the call returns.
- **There is deliberately no `echo_free`.** If a new entry point seems to need
  one, it is the wrong shape.

### What the DLL actually imports (measured 2026-09-06)

```
api-ms-win-core-synch-l1-2-0.dll   WaitOnAddress, WakeByAddress{All,Single}
ws2_32.dll                         WSASocketW, bind, sendto, recvfrom, WSAIoctl, …
kernel32.dll                       the usual
ntdll.dll                          NtCreateFile, NtReadFile, NtWriteFile,
                                   NtDeviceIoControlFile, NtCancelIoFileEx,
                                   RtlNtStatusToDosError
bcryptprimitives.dll               ProcessPrng
ADVAPI32.dll                       SystemFunction036  (= RtlGenRandom)
```

Read that list carefully, because it is the whole app-container risk assessment:

- **No `vcruntime140.dll`, no `msvcp140.dll`.** `crt-static` did its job. Re-run
  `build-bridge.ps1 -Imports` after any dependency bump; a vcruntime appearing
  there is the one result that invalidates this design.
- **`ntdll` is Rust's `std::fs`**, not anything exotic. These resolve inside an
  app container — `ntdll` is always mapped. They bypass the Win32 layer but not
  the container's ACLs, so a write outside `LocalState` still fails with
  access-denied, which is the behaviour we want anyway.
- **`ProcessPrng` and `SystemFunction036` are `getrandom`'s two Windows
  backends.** `ProcessPrng` is the modern, app-container-friendly one and is
  preferred at runtime; `RtlGenRandom` is the fallback. Two paths to entropy,
  which is why probe stage 4 is expected to pass.
- **No `dbghelp`**, so nothing tries to symbolicate a panic backtrace. Panics are
  caught at every boundary and reported as strings.

### Alternatives considered and rejected

| Option | Why not |
|---|---|
| `x86_64-uwp-windows-msvc` target | Tier 3. Needs nightly and `-Z build-std`; this workspace is on stable 1.96. Solves a problem the import table says we do not have. |
| Rust `staticlib` linked into the exe | Forces the whole app to `/MT`, which is not the supported configuration for a UWP app. The DLL keeps the CRT boundary at a place we control. |
| Dynamic CRT + ship `vcruntime140.dll` | Works, but puts two CRTs in the process on purpose. Fine until the day somebody adds an entry point that returns a pointer. |

### Provider: `ring`, not `aws-lc-rs`

`ring` builds its C through the `cc` crate, which honours `crt-static` and emits
`/MT` objects to match. `aws-lc-rs` builds through cmake, which does not. Same
conclusion the Android build reached, arrived at from the opposite direction.

`echo-xbox` is therefore a workspace **member but not a default member** —
identical to `echo-android`, and for the identical reason: a bare
`cargo build --release` that included it would unify `nova-core/ring` into the
host build and link two crypto providers into `nova-server.exe`.

---

## 4. Video: Media Foundation → D3D11 → SwapChainPanel

The path, end to end:

```
echo_fill_buffer  →  IMFSample (Annex-B HEVC)  →  hardware HEVC decoder MFT
                                                          ↓
                       ID3D11Texture2D (NV12) via IMFDXGIBuffer
                                                          ↓
                       NV12 → RGB pixel shader, drawn to the back buffer
                                                          ↓
                       IDXGISwapChain1::Present1  →  SwapChainPanel
```

### Getting the decoder

`MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_HARDWARE |
MFT_ENUM_FLAG_SORTANDFILTER, {MFMediaType_Video, MFVideoFormat_HEVC}, nullptr)`,
then take the first result. Six things decide whether this is a 10 ms decoder or
a 60 ms one:

1. **`MF_LOW_LATENCY = TRUE`** on the MFT's attributes, and
   `CODECAPI_AVLowLatencyMode` through `ICodecAPI`. Without it the decoder holds
   frames to reorder them — for a stream with no B-frames, that is pure added
   latency. This is the single biggest lever on the whole client.
2. **The D3D11 device needs `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`**, and
   `ID3D10Multithread::SetMultithreadProtected(TRUE)` is **mandatory** — the MFT
   decodes on its own threads and shares our device context. Skipping it produces
   corruption that looks like a network fault.
3. **`MFT_MESSAGE_SET_D3D_MANAGER`** with an `IMFDXGIDeviceManager` wrapping that
   device, sent *before* setting media types. Out of order it silently falls back
   to software decoding, and the symptom is high CPU and a stream that keeps up
   at 1080p but not 4K.
4. **`MFVideoFormat_HEVC`, not `MFVideoFormat_HEVC_ES`.** Nova emits Annex-B with
   start codes — the host log proves it (`["VPS","SPS","PPS","IDR_W_RADL"]`), and
   `MFVideoFormat_HEVC` is the byte-stream subtype. The `_ES` variant expects
   length-prefixed NALs and will decode nothing.
5. **Mark keyframes.** `meta[1]` bit 0 from `echo_fill_buffer` →
   `MFSampleExtension_CleanPoint`. After any gap, also set
   `MFSampleExtension_Discontinuity` so the decoder does not try to conceal
   against references it never had.
6. **Timestamps come from the wire frame index**, already scaled to 100 ns units
   in `meta[2]`. They only have to increase. Do not substitute a receive clock —
   that encodes our own jitter into the stream.

### The swap chain

`CreateSwapChainForComposition` (not `ForHwnd` — there is no HWND in UWP), then
`ISwapChainPanelNative::SetSwapChain`. Settings that matter:

- `DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL`, `BufferCount = 2`.
- `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT` +
  `SetMaximumFrameLatency(1)`, and wait on the object before rendering. This is
  the difference between one and three frames of queued latency.
- Present on the frame's own arrival, not on a timer. The stream is the clock.

**`SwapChainPanel` costs one DWM composition hop** versus
`CreateSwapChainForCoreWindow`. It is the right starting point because it
composes with XAML — the stats overlay, the connecting spinner, and the pairing
PIN all want to be XAML, not hand-drawn text. If measurement later says that hop
matters, the escape hatch is a CoreWindow swap chain with the overlay drawn into
it; that is a milestone-5 question, not a milestone-3 one.

### 4K, and why an Xbox gives an app 1080p by default

Observed on hardware 2026-09-06: milestone 2a rendered at 1080p and dropped the
TV's output mode to 1080p with it. That is the console behaving as designed — it
treats a UWP app as a media player, not a title — and it has two separate halves
that must both be fixed:

1. **The console's HDMI output mode.** `HdmiDisplayInformation::
   RequestSetCurrentDisplayModeAsync` is the documented way for an app to take
   that decision back; it is what 4K media apps use. It must run **before** the
   swap chain is created, because it is what makes the composition scale report
   2.0 in the first place. `RequestBestHdmiMode` picks the highest refresh at
   3840x2160 — refresh, not bit depth, because this is a latency path.
2. **The swap chain's size.** An Xbox XAML view is 1920x1080 *logical* whatever
   the TV does. The swap chain must be `logical x compositionScale` with an
   inverse `SetMatrixTransform` to compensate. Size it to the logical size and
   you render 1080p and let the console upscale — a soft picture with no error
   anywhere to explain it.

### ALLM — a negative result, verified

**There is no manifest attribute and no WinRT API that turns on Auto Low Latency
Mode.** Checked against the SDK on this box (10.0.26100.0) rather than assumed:

- `grep -ril "windows.games"` across every `.xsd` in the SDK: **no hits.** The
  suggested `uap:Category="windows.games"` does not exist. The only `game` in
  any manifest schema is `ST_KindValue` in `UapManifestSchema_v4.xsd`, which is
  a *shell file-type Kind* for search and indexing — unrelated.
- `grep -ril "AutoLowLatency|LowLatencyMode|IsGameMode"` across the WinRT
  headers: **no hits.**

ALLM is an HDMI InfoFrame bit the *console* sets, and retail Xbox sets it for
titles the Store classifies as games. A Dev-Mode sideloaded UWP app is an app.
So the levers that actually exist are:

- **Console side:** Settings → General → TV & display options → Video modes →
  "Allow auto low-latency mode" must be on.
- **TV side:** force Game Mode on that HDMI input. For a dev-mode app this is
  the reliable path, and it is what to do today.
- **What we control, which matters more anyway:** `SetMaximumFrameLatency(1)`
  plus the waitable object (one frame in flight, not three), `MF_LOW_LATENCY` on
  the decoder, and matching the highest refresh the TV offers. Those are real
  milliseconds; ALLM mostly removes the TV's own post-processing delay, which
  Game Mode also removes.

Do not spend another session hunting for a manifest flag — the schemas above are
the authority and they do not have one.

### Two more Xbox-specific things nobody expects

- **The TV-safe area.** XAML on a console insets everything by ~48 px so an old
  CRT would not overscan it. For a video stream that is a black frame the user
  cannot explain, plus a scale-down and a re-scale by the TV. The opt-out is
  `ApplicationView::SetDesiredBoundsMode(ApplicationViewBoundsMode::UseCoreWindow)`
  and it is already in `App.cpp`.
- **`HdmiDisplayInformation`.** A console can be told to switch the HDMI output
  mode to match the stream — resolution, refresh, and HDR — via
  `RequestSetCurrentDisplayModeAsync`. This is the Xbox's version of the host's
  display-mode matching and it removes a scaler from the path. It is also how
  the client learns what the TV is really being fed, which is what the stream
  size is taken from - see 4b.1. No other Echo platform has this lever.
  **It does not follow that the console can DECODE what it can display** -
  see 4b.2, which cost ten sessions to learn.

---

## 4b. The settled hardware limits, and the two mistakes that found them

Everything in this section was paid for in blank screens. Read it before
changing what the client asks the host for, or how the renderer presents.

### 4b.1 Ask the host for the SCREEN's size, never the swap chain's

An Xbox composes a XAML app's surface at **1920x1080 logical whatever the TV is
doing**. Sizing the stream request from the back buffer therefore asked a 4K
console for a 1080p desktop and upscaled it, which is why the first working build
was stuck at 1080p with a 4K TV and a host perfectly willing to serve 4K.

The stream size now comes from `HdmiDisplayInformation::GetCurrentDisplayMode()`,
**read back after the mode request rather than inferred from it** — asking for 4K
and being refused leaves the TV at 1080p, and a client that believed its own
request would demand four times the pixels the panel can show. That is what
`HdmiOutcome` exists for. The renderer scales whatever arrives into whatever
surface XAML gives it, which it was already doing for free.

### 4b.2 The panel's refresh rate is NOT the decodable frame rate

Conflating those two cost **ten consecutive sessions of blank screen**, and the
host was flawless through every one of them: VDD at 3840x2160@120 primary
headless, WGC at 4K, `NVENC READY (hevc @ 3840x2160, 39488 Kbps, 120 fps)`, IDR
on the wire, RTT 5–17 ms, input flowing. A 120 Hz TV had made the client ask for
4K120, and **the Xbox HEVC decoder produces nothing at all from it.**

So the frame rate comes from a **pixel-rate budget**
(`kMaxDecodePixelRate = 520M px/s` in `MainPage.cpp`), and the display refresh
only ever caps it:

| mode | pixel rate | |
|---|---|---|
| 4K120 | 995M px/s | over budget → refused, falls to 60 |
| 4K60 | 498M px/s | fine |
| 1440p120 | 442M px/s | fine |
| 1080p120 | 249M px/s | fine |

A budget rather than a table of known-good modes, because the overlay lets a user
pick any resolution and every combination has to land somewhere sensible without
being enumerated. `DecodableFps` is applied at startup **and** in the overlay, so
choosing 4K and then 120 by hand cannot reproduce it.

**The diagnostic signature is the part to remember.** A client that cannot decode
what it is sent asks for **one keyframe at session start and then goes silent** —
`echo_fill_buffer` keeps delivering, `Submit` keeps succeeding, and nothing
upstream notices. Contrast the Android MediaCodec wedge, where a client that
stops *consuming* floods the host with keyframe requests. One request then
silence means the decoder is eating frames and producing nothing; a flood means
the queue is overflowing. They look identical from the sofa and opposite in the
log.

### 4b.3 Never queue decoded frames

The renderer originally took exactly one decoded frame per iteration and paid a
vblank for it (`Present1(1, …)`). That drains the MFT at the display's refresh
rate and no faster, so any burst — a hiccup, a slow present, a stream above the
panel's refresh — became **permanent** latency: the backlog could never shrink.
The symptom was video that lagged input and then visibly raced to catch up.

`HevcDecoder::TryGetFrame` now returns the **newest** decoded picture and
discards everything behind it. The drop policy lives at the queue, not in the
renderer.

**It loops `ProcessOutput` until `MF_E_TRANSFORM_NEED_MORE_INPUT`, and must not
be gated on `GetOutputStatus`.** A hardware MFT is not required to implement that
call and the Xbox HEVC decoder does not — it answers `E_NOTIMPL`, which reads as
"nothing ready" and is indistinguishable from an empty decoder. **The only
reliable way to learn whether a frame is waiting is to ask for it.**

`Submit`'s `MF_E_NOTACCEPTING` arm was also backwards: it dropped the *newest*
frame to preserve a queue of older ones, so the backlog defended itself. It now
discards what is queued and takes the new frame.

Presentation is `SyncInterval = 0` with `DXGI_SWAP_EFFECT_FLIP_DISCARD` — flip
*sequential* presents every buffer in order, so interval 0 alone would still have
enqueued one. `DXGI_PRESENT_ALLOW_TEARING` is queried via
`CheckFeatureSupport(DXGI_FEATURE_PRESENT_ALLOW_TEARING)` and used only if
offered; on a composited `SwapChainPanel` the compositor owns the scanout, so do
not expect it. The frame-latency waitable at `SetMaximumFrameLatency(1)` is the
only pacer.

### 4b.4 A failed present must never be fatal

`if (FAILED(Present1(...))) break;` made one refused present a **permanently**
blank screen: the render thread exited, nothing restarted it, and the XAML layer
above kept drawing perfectly — so the app looked completely alive and the video
simply never appeared.

`SyncInterval` is now a variable that degrades 0 → 1 on `DXGI_ERROR_INVALID_CALL`
(a composition swap chain may refuse "show this immediately"), only
`DEVICE_REMOVED`/`DEVICE_RESET` ends the thread, and the last present HRESULT is
readable from the UI.

### 4b.5 Do not hide the only diagnostic behind a gesture

`fed / decoded / drawn / dropped` were being counted correctly through every one
of those blank sessions, and they name the stuck stage exactly — but they lived
behind an overlay button nobody had a reason to press, so the app showed a plain
background and said nothing.

`NoVideoPanel` now appears whenever a session is live and nothing has ever been
drawn, and says which stage is stuck **in words**. It disappears on the first
drawn frame. If a future change adds a stage, add it to that panel.

---

## 4c. Live resolution changes

The client can re-mode the host display mid-session — no restart, desktop and
windows untouched. `echo_set_display` → `Uplink::control` → the host's
`set_display` RPC → `ControlMsg::SetDisplayMode` → the Worker's
`apply_hot_display_mode`.

**The client sends `force`, and that is a claim about its own decoder.**
`handle_set_display` refuses a mid-session change by default because a Moonlight
client builds its decoder once at ANNOUNCE and a geometry change under it yields
a black frame with a green region (live 2026-08-10). Media Foundation is not that
decoder: an HEVC MFT answers a resolution change with
`MF_E_TRANSFORM_STREAM_CHANGE` and renegotiates its output type. **If the C++
side ever stops handling that message, this call must stop sending `force`.**

One trap found while building it: `TryGetFrame` reused `m_outputSample` across a
stream change, and its buffer was allocated for the *old* `cbSize`. Growing
1080p → 4K on the software path would have handed the decoder a buffer a quarter
of the size it needed, forever. Hardware decoders provide their own samples and
never take that branch — which is exactly why it would have gone unnoticed until
somebody streamed to a machine without one.

Host side, `VirtualDisplay::reconfigure_active` re-modes in place (CCD
`force_resolution` + read-back) rather than running the full
`activate_for_stream`. **MttVDD reads `vdd_settings.xml` when the devnode starts
and never again**, so a mode that is new to this devnode generation will not
commit now; `configure_mode` still runs so the *next* activation can reach it,
and the read-back turns "not advertised" into an honest error instead of a
success that changed nothing.

## 5. Audio: the fact that shapes both directions

**Windows ships an Opus *decoder* but no Opus *encoder*.** Android hands
`MediaCodec` the work in both directions; Xbox cannot.

So the Opus codec moves into the bridge, and the ABI changes shape to match:

| | Android | Xbox |
|---|---|---|
| Downstream | `echo_poll_audio` → Opus packet → MediaCodec → AudioTrack | Opus packet → **decode in Rust** → PCM → XAudio2 |
| Upstream | AudioRecord → MediaCodec → `echo_send_mic(packet)` | AudioGraph → PCM → **encode in Rust** → `echo_send_mic` |

The entry points as written today pass **encoded Opus packets**, identical to
Android — `echo_poll_audio` returns one, `echo_send_mic` takes one. That is
deliberate: it costs nothing now and keeps the two bridges the same shape. When
milestone 4 lands, add `echo_poll_audio_pcm` / `echo_send_mic_pcm` alongside them
behind an `opus` feature on the crate; do not change the existing two.

- **Downstream renderer: XAudio2.9** (`xaudio2_9.lib`, in the OS on Xbox). A
  source voice with a submitted-buffer queue matches the pull model exactly:
  the voice asks for the next 20 ms, `echo_poll_audio` answers, and the four
  step codes map onto submit / conceal / submit-silence / idle.
- **Upstream capture: `AudioGraph`** with `CreateDeviceInputNode` and an
  `AudioFrameOutputNode`, `QuantumSizeSelectionMode = LowestLatency`. Simpler
  than WASAPI on UWP and it hands over float32 frames on a fixed quantum.
  Convert to i16, accumulate to 20 ms, encode, send.
- **20 ms, not 5.** Echo sessions negotiate 20 ms audio frames host-side
  (`CLAUDE.md`, top). Match it or the jitter buffer's arithmetic is wrong.
- **`AUDIO_CONCEAL` is not `AUDIO_SILENCE`.** A concealed frame is the decoder's
  guess at audio that existed; silence is the absence of any. Rendering both as
  quiet is fine — collapsing the *codes* is not, because it destroys the only
  signal that separates packet loss from an empty buffer.
- **The headset mic on a console is a live unknown.** The `microphone`
  capability is declared, but whether the chat headset is available to a UWP app
  while party chat holds it is untested. If it is contested, try
  `MediaCategory::Communications`. Do not assume; measure.

---

## 6. Input — LIVE (2026-09-07)

Two threads, neither of them the UI thread, and one hard-won manifest line.

### 6.0 The console's own cursor has to be switched off first

`App::App()` sets
`RequiresPointerMode(ApplicationRequiresPointerMode::WhenRequested)`.

An Xbox puts every XAML app into "mouse mode" by default: it draws a system
cursor, steers it with the **left stick**, and converts the pad into pointer
input before the app sees any of it. For a launcher that is a kindness. For a
client that forwards a controller to a PC it fails three ways at once — the left
stick never reaches the game, there are two cursors on screen (the console's and
the host's), and the console's pointer emulation adds latency to input that
already has a network trip ahead of it.

`WhenRequested` means "give me raw gamepad input; synthesise a pointer only for a
page that asks". No page here asks. **A real USB mouse is unaffected** — this
governs the emulated pointer, not physical pointer devices.

It must be set in the constructor. `OnLaunched` is already too late.

### 6.1 Pointer: `CreateCoreIndependentInputSource`, on a high-priority thread

XAML pointer events queue behind layout, animation and every `RunAsync` the app
posts. On a page presenting 60 frames a second that is a variable few
milliseconds on every mouse move — which is what a cursor feels like when its
latency is not constant.

`SwapChainPanel::CreateCoreIndependentInputSource` hands raw pointer input to a
thread of the app's choosing. **It must be created on the thread that will pump
it**, so the creation, the handlers and `Dispatcher().ProcessEvents()` all live
inside one `ThreadPool::RunAsync` work item at `WorkItemPriority::High`. Wiring
it up from `Start()` would bind it right back to the UI thread it exists to
escape.

Positions are sent **absolute** (`echo_send_input` kind 2), not relative: this is
a desktop on a television, so where the pointer sits on the panel is where it
belongs on the remote screen. Relative deltas accumulate rounding and drift away
from the cursor the user can see. The panel's logical size is pushed in from the
UI thread (`SetSurfaceSize`) because the input thread cannot read `ActualWidth`.

A release cannot read its own button off `PointerPointProperties` — by the time
it fires they all report "not pressed" — so the bridge keeps the set it believes
is down and derives the release from the difference.

The keyboard stays on the UI thread deliberately. It is the one input whose
latency nobody can feel (a keystroke has already waited on a human), and
`CoreWindow` is the only place UWP reports key state.

### 6.2 Pad: a 250 Hz poll on a high-resolution timer

`Windows.Gaming.Input` is polled, not pushed — there is no event to wait on. The
poll runs at 4 ms on a `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` timer: **the
default Windows tick is ~15.6 ms**, so a plain sleep would sample the controller
at 64 Hz while claiming 250 and nothing would report the difference.

Snapshots go out on change, plus a 50 ms re-send while the pad is not at rest.
Input rides an unreliable datagram channel and each packet is a whole state, so
that bound is what stops a lost "stick returned to centre" leaving the host
holding a direction forever.

`xbox/EchoXbox/GamepadInput.h` has the translation table. Two facts worth
repeating:

- **`GamepadButtons` is not XInput's bit layout.** GameStream's low 16
  `buttonFlags` *are* XInput's, bit for bit, which is why the host hands them to
  ViGEm with no table at all. Both are 16-bit flag enums, so a wrong entry
  compiles and produces a controller that works almost right.
- **Menu is START, View is BACK.** This is where a transposed pair comes from.

Sticks are `-1.0..1.0` with **Y positive up**, which matches XInput, so unlike
Android there is no negation. A sign flip reads as inverted look — a preference
someone forgot to expose — rather than as a defect.

### 6.3 Controller mouse mode is CLIENT-side here, and the chord is swallowed

**This reverses what an earlier draft of this document said.** Nova implements
mouse mode host-side in `gamepad_mouse.rs`, and it *is* reachable from Echo — an
Echo pad datagram reaches `input::handle_input_packet`, whose gamepad arm gives
`gamepad_mouse::intercept` first refusal on every frame. It had simply never
fired, because until 2026-09-07 this client sent zero gamepad packets
(`input peak 0/s x0 samples` in every host stats line).

So the chord is **swallowed** in `InputBridge::PadLoop` — Menu and View are
stripped from the snapshot until both are released — and that is load-bearing.
If the chord reached the host, **both implementations would toggle on the same
press**: two cursor drivers integrating the same stick at roughly double speed,
with the host also swallowing the pad so nothing could turn it off again.
Exactly one of them may see the chord, and on this client it is the local one.

Client-side buys something real: the host's driver integrates a stick position it
learns from arriving packets, while this loop integrates one it sampled 4 ms ago,
and the resulting cursor motion never crosses the network — only the pixels it
moved do.

Behaviour is deliberately identical to `gamepad_mouse.rs`, because a user will
meet both and a stick that moves at a different speed depending on which client
is in their hands is a bug nobody can describe:

| | |
|---|---|
| Cursor | right stick |
| Left click | right trigger |
| Right click | left trigger |
| Deadzone | 0.13 |
| Speed at full deflection | 1600 px/s |
| Response curve | squared |
| Trigger threshold | 96/255 |

Two rules carried over from the host implementation, both real hazards:

1. **The pad is neutralised on both toggle edges** — one zeroed snapshot sent,
   not merely "stop sending". Input that stops leaves the host holding what it
   last saw, and what it last saw has Menu held. Games read Menu as pause.
2. **Sub-pixel remainder is carried between ticks.** At 250 Hz, `SendInput`
   moving whole pixels means anything under 250 px/s rounds to zero every tick —
   removing precisely the slow, careful movement a stick is worst at.

### 6.4 The overlay gesture: hold VIEW

While streaming there is no on-screen UI at all. Holding **View** for 700 ms
opens the overlay (resolution, frame rate, disconnect, diagnostics); a short
press still reaches the PC.

View is **withheld** while it might still become a hold — the same delay-rather-
than-retract trap as the Android client's 60 ms touch press-trap. The first
moment of a gesture is indistinguishable from the start of an ordinary press, and
retraction cannot work: the host has already pressed the button.

The gesture is skipped entirely while the chord is in play. Menu+View is a chord,
not a View press that happens to overlap one.

Opening the overlay **parks forwarding** (`SetForwarding(false)`), which sends a
`release-all` first — otherwise the button press that dismisses the panel also
travels to the PC.

---
## 7. Milestones — all green, 2026-09-07

Each had an exit criterion that is a thing you can see, not a thing you believe.

| # | Milestone | Status |
|---|---|---|
| **1** | Hello World + linkage probe | ✅ hardware, 2026-09-06 — five green probe lines, RSA identity generated inside the app container |
| **2** | A picture on the TV | ✅ hardware, 2026-09-06 — native mDNS discovery, PIN pairing, automatic handoff, live HEVC hardware decode |
| **3** | Input | ✅ hardware, 2026-09-07 — mouse, keyboard, controller, and client-side mouse mode |
| **4** | Resolution & pacing | ✅ hardware, 2026-09-07 — 4K60 headless virtual display, live re-mode, no frame queue, no flashing |
| 5 | Audio both ways | **not started.** Game audio out of the TV; the headset mic into the host's VB-CABLE. Windows ships an Opus *decoder* but no encoder — see §5. |
| 6 | HDR10 | **not started.** The pipeline is SDR BGRA8 end to end today. |
| 7 | UI parity with Android "Ion" | **behaviour matched, styling not.** Next session. |

**Milestone 1 was the whole point of the probe**, and it paid: every unknown in
this document that could have stopped the project was answered by five lines in
one afternoon, rather than surfacing at video time as a black screen with four
plausible causes.

**Milestone 2's staging was abandoned and that was the right call.** The plan
below split it 2a/2b/2c with a canned clip in the middle, to front-load the
decoder risk. The operator skipped straight to the live pipeline and it worked
first time. The reasoning was still sound — it just turned out the decoder was
not where the risk lived. Every expensive failure in this project since has been
in *pacing and negotiation*, not in decoding: what the client asks for (4b.2),
how frames are drained (4b.3), and what a failed present does (4b.4). Keep the
instinct, aim it one layer up.

| | Step | Answered |
|---|---|---|
| 2a | D3D11 device + swap chain on the `SwapChainPanel` | Yes — a composition swap chain fills the TV; the XAML surface is 1080p logical whatever the panel is (4b.1) |
| 2b | Media Foundation HEVC decode of a canned clip | Skipped |
| 2c | Pair, connect, feed `echo_fill_buffer` into 2b | Yes — worked on the first live attempt |

---


## 8. Files

```
echo-xbox/                          NEW — the bridge crate
  Cargo.toml                        cdylib + rlib, ring, not a default member
  src/lib.rs                        the flat C ABI; no protocol logic
echo-client/src/frames.rs           MOVED here from echo-android (see below)
xbox/
  EchoXbox.sln
  build-bridge.ps1                  builds with +crt-static, stages the DLL
  build-app.ps1                     MSBuild + signing + package verification
  make-assets.ps1                   generates the five placeholder tiles
  packages/                         restored C++/WinRT NuGet (gitignored)
  EchoXbox/
    EchoXbox.vcxproj                x64 only, v143, AppContainerApplication
    Package.appxmanifest            capabilities — read the comments
    EchoBridge.h                    the ABI, as C sees it
    GamepadInput.h                  WinRT buttons → XInput bits
    App.xaml / App.h / App.cpp      full-bleed opt-in, RequiresPointerMode
    MainPage.xaml / .h / .cpp       dashboard, overlay, no-video panel
    VideoRenderer.h / .cpp          D3D11 device, composition swap chain,
                                    HDMI mode, present pacing
    HevcDecoder.h / .cpp            the MFT, and the never-queue drain
    HostDiscovery.h / .cpp          DNS-SD; the advertised fp is a LABEL
    EchoSession.h / .cpp            handle, event pump, feeder, hosts.json
    InputBridge.h / .cpp            core-independent pointer, 250 Hz pad,
                                    mouse mode, the VIEW-hold gesture
    MainPage.idl                    the only IDL — see the first-build fixes
    pch.h / pch.cpp
    packages.config                 C++/WinRT NuGet
    Assets/                         the Android icon, redrawn (make-assets.ps1)
    bridge/                         staged DLL + import lib (gitignored)
    AppPackages/                    the .msix output (gitignored)
  dist/                             the sideload zip (gitignored)
```

There is deliberately **no `App.idl`** — see
[the three first-build fixes](#the-three-first-build-fixes) before adding one
back.

### The `frames.rs` move

`echo-android/src/frames.rs` → `echo-client/src/frames.rs`, re-exported from
`echo-android` so `frames::FrameQueue` still resolves there.

The bounded queue's drop policy — small, drop-oldest, re-arm the keyframe gate,
ask for an IDR — is a property of **live streaming**, not of Android. The Xbox
feeder needs precisely the same behaviour, and two copies of a policy that subtle
would have diverged. The workspace's own manifest comment already made this
argument about `echo-client` itself: "the only way to keep them identical is for
there to be one copy."

`cargo check -p echo-client -p echo-android` is clean after the move.

---

## 9. Building it

```powershell
cd xbox
.\build-bridge.ps1 -Imports   # 1. Rust half. -Imports re-verifies the CRT situation
.\make-assets.ps1             # 2. placeholder tiles, once
.\build-app.ps1               # 3. UWP app → signed .msix
```

`build-app.ps1` finds MSBuild through `vswhere`, creates the self-signed
sideload certificate on first run, builds signed Release, and then **verifies
the package** — it fails loudly if `echo_xbox.dll` is not at the package root,
because that particular mistake is invisible until the app refuses to launch on
the console.

### What the toolchain has to have

The decisive artifact is the per-toolset UWP directory:

```
MSBuild\Microsoft\VC\<v170|v180>\Application Type\Windows Store\10.0\
    Platforms\x64\PlatformToolsets\<v143|v145>
```

If that is absent, the "Universal Windows Platform development" workload's C++
tools are not installed, and the failure is an unhelpful message from
`Microsoft.Cpp.Default.props` about the platform toolset. Note that **VS 2026
(v18) builds this project fine**: it resolves the project's `v143` toolset out
of the `v170` tree, so the pin does not need changing. Verified on this box —
VS 2026 Community, Windows SDK 10.0.26100.0, MSVC 14.44.

### C++/WinRT restore without an IDE

`packages.config` needs `nuget.exe`, which is not installed here and which
`msbuild -t:restore` does not cover (that is PackageReference only). A `.nupkg`
is just a zip, so the restore is a download and an extract into
`xbox\packages\` — which is what was done for `Microsoft.Windows.CppWinRT`
**2.0.250303.1** (the last of the 2.0 line; 3.0 is newer but 2.0 is what UWP
XAML is exercised against). Opening the solution in Visual Studio restores it
the normal way instead.

### Signing, and why `signtool verify` "fails"

The certificate subject must equal the manifest's `Publisher` **exactly**
(`CN=Nova`); a mismatch is a late packaging error that talks about the publisher
rather than the certificate. The cert is self-signed, so on this box:

```
SignTool Error: A certificate chain processed, but terminated in a root
    certificate which is not trusted by the trust provider.
```

**That is the expected result, not a failure.** The package *is* signed
(`AppxSignature.p7x` is present and the signer is `CN=Nova`); the root is simply
untrusted here. Installing the `.cer` on the console is what resolves it.

### Sideloading onto the console

Xbox in Developer Mode → Device Portal → **Home → Add**, then supply all three:

| Field | File |
|---|---|
| App package | `EchoXbox_0.1.0.0_x64.msix` |
| Certificate | `EchoXbox_0.1.0.0_x64.cer` — required, the cert is self-signed |
| Dependency | `Dependencies\x64\Microsoft.VCLibs.x64.14.00.appx` |

The VCLibs dependency is not optional and is easy to miss: the app links the
Store CRT dynamically (`VCRUNTIME140_APP.dll`, `MSVCP140_APP.dll`), so the
manifest carries `<PackageDependency Name="Microsoft.VCLibs.140.00">`. If the
console does not already have that framework package, the install fails with an
error about a missing dependency rather than about VCLibs by name. MSBuild
stages the right `.appx` beside the package automatically.

**Deploying to a console:** put the Xbox in Developer Mode, then in the project's
Debugging properties set Debugger to **Remote Machine**, Machine Name to the
console's Device Portal address, and Authentication to **Universal**. F5 deploys
and launches. The Device Portal's file explorer is also how you retrieve
`LocalState` — which is where the identity and any client-side log live, because
a UWP app cannot write beside its executable.

### When a probe stage fails

`MainPage.cpp` stops at the first failure and points here.

| Stage | Failure means | First thing to try |
|---|---|---|
| 0 | The DLL did not load or an export is missing | Confirm `bridge\echo_xbox.dll` is in the `.appx` (`DeploymentContent`), and that `build-bridge.ps1` ran after the last Rust change |
| 1 | The CRT is broken — should be impossible | Re-run with `-Imports`; a `vcruntime140.dll` there is the cause |
| 2 | Filesystem refused | The path must be `ApplicationData::Current().LocalFolder().Path()`. Anything else is denied by the container, correctly |
| 3 | Sockets refused | A missing capability in `Package.appxmanifest`. `internetClientServer` is the one people forget, and it is the one the hole punch needs |
| 4 | Entropy or crypto refused | The interesting one. Report the exact error — it decides whether `ring` needs a different entropy path on this OS |

---

## 10. What is verified, and what is not

Stated plainly, because half of this document is design and the other half is
measurement, and they must not be confused.

**Verified ON THE CONSOLE, 2026-09-07** — operator report, with the host log
agreeing:

- Discovery, PIN pairing, automatic handoff into a stream.
- Live HEVC hardware decode at **3840x2160** on a headless virtual
  display, with the physical monitor released.
- Mouse, keyboard and controller reaching the PC. Host log:
  `⌨️  Echo input/s: 52 datagrams, 19 applied … last inject 2µs`.
- No idle flashing, no frame backlog, stable presentation.
- Install-over-the-top keeps `hosts.json` and the console identity.

**Verified on this box, 2026-09-06:**

- `echo-xbox` compiles, and its 8 unit tests pass.
- The `cdylib` builds with `+crt-static` and links: 3.88 MB.
- Its import table contains no vcruntime — measured with `dumpbin`, listed in §3.
- All 17 `echo_*` exports are present and undecorated.
- `build-bridge.ps1` builds and stages both artifacts end to end.
- `make-assets.ps1` produced the five tiles.
- The `frames.rs` move leaves `echo-client` and `echo-android` compiling.
- **The C++/WinRT app compiles, links, and packages.** MIDLRT, the XAML
  compiler, the C++/WinRT projection, `mdmerge`, and the appx packager all run
  clean; `build-app.ps1` reproduces it from a clean output directory.
- **`EchoXbox.exe` binds the bridge for real** — `dumpbin /IMPORTS` shows
  `echo_xbox.dll` with `echo_probe` and `echo_last_error` (the two milestone 1
  uses; the linker imports only what is referenced).
- **The exe carries the `App Container` DLL characteristic**, so it is a genuine
  UWP binary rather than a desktop one that merely built.
- **The two-CRT prediction is confirmed in the shipped binaries**: the app
  imports `VCRUNTIME140_APP.dll` / `MSVCP140_APP.dll` (the Store CRT) while
  `echo_xbox.dll` imports no CRT at all. The design in §3 was necessary, not
  precautionary.
- **`echo_xbox.dll` is at the package root** and the package is signed —
  asserted by `build-app.ps1` on every build.

### The three first-build fixes

All three were in the hand-written `.vcxproj` / project files, and each is
recorded because the symptom pointed somewhere unhelpful.

1. **`App.idl` had to be deleted.** An empty IDL makes MIDLRT emit an empty
   `.winmd`, and `mdmerge` then fails with `MDM2025: The input directory or file
   … App.winmd is not valid`. The real C++/WinRT template has no `App.idl` at
   all — only classes XAML actually consumes need one, which here is `MainPage`.
2. **`_VSDESIGNER_DONT_LOAD_AS_DLL` had to be defined.** Without it the XAML
   compiler's generated `XamlTypeInfo.g.cpp` emits `VSDesignerCanUnloadNow` and
   `VSDesignerDllGetActivationFactory`, which call `WINRT_CanUnloadNow` /
   `WINRT_GetActivationFactory` — symbols current C++/WinRT does not define for
   an executable. Symptom: `LNK2019` on both, from `XamlTypeInfo.g.obj`. Those
   entry points exist only so the Visual Studio XAML *designer* can load the app
   as a DLL, so an Xbox app loses nothing.
3. **`<Link>echo_xbox.dll</Link>` on the bridge's `None` item.** Without it the
   DLL is packaged as `bridge/echo_xbox.dll`, and the loader does not search
   subfolders for an implicitly-linked DLL — so the package builds, signs,
   deploys, and then **fails at launch** with a missing-dependency error that
   names the app rather than the DLL. Caught by listing the package contents,
   not by the build. `build-app.ps1` now asserts it every time.

**Not verified, and each is a real risk:**

- **Nothing has run on a console.** Every claim in §4 and §5 about Media
  Foundation, XAudio2 and AudioGraph on Xbox is design based on documented
  behaviour, not observation. This is now the *only* large gap.
- **The app container's tolerance of the import table is inferred**, not proven.
  §3 argues these imports resolve; probe stages 2–4 are what will actually
  demonstrate it.
- **UWP memory limits on a console are tighter than on a PC** and vary by
  device and by whether the app is classified as a game. 4K HEVC decode plus a
  swap chain is not small. Worth measuring at milestone 3 rather than assuming.
- **The headset microphone** may be held by system party chat. Untested.

### One local gotcha, already paid for

**PowerShell 5.1 reads a BOM-less `.ps1` as CP1252.** The box-drawing character
`─` (`E2 94 80`) then decodes with `0x94` = a smart double-quote, which opens a
string PowerShell never closes — and the parse error points at a line thirty
lines further down. Both scripts here are saved **UTF-8 with BOM** for this
reason. `deploy.ps1` avoids it by being pure ASCII. Either is fine; silence is
not.

(That sits beside the existing rule in memory: never run `deploy.ps1` with
`2>&1` — PS 5.1 turns cargo's stderr warnings into a terminating error.)

---
## 11. Where to pick this up

The port is live and stable. What remains, in the order it is worth doing:

1. **UI styling to match the Android "Ion" dashboard.** The *behaviour* is
   already matched — persistent host list with paired/presence badges, automatic
   handoff, no chrome at all while streaming, settings behind a gesture. The
   *look* is not: `MainPage.xaml` is functional XAML, not Ion. Read the last
   section of `HANDOFF_ECHO_ANDROID.md` for the design it should meet, and note
   that a TV is a ten-foot display with gamepad focus, so Ion's phone layout is a
   reference and not a template.
2. **Audio, both directions.** Milestone 5, and the one with a real design
   question in it: Windows ships an Opus decoder but no encoder, so the mic path
   needs the codec inside the bridge rather than on the platform side. See §5.
3. **HDR10.** The pipeline is SDR BGRA8 end to end today. The host already
   negotiates and encodes HDR10; the renderer's colour-space plumbing has the
   fallbacks in place but has never been given a PQ stream.

### Before changing anything in the video or input path

Read **§4b** first. Every rule in it was paid for with a blank screen, and three
of the four failures looked like something other than what they were:

- a stream stuck at 1080p that was the client asking wrongly, not the host;
- ten blank sessions from a frame rate the panel could show and the decoder
  could not;
- a permanently blank screen from a single refused `Present1`, with a perfectly
  healthy XAML layer on top of it.

The one process rule worth more than any of them: **`fed / decoded / drawn /
dropped` name the stuck stage exactly, and they were hidden behind a button
nobody had a reason to press.** They are on screen now whenever there is no
picture. Keep them there.
