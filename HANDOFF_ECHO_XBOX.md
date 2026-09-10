# HANDOFF — Echo for Xbox (UWP)

**Status (2026-09-10): LIVE ON HARDWARE AT 4K120.** The console discovers the
host over mDNS, pairs with a PIN, and decodes 3840x2160@120 HEVC in hardware
onto a headless virtual display, with mouse, keyboard and controller reaching
the PC. Clean picture, low latency, no flashing, no frame backlog.

The dashboard is **gesture-driven** as of Phase 3 (§13): no Stream or Pair
buttons, A opens an inline app drawer or launches Virtual Desktop, Start carries
the menu. **A is read from the pad thread, not from XAML — §13.3, and read it
before touching any dashboard input.**

120 fps needed FFmpeg driving D3D11VA directly — Media Foundation hands this app
container no hardware decoder at all (§12.1) — plus a frame queue sized in time
rather than frames (§12.2c). Both are live-confirmed.

What is settled, in one place:

| | |
|---|---|
| Stream | 3840x2160 **@120** HEVC, app 5 (headless Virtual Desktop) — §12.2c |
| Frame rate | 120 via FFmpeg/D3D11VA; no ceiling is enforced any more — §12.1, §13.7 |
| Resolution | the console's **HDMI output** size, never the swap chain's — §4b.1 |
| Present | `SyncInterval 0`, `FLIP_DISCARD`, frame-latency 1, never queue — §4b.3 |
| Pointer | `CreateCoreIndependentInputSource` on a high-priority pool thread — §6.1 |
| Pad | 250 Hz on a high-resolution timer — §6.2 |
| Mouse mode | **client-side**, Menu+View, chord swallowed — §6.3 |
| Overlay | hold **Menu+View** 700 ms; a tap toggles mouse mode — §6.4, §12.2b |
| Live re-mode | `echo_set_display` → the Worker's `apply_hot_display_mode` — §4c |
| Dashboard **A** | from the **pad thread**, never XAML; tap = drawer, tap-tap = Virtual Desktop — §13.3, §13.4 |
| Dashboard **Start** | the menu: Stop Stream, Stream, Pair, Settings — §13.4 |
| Leaving a stream | `echo_detach` — the host **holds** its display; only Start → Stop Stream releases it — §13.5 |
| Pairing | Start menu only, and it does **not** auto-launch — §13.6 |
| Chrome | authored at 3840x2160, scale **measured** not assumed — §13.2 |
| Decode ceiling | **deleted** — the panel rate advises, nothing vetoes — §13.7 |

**Not done:** audio in either direction (§5) and HDR10. The mic slider in
Settings is a deliberate placeholder — §13.9.

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

### 4b.2 The panel's refresh rate is NOT the decodable frame rate — the DIAGNOSIS stands, the CEILING is GONE

**Superseded 2026-09-10, see §13.7.** The pixel-rate budget described below has
been **deleted from the code**: §12.1 explained the ten blank sessions as Media
Foundation handing the app container a *software* decoder, so 520M px/s
described that decoder and never this console. The reasoning is kept because the
failure mode is real and worth recognising; the mechanism is not. Today the
panel rate advises and nothing vetoes. `g_maxDecodePixelRate` and
`DecodableFps` no longer exist — do not go looking for them.

Conflating those two cost **ten consecutive sessions of blank screen**, and the
host was flawless through every one of them: VDD at 3840x2160@120 primary
headless, WGC at 4K, `NVENC READY (hevc @ 3840x2160, 39488 Kbps, 120 fps)`, IDR
on the wire, RTT 5–17 ms, input flowing. A 120 Hz TV had made the client ask for
4K120, and **the Xbox HEVC decoder produces nothing at all from it.**

So the frame rate comes from a **pixel-rate budget**
(`g_maxDecodePixelRate`, 520M px/s in `MainPage.cpp`), and the display refresh
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

