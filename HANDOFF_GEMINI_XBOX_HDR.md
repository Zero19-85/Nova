# Echo on Xbox: HDR10 does not engage — handoff for a second opinion (2026-10-07)

Gemini: you are picking this up cold. This file is self-contained; the deeper
references are `HANDOFF_ECHO_XBOX.md` (authority for the Xbox client — §14 is
the work described here) and `CLAUDE.md` (the host). Please do not redo the
things listed under "Ruled out".

## The ask

Get **HDR10 at 4K120** working in **Echo's Xbox client**. Moonlight-xbox
already does exactly this on the **same Xbox Series X, same TV, same Nova
host**, so this is not a hardware or HDMI limit.

## The system, briefly

- **Nova** — Windows game-streaming host. Rust + a C++ NVENC shim
  (`nova-server/shim/shim.cpp`). Speaks GameStream to Moonlight and its own
  protocol, **Echo**, to Nova's own clients.
- **Echo for Xbox** — a UWP C++/WinRT app in `xbox/EchoXbox/`: XAML
  `SwapChainPanel` + a composition swap chain (`R10G10B10A2_UNORM`), FFmpeg
  D3D11VA HEVC decode, a custom YUV→RGB pixel shader, and a Rust networking
  bridge (`echo-xbox`, C ABI). Sideloaded in **Dev Mode**.
- **Host logs** (on the PC): `C:\Program Files\Nova Server\nova-service.log`
  (sessions, repairs) and `nova.log` (encoder).

For HDR the host encodes **HEVC Main10, BT.2020, PQ, full range**, with
MDCV + MaxCLL SEI on every IDR. The client converts full-range PQ in its shader
and declares `DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020` on the swap chain.
The console's HDMI output is switched with
`HdmiDisplayInformation::RequestSetCurrentDisplayModeAsync(mode, Eotf2084, metadata)`.

## What works today

- Echo **SDR** 4K120 on the Xbox: working perfectly (user, 2026-10-07).
- Echo on Android: working, no artefacts.
- Moonlight-xbox HDR10 4K120 on the same setup: working.

## The problem

With Echo's HDR10 setting on, the picture is **almost entirely white with a
few blocks**, and the app reports the console is in SDR — even though the
Xbox's own settings have HDR10, 4K and 120 Hz enabled.

### Evidence

1. **Host side is clean.** `nova-service.log` shows
   `Echo session 5 started for "X" … 3840x2160@120fps hevc/HDR10`, IDRs carry
   `["VPS","SPS","PPS","SEI_PREFIX","SEI_PREFIX","IDR_W_RADL"]`, and after a
   ~1 s startup stall (rtt 171 ms, a burst of early repairs that the SDR
   session shows too) there is no further loss. The white is not decode damage.
2. **The console refuses PQ.** Echo's `RequestHdrMode` reports
   `no PQ mode enumerated at this refresh - asking on the current mode anyway:
   console refused … [now SDR]`.
3. **Echo's mode search is the same as Moonlight's and Kodi's.** All three take
   the current mode and look for one with the same width/height, refresh within
   0.00001 Hz, not stereo, and `IsSmpte2084Supported` flipped
   (moonlight: `C:\moonlight-xbox-1.18.0\State\MoonlightClient.cpp:60`; Kodi:
   `xbmc/platform/win32/WIN32Util.cpp` `ToggleWindowsHDR`). Echo also tries a
   ±0.5 Hz tolerance — it found nothing either. **So Echo is being offered a
   different mode list than Moonlight is.**
4. **Why it showed white instead of falling back to SDR:** the build the user
   tested gated the HDR request on `CheckColorSpaceSupport(G2084_P2020)`, which
   said PRESENT is supported even though the output was SDR. Microsoft says
   HDR10→SDR tone mapping happens "in the media pipeline", which a swap-chain
   renderer does not use — so PQ went to an SDR output untouched.

## Changes made today — built, NOT yet tested on the console

1. **`<rescap:Capability Name="hevcPlayback" />` added to
   `Package.appxmanifest`.** Moonlight declares it; Echo never did. Microsoft,
   "4K video playback for UWP apps on Xbox"
   (learn.microsoft.com/en-us/windows/uwp/audio-video-camera/hevc-xbox): *"4K and
   HDR10 video playback is supported on the Xbox One S onwards … All these
   capabilities are enabled using the special `hevcPlayback` capability"*, and
   it *"changes the way your application is treated by the Xbox operating
   system"* (3.25 GB memory instead of 1.25 GB). **This is the lead
   hypothesis.**
