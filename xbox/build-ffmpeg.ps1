<#
    build-ffmpeg.ps1 — an FFmpeg for the Xbox app container.

        .\build-ffmpeg.ps1              # install what is missing, build, verify
        .\build-ffmpeg.ps1 -VerifyOnly  # just re-run the acceptance checks
        .\build-ffmpeg.ps1 -Clean       # reconfigure from scratch

    Produces avcodec/avutil DLLs that decode HEVC through D3D11VA and nothing
    else, built against the Store CRT, linked /APPCONTAINER, and staged into
    xbox\EchoXbox\ffmpeg\ ready for the vcxproj to package.

    ── Why this exists as a script rather than a wiki page ────────────────────

    Every step below has a failure mode that reads as something other than what
    it is, and three of them cost an afternoon each if met cold:

      * MSYS2 ships /usr/bin/link.exe (coreutils), which SHADOWS MSVC's linker.
        The symptom is configure reporting "C compiler test failed" — which
        reads as a missing or broken compiler, and is neither.

      * MSVC has to be visible INSIDE the MSYS shell. That needs the vcvars
        environment imported here and MSYS2_PATH_TYPE=inherit set before bash
        starts. Without it, same misleading "C compiler test failed".

      * -DWINAPI_FAMILY=WINAPI_FAMILY_APP is what makes configure define
        HAVE_UWP, which is what makes hwcontext_d3d11va.c link D3D11CreateDevice
        DIRECTLY instead of reaching it through LoadLibrary("d3d11.dll"). A
        desktop build looks identical, packages fine, and fails at runtime
        inside the app container where LoadLibrary of a system DLL is refused.

    ── The acceptance checks are the point ───────────────────────────────────

    A build that fails any of the three dumpbin checks at the end is thrown
    away, not worked around. They are the same questions HANDOFF_ECHO_XBOX.md
    §3 answered for the Rust bridge, asked of somebody else's build:

      1. APPCONTAINER bit set in the PE header.
      2. Imports vcruntime140_app.dll, NEVER vcruntime140.dll. Two CRTs in one
         process is two heaps, and a pointer allocated by one and freed by the
         other is a corruption bug that reproduces on a console and nowhere
         else.
      3. No LoadLibrary / SetDllDirectory imports.
#>

[CmdletBinding()]
param(
    [string]$SourceDir = "C:\src\ffmpeg",
    [string]$Tag       = "n7.1",
    [string]$Msys      = "C:\msys64",
    [switch]$VerifyOnly,
    [switch]$Clean
)

$ErrorActionPreference = "Stop"

# ── Native commands and PowerShell 5.1 ─────────────────────────────────────
#
# Every tool this script drives — git, pacman, configure, make — writes
# progress and warnings to stderr as a matter of course. Under PS 5.1 with
# $ErrorActionPreference = "Stop", a native command's stderr becomes a
# TERMINATING error, so `git clone` fails the script while cloning perfectly
# well. The failure even quotes the success message ("Cloning into ...") as the
# error text, which is as misleading as it sounds.
#
# Same family as the standing rule about never running deploy.ps1 with `2>&1`.
# So: cmdlets keep Stop, native commands are run through here, and the verdict
# comes from $LASTEXITCODE, which is the only honest signal a native tool gives.
function Invoke-Native {
    param([Parameter(Mandatory)][scriptblock]$Command,
          [Parameter(Mandatory)][string]$What,
          [switch]$Quiet)

    $prev = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        if ($Quiet) { & $Command 2>&1 | Out-Null } else { & $Command 2>&1 | ForEach-Object { "$_" } }
    } finally {
        $ErrorActionPreference = $prev
    }
    if ($LASTEXITCODE -ne 0) { throw "$What failed (exit code $LASTEXITCODE)" }
}

$ProjDir  = Join-Path $PSScriptRoot "EchoXbox"
$StageDir = Join-Path $ProjDir "ffmpeg"
# No --prefix: the DLLs are staged from the build tree, which is where an
# MSVC build actually leaves them. See the staging step.

function Step($text) { Write-Host "`n=== $text ===" -ForegroundColor Cyan }
function Info($text) { Write-Host "  $text" -ForegroundColor DarkGray }
function Good($text) { Write-Host "  $text" -ForegroundColor Green }
function Warn($text) { Write-Host "  $text" -ForegroundColor Yellow }

# ── dumpbin, which is the verification instrument ──────────────────────────
function Find-Dumpbin {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        $vs = & $vswhere -latest -products * -property installationPath
        if ($vs) {
            $found = Get-ChildItem (Join-Path $vs "VC\Tools\MSVC") -Recurse -Filter dumpbin.exe -ErrorAction SilentlyContinue |
                     Where-Object { $_.FullName -match "HostX64\\x64" } |
                     Select-Object -First 1
            if ($found) { return $found.FullName }
        }
    }
    throw "dumpbin.exe not found. Is the C++ workload installed?"
}