**520M is an inference, and as of 2026-09-08 it is no longer the only word.**
What those ten sessions established is that 4K120 produced nothing. What they
did *not* establish is why — a decode block that cannot do the pixel rate and a
Media Foundation transform that declines to try are indistinguishable from a
sofa, and they point at completely different work. `HevcDecoder::Probe` now
reads `MF_VIDEO_MAX_MB_PER_SEC` off the transform and asks `ID3D11VideoDevice`
for a 4K HEVC decoder configuration; `AdoptProbedBudget` replaces the guess when
the decoder declares anything, and the overlay's **Decode limit** button lifts
it for a deliberate test. See "Phase 2" at the end of this document.

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

### 6.4 The overlay gesture — SUPERSEDED 2026-09-08, see §12.2b

**As of 2026-09-08 both gestures are on the chord**: tap Menu+View to toggle
mouse mode, hold it 700 ms for the overlay. View alone is an ordinary button
again and reaches the PC on the tick it was pressed. What follows describes the
arrangement that replaced, and the press-trap reasoning still applies — it moved
to the chord rather than going away.

~~While streaming there is no on-screen UI at all. Holding **View** for 700 ms
opens the overlay (resolution, frame rate, disconnect, diagnostics); a short
press still reaches the PC.~~

View was **withheld** while it might still become a hold — the same delay-rather-
than-retract trap as the Android client's 60 ms touch press-trap. The first
moment of a gesture is indistinguishable from the start of an ordinary press, and
retraction cannot work: the host has already pressed the button. **That cost
every View press 700 ms, which is why the gesture moved.**

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
    MainPage.xaml / .h / .cpp       dashboard, inline app drawer, Start menu,
                                    settings, diagnostics, overlay,
                                    no-video panel, the A gestures (§13)
    VideoRenderer.h / .cpp          D3D11 device, composition swap chain,
                                    HDMI mode, present pacing
    HevcDecoder.h / .cpp            the MFT, and the never-queue drain
    HostDiscovery.h / .cpp          DNS-SD; the advertised fp is a LABEL
    EchoSession.h / .cpp            handle, event pump, feeder, hosts.json
    InputBridge.h / .cpp            core-independent pointer, 250 Hz pad,
                                    mouse mode, the Menu+View chord, and the
                                    dashboard A sink (§13.3)
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

1. ~~**UI styling to match the Android "Ion" dashboard.**~~ Landed 2026-09-08
   (§12.2) and reworked on hardware across Phase 3 (§13): true black, two
   accents, a 4K design space, and a gesture-driven dashboard. What is left is
   the list in §13.9 — the accordion's feel, the drawer's focus path, and
   confirming the field really is black.
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

---
## 12. Phase 2 (2026-09-08) — the decode probe, Ion, and the overlay's missing control

Branch `phase2-xbox-4k120-ion`. Built and signed; **none of it has run on the
console yet.**

### 12.1 The decode probe — measure the ceiling before porting FFmpeg to lift it

The plan for 4K120 was an FFmpeg + D3D11VA decoder. That may still be the
answer, but it was about to be built on top of an unmeasured premise: **D3D11VA
and the Media Foundation MFT are two front doors onto the same VCN decode
block.** If the wall is the silicon, an FFmpeg port costs a week and buys
nothing. Nobody had asked the hardware.

`HevcDecoder::Probe(device)` asks, in two independent ways:

| | |
|---|---|
| `MF_VIDEO_MAX_MB_PER_SEC` | what the transform **declares**. A macroblock is 16x16, so `× 256` is the pixel rate it is claiming. Zero means it exposes nothing, which is allowed and is itself a result. |
| `ID3D11VideoDevice` | profile enumeration, `CheckVideoDecoderFormat`, and `GetVideoDecoderConfigCount` for HEVC Main at 3840x2160 — **the same questions FFmpeg's d3d11va hwaccel asks when it sets up.** A refusal here means an FFmpeg port hits the identical wall one layer down. |

`DecoderProbe::Report()` ends in a verdict in words, because three lines of
capability numbers are only useful to somebody who already knows what they
imply. It runs at startup, lands in the dashboard log, and is repeated in the
overlay's diagnostics.

Three things came out of writing it. **The first two were deleted on 2026-09-10
(§13.7)** once this probe had explained the sessions that motivated them; they
are kept here because the reasoning is what led to the deletion:

