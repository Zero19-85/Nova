<#
    build-opus.ps1 -- libopus as a static library for the Xbox app.

        .\build-opus.ps1              # fetch if missing, build, verify, stage
        .\build-opus.ps1 -VerifyOnly  # just re-run the acceptance checks
        .\build-opus.ps1 -Clean       # rebuild every object from scratch

    Produces xbox\EchoXbox\opus\opus.lib plus its public headers in
    xbox\EchoXbox\opus\include\. MicCapture.cpp links it to encode the
    microphone (HANDOFF_ECHO_XBOX.md section 16).

    -- Why libopus, and why a static library --------------------------------

    Windows ships an Opus DECODER and no ENCODER, the FFmpeg build has only
    the decoder, and FFmpeg's native Opus encoder is experimental. libopus is
    the reference encoder and the one the host decodes against (audiopus wraps
    the same library), so the two ends run one codec.

    Static, linked into EchoXbox.exe, because the exe already IS a Store-CRT
    (/MD, vcruntime140_app) binary: objects compiled /MD under the uwp
    environment join it with no second CRT and no new DLL in the package.
    Building it into the Rust bridge instead would have meant a C dependency
    under +crt-static, which is the cmake-vs-crt-static fight the workspace
    manifest already records against aws-lc-rs.

    -- Why cl.exe directly, not CMake ----------------------------------------

    This box has no CMake, and libopus needs nothing CMake provides here: its
    source lists live in *_sources.mk, which this script reads, so a libopus
    upgrade picks up added or removed files without anyone editing a list.
    Same approach build.rs takes for the host's nova_shim.dll.

    Float build, C paths only. x86 SIMD (OPUS_X86_MAY_HAVE_SSE4_1 + RTCD) is
    left out deliberately: a 48 kHz mono voice encode at 24 kbps costs well
    under a millisecond per 20 ms frame in plain C on the console's Zen 2, and
    run-time CPU dispatch is one more thing to be wrong about inside an app
    container for no audible gain.

    -- The acceptance check is the point -------------------------------------

    The library's embedded /DEFAULTLIB directives must name MSVCRT (the /MD
    CRT, which the uwp LIB path resolves to vcruntime140_app) and NEVER
    LIBCMT (/MT -- a second, static CRT and heap inside the exe). A build that
    fails it is thrown away, not worked around. Same rule as build-ffmpeg.ps1.
#>

[CmdletBinding()]
param(
    [string]$SourceDir = "C:\src\opus",
    [string]$Tag       = "v1.5.2",
    [switch]$VerifyOnly,
    [switch]$Clean
)

$ErrorActionPreference = "Stop"

function Step($text) { Write-Host "`n=== $text ===" -ForegroundColor Cyan }
function Info($text) { Write-Host "  $text" -ForegroundColor DarkGray }
function Good($text) { Write-Host "  $text" -ForegroundColor Green }
function Warn($text) { Write-Host "  $text" -ForegroundColor Yellow }

# Native tools write progress to stderr; under PS 5.1 with Stop that becomes a
# terminating error even on success. Verdict from $LASTEXITCODE only -- the
# same reasoning as Invoke-Native in build-ffmpeg.ps1.
function Invoke-Native {
    param([Parameter(Mandatory)][scriptblock]$Command,
          [Parameter(Mandatory)][string]$What)
    $prev = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $out = & $Command 2>&1 | ForEach-Object { "$_" }
    } finally {
        $ErrorActionPreference = $prev
    }
    if ($LASTEXITCODE -ne 0) {
        $out | Select-Object -Last 40 | ForEach-Object { Write-Host "    $_" }
        throw "$What failed (exit code $LASTEXITCODE)"
    }
}

$ProjDir  = Join-Path $PSScriptRoot "EchoXbox"
$StageDir = Join-Path $ProjDir "opus"
$StageLib = Join-Path $StageDir "opus.lib"
$ObjRoot  = Join-Path $SourceDir "build-xbox"