function Test-Acceptance {
    param([string]$Dll, [string]$Dumpbin)

    $name = Split-Path $Dll -Leaf
    $ok = $true

    # 1. APPCONTAINER
    $headers = & $Dumpbin /headers $Dll 2>&1 | Out-String
    if ($headers -match "App Container") {
        Good "$name  APPCONTAINER set"
    } else {
        Warn "$name  APPCONTAINER **MISSING** - '-APPCONTAINER' did not reach the link"
        $ok = $false
    }

    # 2. The CRT. This is the one that invalidates the design.
    $deps = & $Dumpbin /dependents $Dll 2>&1 | Out-String
    if ($deps -match "vcruntime140\.dll|msvcp140\.dll") {
        Warn "$name  links the DESKTOP CRT (vcruntime140.dll) - two heaps in one process. Rejected."
        $ok = $false
    } elseif ($deps -match "vcruntime140_app\.dll|msvcp140_app\.dll|api-ms-win-crt") {
        Good "$name  app CRT only"
    } else {
        Warn "$name  no recognisable CRT import - inspect by hand:"
        Write-Host $deps
    }

    # 3. Container-forbidden calls.
    $imports = & $Dumpbin /imports $Dll 2>&1 | Out-String
    $banned = @("LoadLibraryA", "LoadLibraryW", "LoadLibraryExA", "LoadLibraryExW", "SetDllDirectory")
    $hits = $banned | Where-Object { $imports -match [regex]::Escape($_) }
    if ($hits) {
        Warn "$name  imports $($hits -join ', ') - HAVE_UWP was not defined; the configure cflags did not take"
        $ok = $false
    } else {
        Good "$name  no LoadLibrary / SetDllDirectory"
    }

    return $ok
}

