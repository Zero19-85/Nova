# FFmpeg + D3D11VA on Xbox UWP — the integration path

**Status: planned, nothing built.** This is the recipe and the reasoning, written
before any of it is done so the gate at the top can still stop it.

`HANDOFF_ECHO_XBOX.md` is the authority for the client. This covers one proposed
change to §4: replacing the Media Foundation decoder with FFmpeg driving
D3D11VA directly.

---

## 0. The gate — do not skip this

**D3D11VA and the Media Foundation MFT are two front doors onto the same VCN
decode block.** If the ceiling is the silicon, this port costs a week and buys
nothing. So the first artifact is not code, it is three lines from the probe
already shipped in `HevcDecoder::Probe`:

```
enumerated  hardware: N (…)   software: N (…)
max rate    … MB/s = …M px/s
dxva        HEVC Main yes/NO   Main10 yes/NO   NV12@4K yes/NO   configs N
```

| What comes back | What it means | Build this? |
|---|---|---|
| `hardware: 0` **and** `dxva … configs > 0` | MF offered the container no hardware decoder while the D3D11 video device offers 4K HEVC. **An MF policy boundary, not silicon** — and D3D11VA talks to the layer that said yes. | **Yes.** This is the case the port exists for. |
| `hardware: ≥1`, decoder is hardware, 4K120 still blank | The MFT is real but declines the rate. FFmpeg *might* configure it differently; the win is plausible but unproven. | Probably — but re-test with the `MF_MT_FRAME_RATE` fix first (see below). |
| `dxva … configs 0` or `HEVC Main NO` | The video device itself refuses 4K HEVC. FFrmpeg asks this exact question during setup and gets this exact answer. | **No.** Nothing above the silicon can route around it. |

**One thing to re-test before believing any of the above**: until 2026-09-08 the
decoder's input type declared `MF_MT_FRAME_RATE = 60/1` regardless of what was
being negotiated, and a hardware MFT sizes its surface pool and picks its
internal path from that. A decoder told 60 and then fed 120 is a live candidate
for the original blank screen. That is now fixed, and it has not yet been run at
4K120.

---

## 0b. What actually happened when it was run (2026-09-08)

`xbox\build-ffmpeg.ps1` automates all of section 1 and 2. It was written from
the plan below and then **failed four times**, each for a different reason, and
every one of them reported something other than its cause. They are recorded
here because the script now handles all four and the next person to touch it
should know why the code looks the way it does.

| # | What it said | What it was |
|---|---|---|
| 1 | `git : Cloning into 'C:\src\ffmpeg'...` reported as a **terminating error** | PS 5.1 turns a native command's stderr into a terminating error under `ErrorActionPreference = "Stop"`. git writes progress to stderr, so a *successful* clone killed the script and quoted its own success message as the failure. Same family as the standing "never run deploy.ps1 with `2>&1`" rule. Fixed with `Invoke-Native`, which judges by `$LASTEXITCODE` alone. |
| 2 | `Host compiler lacks C11 support` | Not about MSVC. `--enable-cross-compile` is mandatory here (configure normally *runs* its test programs, and an APPCONTAINER binary will not execute outside a container), and cross mode splits the compiler in two: `cl` builds the target, and a **host** compiler builds small tools that run during the build. That one defaults to gcc, which MSYS does not ship unless asked. `pacman -S gcc`. |
| 3 | `Cannot find path 'C:\src\ffmpeg\build-uwp\bin'` | An MSVC build leaves the shared libraries in the build tree — `libavcodec\avcodec-61.dll` — and `make install` with a Windows `--prefix` through an MSYS shell puts them somewhere else entirely. Stage from the build tree; it is unambiguous and always there. |
| 4 | Built cleanly, **and failed all three acceptance checks** | The important one. See below. |

### Failure 4 is why the acceptance checks exist

The fourth build compiled, linked, installed, and produced DLLs that would have
packaged without complaint. `dumpbin` said:

```
avcodec-61.dll  APPCONTAINER **MISSING**
avcodec-61.dll  imports LoadLibraryExW
avutil-59.dll   dependencies: dxgi.dll d3d11.dll d3d12.dll bcrypt.dll KERNEL32.dll
```

No `WindowsApp.dll`, no app CRT, and the `LoadLibrary` this whole exercise
exists to avoid. **Two causes, both invisible at build time:**

1. **`vcvars64.bat` is the wrong environment.** It sets `LIB` to `...\lib\x64`
   and links `vcruntime140.dll`; `vcvarsall.bat x64 uwp` sets it to
   `...\lib\x64\store` and links `vcruntime140_app.dll`. The desktop toolchain
   also has no reason to set the APPCONTAINER bit. The script now checks `LIB`
   for `\store` and says so.