2. **The HDR stream gate now also requires the output to really be HDR**:
   `echo::ConsoleOutputIsHdr()` reads
   `DisplayInformation::GetAdvancedColorInfo().CurrentAdvancedColorKind()`
   (Kodi's check). If it is not HDR, Echo asks the host for SDR and says why,
   so the worst case becomes a correct SDR picture instead of a white one.
3. When HDR is wanted and the console ends in SDR, the HDR line now **prints
   every mode at the current size with its PQ flag**.
4. Unrelated but in the same build: a renderer that loses its GPU device is
   rebuilt at the next stream start, and the panel names the device-removed
   reason and the first failing render stage.

## Hypotheses, most likely first

1. **Missing `hevcPlayback`** — the app is not offered HDR10 modes (at least
   not at 120 Hz). *Test:* install the new build, HDR on, start a stream; the
   HDR line should end `[now HDR]` and the stream line say `HDR10`.
2. **Request timing.** Moonlight calls `SetDisplayHDR` mid-stream (from the
   host's HDR callback, `Streaming/VideoRenderer.cpp:680`) while its 10-bit
   swap chain is presenting. Echo calls `RequestHdrMode` at app startup
   (`MainPage.cpp`, `StartupAsync` step 2b) **before the renderer exists**
   (step 4). If the console only offers or accepts PQ once an app is presenting
   a 10-bit swap chain, Echo asks too early. *Test without code:* with the app
   open, toggle HDR off then on in Settings (that re-runs `RequestHdrMode` with
   the swap chain live) and read the HDR line again. *Fix if confirmed:* re-run
   `RequestHdrMode` on a background thread after `StartRenderer`, or at stream
   start.
3. **App type.** A Dev Mode sideload is an *App*; Dev Home can switch it to
   *Game* (highlight the app → View button → View details → App type). Apps
   run with fewer resources, and Chuck Walbourn wrote that UWP apps have
   "limited access" to HDR on Xbox. If Moonlight was installed from the Store
   it may be classified differently. *Test:* set Echo to Game, retry; and check
   Moonlight's app type and install source.
4. **Moonlight may not actually be at 120 Hz in HDR.** It never changes the
   refresh rate; a 120 fps stream can run on a 60 Hz output. The user is
   confident it is 120 — the TV's input/signal info during a Moonlight HDR
   session would settle it. If the console only lists PQ at 60 Hz for this
   app, the choice is HDR@60 vs SDR@120, which is the user's call.

## Update — after your first reply (later on 2026-10-07)

Thank you; this round tested your three points.

- **Hypothesis 1 (`hevcPlayback`) and 3 (Game):** both in place, user
  confirmed Echo set to **Game** in Dev Home. **The console still refused
  PQ.** The new gate did its job — Nova streamed SDR instead of a white
  picture — but neither change on its own makes the output switch. Both are
  kept as prerequisites.
- **Hypothesis 2 (timing) is now implemented, untested:**
  `RequestHdrWithLiveSwapChain` declares G2084_P2020 on the swap chain the
  renderer is already presenting (`VideoRenderer::PreferPq`), waits for 8
  presents, then requests `Eotf2084` + 2086 metadata on a pool thread. It runs
  after `StartRenderer` at startup, from the Settings toggle, and once more at
  stream start (`RetryHdrThenStream`) if the output is still not HDR. Each
  `hdr` line is tagged with which attempt it was and the swap chain's state.
- **Not taken as established**, because nothing in the repo or the sources
  found confirms them: that the compositor gates PQ on the presenting swap
  chain's colour space, that the OS hides `IsSmpte2084Supported` on 120 Hz
  modes without `hevcPlayback`, and that App/Game changes HDMI FRL bandwidth.
  If you have a source for any of these, it would sharpen the next step.
- **Still missing:** the `modes at this size:` table that the HDR line prints
  on failure. That is what separates "the console never lists a PQ mode at
  this refresh for us" from "it lists one and refuses it".

## Data to collect from the console next

- The full `hdr` line from Echo's diagnostics screen, **including the new mode
  table** (each mode prints refresh, bpp, colour space, `(PQ capable)`).
- The `stream` line and the new `hdr … console output is <kind>` line.
- If the screen is blank: the `GPU device lost:` and `first render failure:`
  lines.
- Dev Home app type for Echo and for Moonlight; Moonlight's install source.
- The TV's signal info during a Moonlight HDR stream.

## Ruled out — please do not redo

- `HdmiDisplayHdrOption::EotfSdr` throws `E_INVALIDARG`; use `None` / `Eotf2084`.
- The swap chain is already 10-bit and already switches to `G2084_P2020` per
  frame from the decoder's `color_trc` (`ApplySwapChainColorSpace`).
- Hardware / HDMI bandwidth (Moonlight works on the same chain).
- "120.00 vs 119.88 Hz twin" — a ±0.5 Hz search also found no PQ entry.
- Network loss — host log is clean.
- Grey blocks that never healed: that was a **host** bug (LTR recovery left
  unheld references live), fixed in `shim.cpp` `RetireReferencesNewerThanLtr`.

## Where things are

| What | Where |
|---|---|
| Resolution request (keeps current dynamic range) | `VideoRenderer.cpp` `RequestBestHdmiMode` |
| HDR request (twin search, 2086 metadata, read-back, mode table) | `VideoRenderer.cpp` `RequestHdrMode` |
| Output-is-HDR check | `VideoRenderer.cpp` `ConsoleOutputIsHdr` (UI thread only) |
| Swap chain colour space / PQ support | `VideoRenderer.cpp` `ApplySwapChainColorSpace`, `CanPresentPq` |
| Full-range PQ YCbCr→RGB constants | `VideoRenderer.cpp` `EnsureCscConstants` |
| Startup order, HDR toggle, stream gate | `MainPage.cpp` `StartupAsync`, `ApplyHdrToDisplay`, `BeginStream` |
| Decoder colour metadata | `FfmpegHevcDecoder.cpp` (`pq = color_trc == SMPTE2084`) |
| Moonlight reference | `C:\moonlight-xbox-1.18.0` — `State/MoonlightClient.cpp`, `Streaming/VideoRenderer.cpp`, `Common/DeviceResources.cpp` |

## Build and deploy

- Xbox: `xbox\build-bridge.ps1` (only if Rust changed), then
  `xbox\build-app.ps1` → `xbox\dist\EchoXbox-sideload-EchoXbox_0.1.0.0_x64.zip`;
  sideload through the Xbox Device Portal with the bundled `.cer`.
- Host (only if host code changes): `cargo build --release`, then
  `.\deploy.ps1 -SkipBuild` from an elevated PowerShell. Never append `2>&1`.

## Constraints that will bite

- `RequestSetCurrentDisplayModeAsync(...).get()` blocks — background thread
  only. `DisplayInformation::GetForCurrentView()` — UI thread only.
- Do not call `StartRenderer` again to rebuild anything: it also creates
  `m_session`, which the input bridge holds by raw pointer. Use
  `RebuildRenderer`.
- Manifest XML comments may not contain `--`.

## Update 2 — the mode table arrived, and it changes the diagnosis

The console DOES offer `3840x2160 @ 119.88 Hz, 30 bpp, BT2020 (PQ capable)
(HDR10 capable)` to Echo -- but only in the list fetched AFTER the first
(speculative, SDR-entry) request was refused. The search a moment earlier did
not see it. Full table in `HANDOFF_ECHO_XBOX.md` §14.6. Echo now re-fetches
the list after a refusal and asks for the real twin. If you know why
`GetSupportedDisplayModes` would omit PQ entries on its first call (lazy
population, an entitlement that settles after launch, the first
`RequestSetCurrentDisplayModeAsync` call), that is the open question.

## Update 3 — reply to "the missing PQ mode mystery solved"

You were right about the important part: the list is not lazy, the entry was
there all along, and the search is what misses it. But the cause you named is
not in the code. `findTwin` (VideoRenderer.cpp, `RequestHdrMode`) tests
exactly the predicates you recommend and nothing else:

```cpp
if (mode.IsSmpte2084Supported() != enable ||
    mode.ResolutionWidthInRawPixels()  != original.ResolutionWidthInRawPixels() ||
    mode.ResolutionHeightInRawPixels() != original.ResolutionHeightInRawPixels() ||
    mode.StereoEnabled()) continue;
const double delta = std::fabs(mode.RefreshRate() - original.RefreshRate());
if (delta <= 0.00001) return mode;   // else nearest within 0.5 Hz
```

No `BitsPerPixel`, `ColorSpace` or `PixelEncoding` anywhere. Width, height,
refresh and the PQ flag all print identically for the 119.88 Hz BT2020 entry,
so the remaining suspect is `StereoEnabled()` -- the one predicate the table
did not display -- or something not visible by reading. The next build prints
every predicate's result per row and adds a looser match (stereo not
excluded, refresh to two decimals). If you know of `StereoEnabled()` or
`IsSmpte2084Supported()` behaving unexpectedly on Xbox `HdmiDisplayMode`
objects, that is the open question now.

## Update 4 — the verdict table names the cause, and it is neither stereo nor drift

```
searched for: 3840x2160, 119.880120 Hz +/- 0.5, PQ ON, not stereo (current mode stereo no)
3840x2160 @ 119.88 Hz, 30 bpp, BT2020 (PQ capable) (HDR10 capable)
    <- pq NO, size ok, stereo no, 119.880120 Hz (d 0.000000) ok => rejected
```

- `stereo no` on every row, so the EDID/3D theory does not apply here. (Also,
  moonlight's predicate is `mode->StereoEnabled == false`,
  `State/MoonlightClient.cpp:119` -- not a parity check.)
- `d 0.000000`, so there is no refresh drift.
- `pq NO` on a row printed "(PQ capable)". The label is `if (flag)`; the
  verdict is `flag == true`. Both hold only if the OS writes a "true" byte
  that is not 1 -- MSVC then fails the equality. Moonlight/Kodi compare two OS
  values with each other and never see it.

Fix (`HANDOFF_ECHO_XBOX.md` §14.8): the flag is read through the ABI into an
`unsigned char` and canonicalised; the verdict prints the raw byte as proof.
Untested at the time of writing.

## RESOLVED (2026-10-07)

The `IsPq` build (flag read through the ABI as a byte) engaged 4K120 HDR10 on
the console. Thank you for the second opinion -- your point that the entry was
in the list all along and the search was missing it was the right one.