# -- the uwp MSVC environment ---------------------------------------------
#
# `vcvarsall x64 uwp`, NOT vcvars64: it points LIB at ...\lib\x64\store, which
# is what makes the /MD objects resolve to vcruntime140_app at the app's link.
function Import-UwpVcVars {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    $vs = & $vswhere -latest -products * -property installationPath
    $vcvars = Join-Path $vs "VC\Auxiliary\Build\vcvarsall.bat"
    if (-not (Test-Path $vcvars)) { throw "vcvarsall.bat not found under $vs" }
    cmd /c "`"$vcvars`" x64 uwp >nul 2>&1 && set" | ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') {
            Set-Item -Path "Env:$($matches[1])" -Value $matches[2] -ErrorAction SilentlyContinue
        }
    }
    if (-not $env:VCToolsInstallDir) { throw "vcvarsall.bat x64 uwp did not take" }
    Good "cl.exe from $($env:VCToolsInstallDir)"
}

function Test-Acceptance {
    param([string]$Lib)
    $ok = $true

    $directives = & dumpbin /nologo /directives $Lib 2>&1 | Out-String
    if ($directives -match "LIBCMT") {
        Warn "opus.lib asks for LIBCMT (the static /MT CRT) - a second heap in the exe. Rejected."
        $ok = $false
    } elseif ($directives -match "MSVCRT") {
        Good "opus.lib  /MD CRT only (MSVCRT -> vcruntime140_app under the store LIB)"
    } else {
        Warn "opus.lib  no /DEFAULTLIB directive found - inspect by hand"
        $ok = $false
    }

    $symbols = & dumpbin /nologo /linkermember:1 $Lib 2>&1 | Out-String
    foreach ($sym in @("opus_encoder_create", "opus_encode_float", "opus_encoder_ctl", "opus_encoder_destroy")) {
        if ($symbols -notmatch "\b$sym\b") {
            Warn "opus.lib  missing $sym"
            $ok = $false
        }
    }
    if ($ok) { Good "opus.lib  exports the encoder entry points MicCapture uses" }
    return $ok
}

# A make-style list out of libopus's *_sources.mk: `NAME = \` then one path per
# line, each continued with a trailing backslash until the last.
function Get-MkList {
    param([string]$File, [string]$Name)
    $lines = Get-Content (Join-Path $SourceDir $File)
    $start = ($lines | Select-String -Pattern "^$Name\s*=" | Select-Object -First 1)
    if (-not $start) { throw "$Name not found in $File" }
    $files = @()
    for ($i = $start.LineNumber; $i -lt $lines.Count; $i++) {
        $line = $lines[$i].Trim()
        if (-not $line) { break }
        $path = $line.TrimEnd('\').Trim()
        if ($path) { $files += $path }
        if (-not $line.EndsWith('\')) { break }
    }
    return $files
}

Import-UwpVcVars

if ($VerifyOnly) {
    if (-not (Test-Path $StageLib)) { throw "$StageLib does not exist - build it first" }
    if (-not (Test-Acceptance $StageLib)) { throw "acceptance failed" }
    return
}

# -- 1. source -------------------------------------------------------------
Step "libopus $Tag"
if (-not (Test-Path (Join-Path $SourceDir "opus_sources.mk"))) {
    Invoke-Native { git clone --depth 1 --branch $Tag https://github.com/xiph/opus.git $SourceDir } "git clone"
}
Good "source at $SourceDir"

# -- 2. compile ------------------------------------------------------------
Step "compile"
if ($Clean -and (Test-Path $ObjRoot)) { Remove-Item -Recurse -Force $ObjRoot }
New-Item -ItemType Directory -Force $ObjRoot | Out-Null

$sources = @()
$sources += Get-MkList "celt_sources.mk" "CELT_SOURCES"
$sources += Get-MkList "silk_sources.mk" "SILK_SOURCES"
$sources += Get-MkList "silk_sources.mk" "SILK_SOURCES_FLOAT"
$sources += Get-MkList "opus_sources.mk" "OPUS_SOURCES"
$sources += Get-MkList "opus_sources.mk" "OPUS_SOURCES_FLOAT"
Info "$($sources.Count) translation units"

# A response file, because PS 5.1 mangles embedded quotes on a native command
# line and PACKAGE_VERSION needs them.
$version = $Tag.TrimStart('v')
$rsp = Join-Path $ObjRoot "cl.rsp"
@(
    "/nologo", "/c", "/O2", "/MD", "/GS", "/Gy", "/Gw", "/W1", "/utf-8",
    "/DWINAPI_FAMILY=WINAPI_FAMILY_APP", "/D_WIN32_WINNT=0x0A00",
    "/DOPUS_BUILD", "/DUSE_ALLOCA", "/DHAVE_LRINT", "/DHAVE_LRINTF",
    "/D_CRT_SECURE_NO_WARNINGS",
    "/DPACKAGE_VERSION=\`"$version\`"",
    "/I`"$SourceDir\include`"", "/I`"$SourceDir\celt`"", "/I`"$SourceDir\silk`"",
    "/I`"$SourceDir\silk\float`"", "/I`"$SourceDir\src`""
) | Set-Content -Encoding ASCII $rsp

# One cl /MP per source directory, each into its own object directory: libopus
# reuses basenames across directories, and one flat /Fo would let a later
# object overwrite an earlier one without a word.
$objects = @()
foreach ($group in ($sources | Group-Object { Split-Path $_ -Parent })) {
    $objDir = Join-Path $ObjRoot ($group.Name -replace '[\\/]', '_')
    New-Item -ItemType Directory -Force $objDir | Out-Null
    $files = $group.Group | ForEach-Object { Join-Path $SourceDir $_ }
    Invoke-Native { cl "@$rsp" /MP "/Fo$objDir\" $files } "cl ($($group.Name))"
    $objects += $group.Group | ForEach-Object {
        Join-Path $objDir ([IO.Path]::GetFileNameWithoutExtension($_) + ".obj")
    }
    Info "$($group.Name): $($group.Count) files"
}

# -- 3. archive ------------------------------------------------------------
Step "archive"
$built = Join-Path $ObjRoot "opus.lib"
Invoke-Native { lib /nologo "/OUT:$built" $objects } "lib"
Good "$built ($([math]::Round((Get-Item $built).Length / 1KB)) KB)"

# -- 4. verify, THEN stage --------------------------------------------------
Step "acceptance"
if (-not (Test-Acceptance $built)) { throw "acceptance failed - not staging" }

Step "stage"
New-Item -ItemType Directory -Force (Join-Path $StageDir "include") | Out-Null
Copy-Item $built $StageLib -Force
Get-ChildItem (Join-Path $SourceDir "include") -Filter *.h |
    Copy-Item -Destination (Join-Path $StageDir "include") -Force
Good "staged into $StageDir"