1. ~~**`AdoptProbedBudget`**~~ replaced the inferred 520M with the declared
   number when there was one, and re-decided the frame rate the startup path had
   already picked. It did **not** invent a number when the decoder declared none —
   silence is not a claim, and the guess stayed as visible a guess as it was.
2. ~~**The overlay's "Decode limit" button**~~ lifted the budget entirely, so
   4K120 could be tested from a sideload rather than a rebuild. Deliberately not
   sticky: it lived for the life of the process, because the failure it could
   produce is a blank screen and nobody should meet that on a launch they did
   not ask for. Both are gone now — nothing enforces a ceiling, so there is
   nothing left to lift.
3. **`MF_MT_FRAME_RATE` was hard-coded to 60/1** on the input type, commented as
   nominal because it "only helps the decoder size its internal pool". Sizing the
   pool is not nothing — a hardware MFT picks its surfaces and its internal path
   from what it is told — so a decoder handed 60 and then fed 120 is a live
   candidate for the blank screen that got blamed on the silicon. It now carries
   the negotiated rate. **This alone may be the whole bug.**

**What to report back from the console:** the `decoder` / `max rate` / `dxva` /
`verdict` block from the dashboard log, then a 4K120 attempt with the decode
limit off. `decoded 0` with `fed` climbing is the decoder eating frames and
producing nothing — the known signature from §4b.2.

### 12.2 Ion

**The palette in this section was replaced on 2026-09-10 — see §13.1 for the
current one** (pure black, Ion Yellow and Ion Blue). Everything below about
*technique* still holds; only the colours moved.

`Ion.xaml` is a `ResourceDictionary` merged by `App.xaml`. It originally carried
the Android `Theme.kt` palette unchanged — Void, Carbon, Edge, Ion cyan, Matrix
green, and the rule that nothing else may be green. Matrix green and that rule
survive; the ground and the accent do not. The type ramp is deliberately *not*
carried over from Compose: it sizes for a phone at 30 cm, this is read at three
metres — and it is now authored in the 4K design space (§13.2).

The one technique worth knowing: a UWP Button's hover/pressed/focus appearance
comes from its template's VisualStateManager, which reads **theme resources by
key**. Setting `Background` in a `Style` restyles the rest state and nothing
else, so the button turns grey again the moment it takes focus — which on a
console is most of the time. Overriding the keys themselves is what makes every
state consistent, and it does it **without replacing a single ControlTemplate**,
so the console's own focus behaviour — which gamepad navigation depends on
entirely — is left exactly as Microsoft shipped it.

`Ion.xaml` must be a `<Page>` in the vcxproj, not `<None>`. As `None` it is
packaged but never compiled, and the failure is a runtime "cannot find a
resource with the key" on first navigation rather than anything at build time.

### 12.2b The chord now carries both gestures, and View got its latency back

Menu+View: **tap toggles mouse mode, hold (700 ms) opens the overlay.** The
View-hold gesture is gone.

That is better than consistency alone, and the reason is worth keeping. The old
arrangement had to **withhold every single View press for 700 ms** in case it
turned into a hold — so an ordinary Back/View press reaching the game was always
a third of a second late, on a button plenty of games bind. Only the chord pays
that delay now, and a chord pressed together is never meant for the game anyway.

Two rules inside it:

- **Nothing happens on the press edge.** Same delay-rather-than-retract rule the
  old View trap followed: the first moment of a hold is indistinguishable from a
  quick press, and retraction cannot work, because by the time we know we have
  already toggled the mode and un-toggling it is a second visible event the user
  did not ask for.
- **`chordFired` makes the two exclusive.** Without it, holding the chord opens
  the overlay at 700 ms and *also* toggles mouse mode on release — so every
  overlay summon would leave the controller driving a cursor nobody asked for.

The whole chord is still swallowed either way, which is still what keeps
`gamepad_mouse.rs` out of the picture (§6.3).

### 12.2c 4K120 is LIVE — and the four bugs between here and there