2. **Quoting was eaten crossing PowerShell → `bash -lc`.** `--extra-cflags="a b
   c"` loses its inner quotes somewhere in the middle, and configure does not
   error — it takes the first token and treats the rest as stray arguments. The
   evidence was in `config.log`: `-DWINAPI_FAMILY=WINAPI_FAMILY_APP` arrived
   correctly while `-D_WIN32_WINNT=0x0A00` and `-MD` silently did not, and
   `-APPCONTAINER` never reached the linker at all. The configure line is now
   written to a **shell script file** with LF endings and executed, so there is
   exactly one level of quoting and no interpreter in between.

**The lesson worth keeping**: a UWP FFmpeg that is subtly a desktop FFmpeg
builds, links and packages perfectly. Nothing before `dumpbin` would have
caught it, and the first symptom would have been a decoder that fails to
initialise inside the app container on a console, three layers away from the
cause.

---

## 1. What the build container is missing today

Measured on this box, 2026-09-08:

| | State | Needed for FFmpeg |
|---|---|---|
| MSVC | VS 18 Community + BuildTools, 14.44 / 14.51 | ✅ already there |
| Windows SDK | 10.0.26100.0 | ✅ |
| UWP Store toolset | `v143` under the `v170` tree | ✅ |
| `dumpbin` | `…/14.44.35207/bin/HostX64/x64` | ✅ — this is the verification instrument |
| POSIX shell + `make` | **absent.** Git Bash is MSYS-flavoured but ships no build tools | ❌ FFmpeg's `configure` needs both |
| `nasm` | **absent** | ⚠️ avoidable — see below |
| `nuget` / `vcpkg` | **absent.** Dependencies here are vendored by hand-extracting a `.nupkg` into `xbox\packages\` | — FFmpeg follows the same pattern: built once, committed as binaries |

**`nasm` is avoidable.** `--disable-x86asm` costs nothing on this path: the
decode is on the GPU, and the only CPU-side work left is bitstream parsing,
which is C. Skip it unless a software fallback ever becomes interesting.

**`make` is not avoidable.** The one install this needs is MSYS2:

```powershell
winget install MSYS2.MSYS2
C:\msys64\usr\bin\pacman -S --noconfirm make diffutils
```

Then configure from an **x64 Native Tools** prompt with the MSVC environment
inherited into the MSYS shell:

```
set MSYS2_PATH_TYPE=inherit
C:\msys64\msys2_shell.cmd -defterm -no-start -use-full-path
```

Without `-use-full-path` / `MSYS2_PATH_TYPE=inherit`, `cl.exe` is not on PATH
inside the shell and `configure` reports "C compiler test failed", which reads
like a broken toolchain rather than a missing PATH.

---

## 2. The configure line

```sh
./configure \
  --toolchain=msvc --target-os=win32 --arch=x86_64 --enable-cross-compile \
  --enable-shared --disable-static \
  --disable-programs --disable-doc --disable-avdevice --disable-avformat \
  --disable-swresample --disable-swscale --disable-postproc --disable-avfilter \
  --disable-network --disable-x86asm \
  --disable-everything \
  --enable-decoder=hevc --enable-parser=hevc \
  --enable-hwaccel=hevc_d3d11va --enable-hwaccel=hevc_d3d11va2 \
  --enable-d3d11va \
  --extra-cflags="-DWINAPI_FAMILY=WINAPI_FAMILY_APP -D_WIN32_WINNT=0x0A00 -MD" \
  --extra-ldflags="-APPCONTAINER WindowsApp.lib" \
  --prefix=$PWD/build-uwp
```

Line by line, the ones that are load-bearing:

- **`--disable-everything` then re-enable exactly two things.** We decode one
  codec on one hardware path. Everything else is package size and attack
  surface. Expect ~2–4 MB of `avcodec` rather than ~30.
- **`--disable-avformat`.** There is no container. `echo_fill_buffer` hands us
  whole Annex-B access units, already reassembled, FEC-repaired and decrypted by
  `echo-client`. Feeding them to `avcodec_send_packet` needs no demuxer.
- **`--disable-swscale`.** The renderer already converts NV12 → RGB in a pixel
  shader on the GPU. A CPU scaler on this path would be a bug.
- **`-DWINAPI_FAMILY=WINAPI_FAMILY_APP`** is what makes `configure` define
  `HAVE_UWP`, which is what makes `hwcontext_d3d11va.c` link `D3D11CreateDevice`
  directly instead of reaching it through `LoadLibrary("d3d11.dll")`. **That
  substitution is the whole reason a desktop FFmpeg build cannot be dropped in**
  — `LoadLibrary` of a system DLL from an app container is exactly the kind of
  call the container exists to refuse.
- **`-MD`** is the dynamic CRT, which under the Store toolset resolves to
  `vcruntime140_app.dll`. Verified in §3, never assumed.
- **`-APPCONTAINER WindowsApp.lib`** sets the PE flag the loader checks and links
  the UWP umbrella import library.

---

## 3. Verify the binaries BEFORE writing any C++

This is `build-bridge.ps1 -Imports` applied to somebody else's build, and it is
the same three questions §3 answered for the Rust bridge. All three are
answerable at the desk, with no console:

```powershell
$dumpbin = "C:\Program Files\Microsoft Visual Studio\18\Community\VC\Tools\MSVC\14.44.35207\bin\HostX64\x64\dumpbin.exe"