# ══════════════════════════════════════════════════════════════════════════
if (-not $VerifyOnly) {

    # ── 1. MSYS2 ───────────────────────────────────────────────────────────
    Step "MSYS2"
    $bash = Join-Path $Msys "usr\bin\bash.exe"
    if (-not (Test-Path $bash)) {
        Info "not present - installing via winget (this needs elevation)"
        Invoke-Native -What "winget install MSYS2" -Command {
            winget install -e --id MSYS2.MSYS2 --accept-source-agreements --accept-package-agreements
        }
        if (-not (Test-Path $bash)) {
            throw "MSYS2 did not install to $Msys. Install it by hand, or pass -Msys <path>."
        }
    }
    Good "msys2 at $Msys"

    # `gcc` is NOT the compiler that builds FFmpeg — MSVC is. It is the HOST
    # compiler, and the distinction is the third misleading error in this
    # script's collection.
    #
    # `--enable-cross-compile` is mandatory for a UWP build, because configure
    # normally settles questions by compiling a test program AND RUNNING IT,
    # and an /APPCONTAINER binary will not execute outside a container. Cross
    # mode stops it trying. But cross mode also splits the compiler in two: the
    # target compiler (cl, producing the DLLs) and the host compiler (producing
    # small build-time tools that run HERE during the build). The host one
    # defaults to gcc, which MSYS does not ship unless asked.
    #
    # Absent, configure reports "Host compiler lacks C11 support" — which reads
    # as an indictment of MSVC, is not about MSVC at all, and sends you off to
    # add /std:c11 flags that change nothing.
    Info "pacman: make, diffutils, gcc (the HOST compiler - see the note above)"
    Invoke-Native -Quiet -What "pacman" -Command {
        & $bash -lc "pacman -S --needed --noconfirm make diffutils gcc"
    }

    # ── 2. The link.exe shadow ─────────────────────────────────────────────
    #
    # MSYS's coreutils `link` and MSVC's linker have the same name. Whichever
    # is first on PATH wins, and MSYS's own bin is always first inside the
    # shell — so configure runs coreutils' link, gets nonsense, and reports a
    # broken C compiler.
    Step "link.exe shadow"
    $msysLink = Join-Path $Msys "usr\bin\link.exe"
    if (Test-Path $msysLink) {
        Move-Item $msysLink (Join-Path $Msys "usr\bin\link-coreutils.exe") -Force
        Good "moved MSYS link.exe aside - MSVC's linker is now reachable"
    } else {
        Good "already moved aside"
    }

    # ── 3. The MSVC environment, imported HERE so bash inherits it ─────────
    Step "MSVC environment"
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    $vs = & $vswhere -latest -products * -property installationPath
    # `vcvarsall x64 uwp`, NOT `vcvars64`.
    #
    # This is what selects the STORE CRT. The desktop environment points LIB at
    # ...\lib\x64 and links vcruntime140.dll; the uwp environment points it at
    # ...\lib\x64\store and links vcruntime140_app.dll. Getting this wrong
    # produces a DLL that builds, packages and looks fine, and puts a second CRT
    # — a second heap — in the process at runtime, which is the one outcome
    # HANDOFF_ECHO_XBOX.md §3 says invalidates the whole design.
    #
    # It is also half of why the first build had no APPCONTAINER bit: the
    # desktop toolchain has no reason to set it.
    $vcvars = Join-Path $vs "VC\Auxiliary\Build\vcvarsall.bat"
    if (-not (Test-Path $vcvars)) { throw "vcvarsall.bat not found under $vs" }

    # `cmd /c "vcvars && set"` is how the MSVC environment is captured without
    # a native-tools prompt: run it, print the resulting environment, and copy
    # it into this process. bash then inherits it via MSYS2_PATH_TYPE=inherit.
    cmd /c "`"$vcvars`" x64 uwp >nul 2>&1 && set" | ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') {
            Set-Item -Path "Env:$($matches[1])" -Value $matches[2] -ErrorAction SilentlyContinue
        }
    }
    if (-not $env:VCToolsInstallDir) { throw "vcvarsall.bat x64 uwp did not take" }
    Good "cl.exe from $($env:VCToolsInstallDir)"
    if ($env:LIB -match "\\store") {
        Good "LIB points at the store CRT"
    } else {
        Warn "LIB does not mention \store - the UWP workload may be missing; the CRT check will catch it"
    }
    $env:MSYS2_PATH_TYPE = "inherit"

    # ── 4. Source ──────────────────────────────────────────────────────────
    Step "FFmpeg source ($Tag)"
    if (-not (Test-Path (Join-Path $SourceDir "configure"))) {
        # A directory without a `configure` in it is a clone that died partway.
        # Leaving it would make git refuse ("destination path already exists")
        # on every subsequent run.
        if (Test-Path $SourceDir) {
            Info "removing a partial clone"
            Remove-Item $SourceDir -Recurse -Force
        }
        New-Item -ItemType Directory -Force (Split-Path $SourceDir) | Out-Null
        Info "cloning $Tag"
        Invoke-Native -What "git clone" -Command {
            git clone --depth 1 --branch $Tag https://git.ffmpeg.org/ffmpeg.git $SourceDir
        }
    }
    Good "source at $SourceDir"

    $msysSrc = "/" + ($SourceDir -replace ":", "" -replace "\\", "/")   # C:\src -> /c/src
    $msysSrc = $msysSrc.Substring(0, 2).ToLower() + $msysSrc.Substring(2)

    # ── 5. Configure + build ───────────────────────────────────────────────
    # ── The configure line goes through a FILE, not through -c ─────────────
    #
    # Two of this build's three acceptance failures came from quoting. A
    # PowerShell string containing `--extra-cflags="a b c"` handed to
    # `bash -lc "..."` loses the inner quotes somewhere in the middle, and the
    # result is not an error — configure takes the first token and the rest
    # become stray arguments. The visible symptom was
    # -DWINAPI_FAMILY=WINAPI_FAMILY_APP arriving correctly while
    # -D_WIN32_WINNT and -MD silently did not, and -APPCONTAINER never reaching
    # the linker at all. Everything compiled. Everything was wrong.
    #
    # A script file has exactly one level of quoting and no interpreter in
    # between, so what is written here is what configure receives. It is also
    # then re-runnable by hand, which matters when diagnosing config.log.
    #
    # LF endings, deliberately: bash on a CRLF script fails with
    # "$'\r': command not found", pointing at a line that looks perfect.
    Step "configure"
    $configureSh = @'
#!/bin/bash
# Generated by build-ffmpeg.ps1 — edit that, not this.
set -e
./configure \
  --toolchain=msvc \
  --target-os=win32 \
  --arch=x86_64 \
  --enable-cross-compile \
  --enable-shared \
  --disable-static \
  --disable-everything \
  --disable-programs \
  --disable-doc \
  --disable-avdevice \
  --disable-avformat \
  --disable-swresample \
  --disable-swscale \
  --disable-postproc \
  --disable-avfilter \
  --disable-network \
  --disable-x86asm \
  --enable-decoder=hevc \
  --enable-parser=hevc \
  --enable-hwaccel=hevc_d3d11va \
  --enable-hwaccel=hevc_d3d11va2 \
  --enable-d3d11va \
  --extra-cflags="-DWINAPI_FAMILY=WINAPI_FAMILY_APP -D_WIN32_WINNT=0x0A00 -MD" \
  --extra-ldflags="-APPCONTAINER WindowsApp.lib"
'@
    $shPath = Join-Path $SourceDir "uwp-configure.sh"
    [System.IO.File]::WriteAllText($shPath, ($configureSh -replace "`r`n", "`n"),
                                   (New-Object System.Text.UTF8Encoding $false))
    Info "wrote $shPath"

    if ($Clean) {
        $ErrorActionPreference = "Continue"
        & $bash -lc "cd $msysSrc && make distclean" 2>&1 | Out-Null
        $ErrorActionPreference = "Stop"
    }

    Info "this takes a few minutes"
    try {
        Invoke-Native -What "configure" -Command {
            & $bash -lc "cd $msysSrc && chmod +x uwp-configure.sh && ./uwp-configure.sh"
        }
    } catch {
        Write-Host ""
        Warn "configure failed. The tail of $SourceDir\ffbuild\config.log names the real reason;"
        Warn "'C compiler test failed' almost always means MSVC is not visible in the shell"
        Warn "(step 3) or MSYS's link.exe is back (step 2)."
        throw
    }
    Good "configured"

    Step "make"
    Invoke-Native -What "make" -Command {
        & $bash -lc "cd $msysSrc && make -j$([Environment]::ProcessorCount) && make install"
    }
    Good "built"

    # ── 6. Stage into the project ──────────────────────────────────────────
    # Staged from the BUILD TREE, not from --prefix.
    #
    # An MSVC build leaves the shared libraries next to their sources —
    # libavcodec\avcodec-61.dll and libavcodec\avcodec.lib — and `make install`
    # is a headers-and-pkgconfig affair whose prefix handling with a Windows
    # path through an MSYS shell is its own small adventure. The build tree is
    # unambiguous and always present, so take them from there.
    #
    # The VERSIONED dll is the one to ship: `avcodec.lib` is an import library
    # naming `avcodec-61.dll`, and the unversioned copy beside it is not what
    # the loader will be asked for.
    Step "stage"
    New-Item -ItemType Directory -Force $StageDir | Out-Null
    $staged = 0
    foreach ($lib in @("libavcodec", "libavutil", "libswresample", "libswscale", "libavformat")) {
        $dir = Join-Path $SourceDir $lib
        if (-not (Test-Path $dir)) { continue }
        Get-ChildItem $dir -Filter "*-*.dll" -ErrorAction SilentlyContinue | ForEach-Object {
            Copy-Item $_.FullName $StageDir -Force
            Info "$($_.Name)  $([math]::Round($_.Length/1MB,2)) MB"
            $staged++
        }
        Get-ChildItem $dir -Filter "*.lib" -ErrorAction SilentlyContinue | ForEach-Object {
            Copy-Item $_.FullName $StageDir -Force
        }
    }
    if ($staged -eq 0) { throw "no versioned DLLs found under $SourceDir - did make actually link?" }

    # Headers too, so the project builds without a machine-specific include path
    # pointing at wherever somebody happened to clone FFmpeg. `avconfig.h` and
    # `libavcodec/version.h` are GENERATED by configure, so these have to come
    # from this build rather than from a tarball — a mismatched pair compiles
    # and then disagrees about struct layout at runtime, which is the worst
    # class of bug this project can have.
    #
    # Headers only: the source directories also hold .c, .o and .asm files, and
    # copying those would put a hundred megabytes of build tree in the staging
    # directory for no reason.
    $incDir = Join-Path $StageDir "include"
    Remove-Item $incDir -Recurse -Force -ErrorAction SilentlyContinue
    foreach ($lib in @("libavcodec", "libavutil")) {
        $src = Join-Path $SourceDir $lib
        $dst = Join-Path $incDir $lib
        New-Item -ItemType Directory -Force $dst | Out-Null
        Get-ChildItem $src -Filter *.h -File | Copy-Item -Destination $dst -Force
    }
    $headers = (Get-ChildItem $incDir -Recurse -Filter *.h).Count
    Good "staged $staged DLL(s) and $headers header(s) into $StageDir"
}

# ── 7. Acceptance ──────────────────────────────────────────────────────────
Step "acceptance checks"
$dumpbin = Find-Dumpbin
Info "dumpbin: $dumpbin"

$dlls = Get-ChildItem $StageDir -Filter *.dll -ErrorAction SilentlyContinue
if (-not $dlls) { throw "no DLLs in $StageDir - nothing to verify" }

$allOk = $true
foreach ($dll in $dlls) {
    Write-Host ""
    if (-not (Test-Acceptance -Dll $dll.FullName -Dumpbin $dumpbin)) { $allOk = $false }
}

Write-Host ""
if ($allOk) {
    Good "ALL CHECKS PASSED - these binaries are safe to package"
} else {
    Warn "CHECKS FAILED - do not package these. Fix the configure flags and rebuild."
    exit 1
}