**Live-confirmed 2026-09-09**: 3840x2160@120, 56 of 61 host samples at 120-121
fps, **4 invalidations and 6 IDRs for the whole session**, worst frame age 92 ms.
The comparable session two builds earlier had **822 invalidations**.

Getting there took four fixes, and the order matters because each one hid the
next.

**1. The frame queue was sized in FRAMES, not TIME** (`echo-client/src/frames.rs`).
`CAPACITY = 3`, documented as "~50 ms of slack at 60 fps" — which is 25 ms at
120. The number was right and the unit was wrong, and it halved at exactly the
rate this client exists to reach. It is now `SLACK_MS = 50` sized against
`MAX_FPS`.

**The cascade this produced is the part worth remembering**, because none of it
looks like a queue problem from the sofa:

  1. the queue drops a frame locally
  2. `last_delivered` jumps, so the receiver reports **transit loss** — the
     client asks the host to invalidate frames that were never lost on the wire
  3. the drop re-arms the keyframe gate, so more frames are refused behind it
  4. at 4K the host's DPB is 5 frames, so a 4-5 frame invalidation range
     immediately exceeds it: `RFI range … >= DPB 5 — forcing IDR`, then
     `[LTR] no usable long-term reference`. **Both repair rungs are
     structurally unable to help at this resolution.**
  5. so every one costs a full 4K keyframe — 84 in 400 log lines, 20 Mbps of
     repair traffic, and a picture that visibly blinks

**2. `HdmiDisplayHdrOption::EotfSdr` does not work.** It reads like the obvious
way to say "drive this SDR" and the console answers `E_INVALIDARG` — **thrown,
not returned**, so it escaped through the outer catch, the mode was never set,
and the console sat at 1920x1080@60 with the swap chain sized to match. One
wrong enum cost the app 4K entirely, and it presented as a UWP container
restriction.

`moonlight-xbox` uses **`None` for SDR and `Eotf2084` for HDR, and nothing
else** (`State/MoonlightClient.cpp:137-150`). Checked against their source
rather than reasoned about. Each attempt now has its own try/catch, because
"returns false" and "throws" are both real answers and only one was handled — a
fallback that cannot run is not a fallback.

**3. The green bar was padding, not stride.** A decoder surface is allocated at
the ALIGNED size (1088 rows for 1080), and `BlitFrame` sized its video processor
from `srcDesc`, so eight rows of undefined memory were stretched onto the
screen. Fixed with `VideoProcessorSetStreamSourceRect` at the coded size, which
is free because the processor is already scaling.

**4. `extra_hw_frames`.** The D3D11VA pool is sized from the stream's DPB on the
assumption the caller returns every frame immediately; this decoder holds one
for the renderer. Missing, the decoder cannot obtain a surface and it presents
as missing references.

#### Two dead ends recorded so they are not re-run

- **A private NV12 copy in `TryGetFrame` was built, tested and removed.** The
  theory was that handing out the decoder's live pool surface aliased against
  its own reference pictures. It does not: `fed == decoded` with every decoder
  counter at zero was the frame queue all along. And the copy was actively
  harmful here — it runs on the present thread inside `m_lock`, which `Submit`
  also needs, so a per-frame 4K copy put the decode-context mutex and the shared
  D3D context in the feeder's way: `dropped overflow` 308 and worst frame age
  483 ms at 4K120, against 0 at 4K60.
- **Do not make the HDMI refresh rate a hard cap on the requested fps.** It was,
  briefly, and it immediately removed the operator's ability to test 120 at all
  when the output read 60 Hz. One reading from one API is too thin a basis for
  overruling an explicit press. It warns now.

#### The diagnostic that ended it

`echo_stats` had been in the bridge since the start and this client never called
it. It already tracked `frames_dropped_overflow` — the exact counter that names
this fault — while several sessions went into adding decoder-side counters that
**structurally could not see it**, because a frame that never completes never
reaches the decoder. `fed == decoded` and "the picture is decaying" are entirely
consistent, and that is the trap.

**Before adding an instrument, check what the shared core already measures.**

### 12.3 The overlay already existed — what was missing was one control