& $dumpbin /headers  avcodec-62.dll | Select-String "App Container"
& $dumpbin /dependents avcodec-62.dll
& $dumpbin /imports  avcodec-62.dll | Select-String -Pattern "LoadLibrary|GetModuleHandle|SetDllDirectory"
```

| Check | Pass | Fail means |
|---|---|---|
| `App Container` in DLL characteristics | present | `-APPCONTAINER` did not reach the link |
| Dependents | `vcruntime140_app.dll`, `msvcp140_app.dll`, `api-ms-win-*` | **`vcruntime140.dll` is the one result that invalidates the design** — two CRTs, two heaps, in one process. Same rule as §3. |
| Imports | no `LoadLibrary*`, no `SetDllDirectory` | `HAVE_UWP` was not defined; the configure `--extra-cflags` did not take |

A build that fails any of these is thrown away, not worked around.

---

## 4. Packaging

Identical treatment to `echo_xbox.dll`, and for the identical reason:

```xml
<None Include="ffmpeg\avcodec-62.dll">
  <DeploymentContent>true</DeploymentContent>
  <Link>avcodec-62.dll</Link>       <!-- package ROOT, not a subfolder -->
</None>
```

**The `<Link>` is not optional.** The Windows loader resolves an implicitly
linked DLL from the application directory and does **not** search subfolders.
Without it the package builds, deploys, and dies at launch with a
missing-dependency error that names the app rather than the DLL — which cost a
session already on the Rust bridge. `build-app.ps1` verifies `echo_xbox.dll` sits
at the root for exactly this reason; extend that check to cover the new DLLs.

Import libs go beside the bridge's:
`<AdditionalDependencies>avcodec.lib;avutil.lib;…`

---

## 5. The decoder

New `FfmpegHevcDecoder`, with the **same public shape** as `HevcDecoder`
(`Initialize / Submit / TryGetFrame / Shutdown`, plus the counters). Then
`VideoRenderer`'s frame source and all of `MainPage` are untouched.

Five things that decide whether this is worth having:

1. **Share the renderer's D3D11 device — do not let FFmpeg create one.**
   `av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_D3D11VA)`, set
   `AVD3D11VADeviceContext::device` to our device (AddRef it), then
   `av_hwdevice_ctx_init`. A device of FFmpeg's own means a cross-device copy on
   every frame, at 4K120, forever. `SetMultithreadProtected(TRUE)` is already on
   the device and FFmpeg requires it.
2. **`get_format` must return `AV_PIX_FMT_D3D11`.** The default callback picks
   the software format and the decode silently lands on the CPU — the same
   failure mode as MF's out-of-order `SET_D3D_MANAGER`, with the same symptom:
   keeps up at 1080p, drowns at 4K.
3. **The output maps 1:1 onto `TryGetFrame`.** `AVFrame::data[0]` is an
   `ID3D11Texture2D*` and `data[1]` is the array index — which is exactly the
   `(texture, subresource)` pair the renderer already takes from the MFT, whose
   output is also a texture array. This is why the renderer needs no change.
4. **Keep §4b.3's drop policy.** Loop `avcodec_receive_frame` until
   `AVERROR(EAGAIN)`, keep the **newest**, discard the rest. The bug that policy
   fixed — a backlog that defends itself and can never shrink — is a property of
   the queue, not of Media Foundation, and it will come straight back otherwise.
5. **Latency:** `AV_CODEC_FLAG_LOW_DELAY`, `thread_count = 1` (threads are
   meaningless with a hwaccel and cost latency), `AV_PKT_FLAG_KEY` from
   `meta[1]` bit 0, and timestamps from `meta[2]` exactly as now.

## 6. Keep both, and let the overlay choose

`HevcDecoder` is **not** deleted. A runtime switch — MF or FFmpeg — in the
overlay costs almost nothing and buys two things worth more than the tidiness:
a one-button A/B on the console for latency and decode counters, and a known-good
4K60 path if D3D11VA disappoints. Given how much of §4b was paid for in blank
screens, the fallback earns its keep.

---

## What is needed before step 1

1. **The `enumerated` / `max rate` / `dxva` / `verdict` block from the console.**
   This decides the gate in §0 and nothing should be installed before it.
2. **Which console.** Series X and Series S differ in VCN configuration, and a
   UWP *app* also runs in a smaller memory and GPU partition than a title —
   a second candidate ceiling that FFmpeg would not lift either.
3. **Permission to install MSYS2** on the build box (§1). It is the only new
   machine-level dependency in the whole plan.
