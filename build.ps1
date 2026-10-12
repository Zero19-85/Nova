# build.ps1 - Unified Nova build script
# Usage:
#   .\build.ps1           # release build (default)
#   .\build.ps1 -Debug    # debug build
#   .\build.ps1 -Check    # just check prerequisites, don't build
#
# Runs on Windows PowerShell 5.1 (the inbox shell) as well as PowerShell 7, so
# keep it to 5.1 syntax: no ternary `? :`, no `??`, no `&&`/`||` chains. Keep
# the file pure ASCII too, like deploy.ps1 - 5.1 reads a BOM-less script as
# Windows-1252, where the last byte of a UTF-8 em dash decodes as a curly
# quote and PowerShell treats that as a string delimiter.
param(
    [switch]$Debug,
    [switch]$Check
)

$ErrorActionPreference = "Stop"

# Not $Profile: that is PowerShell's own automatic variable.
$BuildProfile = if ($Debug) { "debug" } else { "release" }
# @( ... ) around the whole if: an if-expression unrolls a one-element array to
# a bare string, and splatting a string hands cargo one character per argument.
$CargoArgs    = @(if (-not $Debug) { "--release" })
$ExePath      = "target\$BuildProfile\nova-server.exe"
$DllPath      = "target\$BuildProfile\nova_shim.dll"
$NvencLib     = "C:\NVSDK\Lib\win\x64\nvencodeapi.lib"

Write-Host ""
Write-Host "=== Nova Build ($BuildProfile) ===" -ForegroundColor Cyan

# -- Prerequisites --------------------------------------------------------------
$ok = $true

if (-not (Get-Command "cargo" -ErrorAction SilentlyContinue)) {
    Write-Host "  [MISSING] cargo - install Rust from https://rustup.rs" -ForegroundColor Red
    $ok = $false
} else {
    Write-Host "  [OK] $(cargo --version)" -ForegroundColor Green
}

if (-not (Test-Path $NvencLib)) {
    Write-Host "  [MISSING] $NvencLib - install the NVIDIA Video Codec SDK" -ForegroundColor Red
    $ok = $false
} else {
    Write-Host "  [OK] NVENC SDK at $NvencLib" -ForegroundColor Green
}

# where.exe exits 1 when cl.exe is not on PATH (the normal case outside a
# Developer prompt); under "Stop" that must not end the script.
$clExe = $null
try { $clExe = & where.exe cl.exe 2>$null | Select-Object -First 1 } catch { }
if (-not $clExe) {
    # Try to find via vswhere
    $vsWhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vsWhere) {
        $vsPath = & $vsWhere -latest -property installationPath 2>$null
        $clExe  = "$vsPath\VC\Tools\MSVC\*\bin\Hostx64\x64\cl.exe" |
                  Resolve-Path -ErrorAction SilentlyContinue |
                  Select-Object -Last 1 -ExpandProperty Path
    }
}
if ($clExe) {
    Write-Host "  [OK] cl.exe at $clExe" -ForegroundColor Green
} else {
    Write-Host "  [MISSING] cl.exe - install Visual Studio C++ Build Tools" -ForegroundColor Red
    $ok = $false
}

if ($Check) {
    Write-Host ""
    if ($ok) {
        Write-Host "All prerequisites met." -ForegroundColor Green
        exit 0
    }
    Write-Host "Prerequisites missing - fix the above before building." -ForegroundColor Red
    exit 1
}

if (-not $ok) {
    Write-Host ""
    Write-Host "Prerequisites not met - aborting." -ForegroundColor Red
    exit 1
}

# -- Build ----------------------------------------------------------------------
Write-Host ""
Write-Host "Running: cargo build $CargoArgs" -ForegroundColor Yellow
Write-Host "(build.rs compiles the C++ shim -> nova_shim.dll and copies it to target\$BuildProfile\)" -ForegroundColor DarkGray
Write-Host ""

$sw = [System.Diagnostics.Stopwatch]::StartNew()

# cargo reports progress on stderr. Under "Stop", Windows PowerShell 5.1 turns
# a native command's stderr into terminating errors, so relax it for the build
# and judge success by the exit code alone.
$ErrorActionPreference = "Continue"
cargo build @CargoArgs
$exitCode = $LASTEXITCODE
$ErrorActionPreference = "Stop"

$sw.Stop()

# -- Results --------------------------------------------------------------------
Write-Host ""
Write-Host "=== Build Output ($([math]::Round($sw.Elapsed.TotalSeconds, 1))s) ===" -ForegroundColor Cyan

if ($exitCode -ne 0) {
    Write-Host "  BUILD FAILED (exit $exitCode)" -ForegroundColor Red
    exit $exitCode
}

foreach ($path in @($ExePath, $DllPath)) {
    if (Test-Path $path) {
        $size = (Get-Item $path).Length
        Write-Host ("  [OK]  {0,-45} {1,7:N0} KB" -f $path, ($size / 1KB)) -ForegroundColor Green
    } else {
        Write-Host "  [!!]  $path  - NOT FOUND" -ForegroundColor Red
        $exitCode = 1
    }
}

Write-Host ""
if ($exitCode -eq 0) {
    Write-Host "Build succeeded. Deploy both files together:" -ForegroundColor Green
    Write-Host "  $ExePath" -ForegroundColor White
    Write-Host "  $DllPath" -ForegroundColor White
} else {
    Write-Host "Build finished with warnings - check output above." -ForegroundColor Yellow
}

exit $exitCode