Worth recording, because it is the third time on this project: the request was
to *build* an in-game overlay with disconnect, mouse-mode toggle and
diagnostics. **Disconnect and diagnostics were already there** (§6.4), along
with live resolution and frame-rate switching. What was genuinely missing was
the mouse-mode control, and the real complaint underneath it — that the chord
toggles **silently**.

(That "Disconnect" button is now **"Leave the stream"** and it *detaches* rather
than ending the session — §13.5. The toast below has been generalised into
`ShowToast` and is used for refusals on the dashboard too.)

Two additions:

- **`MouseModeButton`**, because a chord is not discoverable and a user who hits
  Menu+View by accident had no way back that did not require knowing what they
  had pressed.
- **A toast**, 1.8 s, bottom centre, `IsHitTestVisible="False"`. It appears over
  a live stream while input is being forwarded, so it must never take focus, eat
  a button, or move where the gamepad is pointing.

**The button does not write the flag.** `InputBridge::RequestMouseMode` posts an
atomic request that `PadLoop` consumes, and the loop performs the transition
through the identical path the chord takes. That indirection is load-bearing: a
transition is four things, not a bool — the flag, a zeroed snapshot on **both**
edges (a pad that merely goes quiet leaves the host holding Menu, and games read
Menu as pause), lifting whatever the triggers were holding on the way out, and
clearing the sub-pixel accumulator. A UI thread that set the atomic directly
would skip three of the four, and the resulting bug — a click that outlives the
mode that made it — would present as a host-side input fault a long way from
here.

The request is consumed **before** the forwarding gate, because the overlay is
open (and forwarding therefore parked) at exactly the moment the button is
pressed.

Also fixed: `InputBridge.h`'s header comment still said mouse mode was
implemented host-side and that "this file must not implement it". That was true
of an earlier draft and had been backwards since 2026-09-07.

---
## 13. Phase 3 (2026-09-10) — the dashboard becomes gesture-driven

Four rounds of live testing, and the headline is not the styling: **the
dashboard has no Stream or Pair buttons any more.** A drives launching, Start
carries everything else, and the two arrive by completely different routes for a
reason that took three builds to find.

Read §13.3 before touching anything that reads a button on the dashboard.

### 13.1 True black, and where the blue was actually coming from

The palette is `#000000`, not the old `#050508`. That is a functional choice
before an aesthetic one: a Mini-LED or OLED panel switches a dimming zone **off**
only for a genuinely black pixel, and a near-black keeps every zone under the UI
lit at its floor — visible as a grey haze framing the video.

One cyan became two accents with a rule, because a single hue on black gives a
screen no depth:

| | |
|---|---|
| **Ion Yellow** `#FFF200` | WHERE YOU ARE / WHAT YOU CHOSE — focus, selection, the active mode |
| **Ion Blue** `#2B8CFF` | WHAT IS HAPPENING NOW — live state, telemetry, a value still moving |
| **Matrix green** | unchanged: THE NETWORK ANSWERED, and nothing else |

The yellow is a *true* yellow rather than the chartreuse that "neon yellow"
usually means, and that is the Matrix rule doing work: at three metres a
green-shifted yellow and Matrix green are the same colour.

**The pixels that were reported as blue-grey were not XAML's.** They came from
the SWAP CHAIN. `VideoRenderer`'s background clear still matched the old palette
(`RGB(5,5,8)` — and blue-dominant, B8 against R5), and the `SwapChainPanel` is
stretched across the whole window ABOVE the Page's fill, the Frame's, and every
theme brush. Whatever that clear paints **is** the app's background wherever the
chrome does not cover it. A "the background is not black" report is answered
there first and in XAML second.

The XAML side is now hardened too — `ApplicationPageBackgroundThemeBrush` and
three sibling page grounds overridden, plus an explicit black on the root
`Frame`, which sits between the CoreWindow and the Page and had been using the
theme's lifted near-black. That is defence in depth, not the fix.

### 13.2 The 4K design space

`ApplicationViewScaling::TrySetDisableLayoutScaling(true)` at launch asks the
console for native pixels instead of a 200%-scaled 1080p view. But the layout
does **not** trust the answer: `ChromeRoot` is a fixed 3840x2160 canvas whose
scale is **measured** from the view it actually got (`ApplyChromeScale`), so

- request honoured → view is 3840x2160 logical, scale 1.0, every number lands on
  the pixel it names;
- request refused (a PC, an older console, a policy change) → view is 1920x1080,
  scale 0.5, and the same layout renders at exactly the proportions it would
  have had anyway.

The bool that call returns says whether the request was *accepted*, which is a
different question from what the view ended up being. A layout built on the
first question is wrong, silently, in every case where they disagree.

**The type ramp is 1.6x the old 1080p-space numbers, not 2x.** At 2x the chrome
would be pixel-identical to before — still enormous — and at 1x it would be half
the physical size and unreadable from a sofa. 1.6x lands at about 80% of the old
physical size: denser, more on screen, still above the ten-foot floor (~24 px
measured in the 1080p space, which is 38 here).

`ChromeRoot` is `HorizontalAlignment="Left" VerticalAlignment="Top"` on purpose —
a fixed-size child is centred by default, and a 3840-wide child centred in a
1920-wide parent starts 960 px off the left of the screen. The leftover after
scaling is taken out by hand in `ApplyChromeScale`, because `RenderTransform`
does not move layout.

### 13.3 A COMES FROM THE PAD THREAD — read this one

**`GamepadA` is not yours to intercept from XAML.** A is the console's Accept
button; the framework runs its own state machine over it to invoke whatever has
focus, and on this hardware a page-level `PreviewKeyDown` **never saw it at
all** — not with focus on a host row, not with focus on the page, not with the
press marked handled and not with it left alone.

Three builds were spent on the wrong side of that boundary. What finally named
it was **which button still worked**: `GamepadMenu` arrived through the *same*
handler, behind the *same* arming condition, through the *same* intercept, every
single time. That ruled out the intercept, the condition and the focus rect
together — the only thing not shared was the key itself.

The fix is `InputBridge`'s existing 250 Hz `Windows.Gaming.Input` poll — the
same mechanism the Menu+View chord has always used. It predates XAML's focus
entirely and **no control can consume from it**, because no control is on it.
`SetSinks` gained an `accept` sink that fires on A's rising edge, and only while
forwarding is parked, so during a stream A belongs to the game untouched.

Three rules that are load-bearing:

1. **Nothing is suppressed.** XAML still gets its own copy of A and still
   invokes whatever has focus. That is what keeps menus working — on the bare
   dashboard there is nothing for A to activate (`IsItemClickEnabled="False"` on
   the host list), and the moment a panel is open the page ignores the sink and
   lets XAML press the focused button.
2. **`FocusInsideDrawer()` is the arbitration.** A on the host list toggles the
   drawer; A on a tab inside it launches that app. Same physical press, same
   thread — what it is pointing AT is the only thing that can tell them apart.
3. **The sink arrives on the PAD THREAD.** `OnPadAccept` hops to the dispatcher
   before touching a timer or a panel. Getting that wrong is silent, not a
   crash: `DispatcherTimer` off-thread throws inside a `noexcept` pad loop, and
   the visible result is a gesture that does nothing.

**Both gestures resolve on the PRESS, never the release.** An earlier build
queued the outcome on the down edge and executed it on the up edge, and neither
gesture ever fired — marking a `GamepadA` KeyDown handled is what stops its
KeyUp from being routed at all. Suppressing a press and then waiting for its
release asks for an event that the act of suppressing removed. A first press
arms a 400 ms timer; a second inside the window IS the double tap and fires
immediately.

**The Diagnostics screen counts both paths**: `A pad N :: A xaml N down / N up`.
`pad` climbing while `xaml` stays at **zero** is the expected reading and the
evidence for this whole design. If `pad` does not climb while A is pressed, the
pad thread is not running or forwarding is on. If `xaml` starts climbing, the
platform changed and §13.3 needs revisiting.

### 13.4 The gesture map, and the inline drawer

| gesture | unpaired host | paired host |
|---|---|---|
| **A** | amber `NOT PAIRED :: press Start` toast | toggle the app drawer |
| **A A** | same toast | launch Virtual Desktop (app 5) |
| **Start** | the menu: Stop Stream, Stream, Pair, Settings | same |
| **B** | unwinds the innermost panel; leaves the app from the bare dashboard | same |

**The drawer is inline, not a flyout.** It lives in its own `Auto` row of
`SetupRoot` directly under the host card, so the card's `*` row gives up exactly
the height the drawer takes and the layout genuinely moves. It animates its own
`Height` (160 ms, ease-out) between 0 and its natural size.

Four things about it are not obvious:

- **The natural height is measured through real layout** — set `Height` to Auto,
  force a pass, read `ActualHeight` — not by measuring the content by hand. Hand
  measurement gets the hint line's wrapping wrong, because that depends on the
  width the row actually gets.
- **A `Grid` does not clip its children.** Without the `RectangleGeometry` on
  `AppDrawer.Clip`, resized from `SizeChanged`, the card spills over the hint bar
  for the whole of a collapse.
- **`EnableDependentAnimation` is not optional.** `Height` is a layout property;
  without it the animation silently does nothing and the panel simply appears at
  its final size, which looks like the code was never called.
- **Focus deliberately STAYS on the host list when it opens.** That is what lets
  a second A close it. Down moves into the strip when the user wants it.

Virtual Desktop is deliberately **absent** from the strip: it is what a double
tap launches, and listing it would make the fast path look like one more equal
option. The strip is Steam / Xbox / RetroArch / Mirror, each with a description
that follows the focus rect — because "Mirror" and "Virtual Desktop" sound like
the same thing and behave completely differently, and somebody picking blind
gets it wrong about half the time.

The drawer only ever opens for a **paired** host, with a backstop inside
`ShowAppDrawer` itself: every entry in it launches something, launching needs a
fingerprint, so for an unpaired host it would be four buttons that all answer
with the same refusal.

### 13.5 Two-step teardown — `echo_detach` is new, and it is the whole feature

`echo_close` **sends the host a goodbye** (`stop_session`), which the host
answers by tearing the session down and handing back the display it was driving.
So before this, every way out of a stream was the destructive one.

New bridge entry point `echo_detach(handle)`: same teardown, minus the wait for
the session task to unwind, and **the `stop` watch channel is deliberately NOT
signalled**. Signalling asks the task to shut down gracefully, and a graceful
shutdown is what sends the goodbye — whether it won the race with the runtime
drop would decide whether the user's monitor came back, so the race is removed
rather than tuned. The host sees the client go quiet, takes
`detach_on_disconnect`, and **holds the virtual display for its detach grace
period** so a reconnect walks back into the same desktop.

| action | call | host result |
|---|---|---|
| overlay → **Leave the stream** | `echo_detach` | detaches, **holds** its display |
| Start → Stop Stream → **End Stream**, live | `echo_close` | tears down, releases |
| Start → Stop Stream → **End Stream**, detached | `echo_release` | tears down, releases |

`echo_release` needs no session at all — ending a session never did — which is
what makes the detached case work when there is no handle left to ask. The
config used for the stream is kept for exactly this.

Both block, so both run on a pool thread: `echo_close` can take a second and a
half waiting for the goodbye, and on the UI thread that is a frozen screen at
the moment somebody is watching for their monitor to come back. **A failed
release does not clear the held state** — the badge and the confirmation behind
Start are the only two things pointing at an outstanding session, and clearing
them would leave a monitor nobody can get back and no sign anything is wrong.

### 13.6 Pairing initiates from Start only, and ends at paired

Two deliberate reversals of earlier behaviour:

- **`BeginPairing` has exactly one caller**, the Start menu's Pair button. The
  double tap and the app strip no longer fall through to it. Pairing is not a
  fast action — it opens a session, puts a PIN on the television and asks
  somebody to walk to another room — and reaching that by mashing A is a worse
  failure than not reaching it at all.
- **Pairing no longer auto-launches a stream.** Setup and streaming are two
  decisions; running them together means a user who wanted to set the console up
  now owns a live session, a virtual display on the PC, and a teardown to
  perform before they can do anything else.

`closed` fires whether the handshake succeeded or not, so success is read back
from what pairing actually **left behind** (`SelectedHostPaired()`) rather than
assumed from having reached that point. The host list is *rebuilt* rather than
re-labelled — each row's badge is built from `LoadHostFingerprint` at
construction, so PAIRED only appears if the rows are made again.

One trap worth keeping in mind: the Start menu's Pair button is gated on
`CanPair()`, which needs a selected host, and selection follows the focus rect.
On a freshly-booted console nobody has touched there is no selection — so the
**only** entry point to pairing would open with its button greyed out, and a
disabled control cannot take focus on a gamepad. Visible, greyed, unreachable.
`ShowActionMenu` now selects the first host when none is selected.

### 13.7 The decode ceiling is DELETED

`g_maxDecodePixelRate`, `DecodableFps` and the overlay's "Decode limit" toggle
are gone — removed, not defaulted off.

520M px/s was drawn through one data point: ten blank 4K120 sessions. §12.1 has
since **explained** those — Media Foundation handed the app container a software
HEVC decoder, and a software decoder was never going to do 4K120. The number
described that decoder, not this console. FFmpeg/D3D11VA is the decoder now,
4K120 is live, and a ceiling whose only remaining function is to refuse the
thing the port was built to do is worse than no ceiling.

What survives is the **panel refresh rate, which advises and does not veto** —
asking for more frames than the TV can show is real waste and worth naming, and
still the user's call. The decode probe (§12.1) is untouched and still on the
Diagnostics screen: it was always the honest half of this, a measurement sitting
next to the guess that used to overrule it.

### 13.8 Four smaller things, each of which cost something

- **`FfmpegHevcDecoder` reported `private copy` long after the copy was gone.**
  The frame path hands the renderer the decoder's own surface, and
  `CreateTexture2D` / `CopyResource` / `CopySubresourceRegion` appear **nowhere**
  in `FfmpegHevcDecoder.cpp`, `HevcDecoder.cpp` or `VideoRenderer.cpp`. That
  string's entire job is to say which pipeline is on the console; a stale one
  answers wrongly with total confidence. It reads `zero-copy` now, and the
  comment above it names what makes the claim checkable.
- **`/utf-8` added to the vcxproj.** The sources are UTF-8 with no BOM, so MSVC
  was reading them in the system ANSI code page: a multi-byte character inside a
  *wide string literal* became two wrong characters, and an interpunct in a
  status line reached the television as mojibake. Comments were unaffected,
  which is why it survived — the damage only showed in text the user reads.
- **`HostList().Focus()` right after appending items does not work.** A ListView
  creates a row's visual lazily during layout; until then there is no
  `ListViewItem` to receive focus and `Focus()` returns false. The symptom was a
  dashboard where the D-pad highlighted nothing, and the tell was the
  workaround — opening the Start menu and backing out calls the *same* `Focus()`,
  a second later, and worked every time. `FocusHostList()` forces the layout
  pass, focuses the **container** rather than the list, and posts one retry at
  `Low` priority.
- **XAML-referenced handlers must be `public`.** The generated code is not a
  friend of the implementation class, so a handler in the private section is a
  C2248 raised from inside `MainPage.xaml.g.hpp`.

### 13.9 Still not verified on hardware

Everything in §13 was reasoned and compiled, not measured, except where a live
report is quoted:

- the 160 ms accordion feel, and whether Down out of a single-item ListView
  lands cleanly on the drawer's tabs;
- whether the field is now genuinely black. If it is not, the next suspect is
  the swap chain being created as an `_SRGB` format — zero survives that
  unchanged, so it would point somewhere else entirely;
- the mic passthrough slider in Settings is a **placeholder**. It is a real,
  focusable control with a remembered value that is sent nowhere; the panel says
  so in words. It is enabled rather than disabled on purpose — a disabled
  control cannot take focus, so on a console it is invisible to the only input
  device there is, and the point of putting it in early is to settle the layout
  and the navigation order before the audio path arrives.
