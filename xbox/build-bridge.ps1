# build-bridge.ps1 — build the Rust half of the Xbox client and stage it for the
# UWP project.
#
# Run this before opening (or building) EchoXbox.sln, and again after any change
# to echo-xbox, echo-client, or nova-core. The .vcxproj fails with a sentence
# rather than a wall of link errors if you forget.
#
#   .\build-bridge.ps1              # release, the default
#   .\build-bridge.ps1 -DebugBuild  # debug symbols, no optimisation
#   .\build-bridge.ps1 -Imports     # ...then list what the DLL imports
#
# ── The one flag that matters ────────────────────────────────────────────────
#
# `-C target-feature=+crt-static` links the C runtime INTO echo_xbox.dll. A UWP
# app links the Store CRT (vcruntime140_app.dll) instead, and the two must not
# meet: with a dynamic CRT this DLL would drag vcruntime140.dll into the package
# beside its _app twin, giving the process two heaps with allocations crossing
# between them. Static linking removes the question. Verified 2026-09-06 — the
# built DLL imports no vcruntime at all, only kernel32/ntdll/ws2_32/advapi32/
# bcrypt, which is the whole reason this approach was chosen over a staticlib.
#
# Note this REPLACES the workspace's .cargo/config.toml rustflags rather than
# adding to them. That is correct here: the flag it drops is the NVENC library
# path, which only nova-server needs.

[CmdletBinding()]
param(
    [switch]$DebugBuild,
    [switch]$Imports
)

$ErrorActionPreference = "Stop"

$RepoRoot   = Split-Path -Parent $PSScriptRoot
$Profile    = if ($DebugBuild) { "debug" } else { "release" }
$TargetDir  = Join-Path $RepoRoot "target\$Profile"
$StageDir   = Join-Path $PSScriptRoot "EchoXbox\bridge"

Write-Host "=== Echo Xbox bridge ===" -ForegroundColor Cyan
Write-Host "repo    : $RepoRoot"
Write-Host "profile : $Profile"

# ── Build ────────────────────────────────────────────────────────────────────
$env:RUSTFLAGS = "-C target-feature=+crt-static"

$cargoArgs = @("build", "-p", "echo-xbox", "--target", "x86_64-pc-windows-msvc")
if (-not $DebugBuild) { $cargoArgs += "--release" }

Write-Host "`ncargo $($cargoArgs -join ' ')" -ForegroundColor DarkGray
Push-Location $RepoRoot
try {
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
    Remove-Item Env:\RUSTFLAGS -ErrorAction SilentlyContinue
}

# An explicit --target puts artifacts under target\<triple>\<profile>, not
# target\<profile>. Check both so the script works either way.
$candidates = @(
    (Join-Path $RepoRoot "target\x86_64-pc-windows-msvc\$Profile"),
    $TargetDir
)
$built = $null
foreach ($dir in $candidates) {
    if (Test-Path (Join-Path $dir "echo_xbox.dll")) { $built = $dir; break }
}
if (-not $built) {
    throw "echo_xbox.dll was not produced. Looked in:`n  $($candidates -join "`n  ")"
}

# ── Stage ────────────────────────────────────────────────────────────────────
# The .vcxproj links against bridge\echo_xbox.dll.lib and packages
# bridge\echo_xbox.dll as DeploymentContent. Copying rather than referencing
# target\ directly keeps the package contents explicit — a stale DLL inside an
# .appx is otherwise invisible until it misbehaves on the console.
New-Item -ItemType Directory -Force -Path $StageDir | Out-Null

$artifacts = @("echo_xbox.dll", "echo_xbox.dll.lib")
if (Test-Path (Join-Path $built "echo_xbox.pdb")) { $artifacts += "echo_xbox.pdb" }

foreach ($name in $artifacts) {
    $src = Join-Path $built $name
    if (-not (Test-Path $src)) { throw "missing build artifact: $src" }
    Copy-Item $src (Join-Path $StageDir $name) -Force
    $size = [math]::Round((Get-Item $src).Length / 1MB, 2)
    Write-Host ("  staged {0,-22} {1,6} MB" -f $name, $size) -ForegroundColor Green
}

Write-Host "`nstaged into: $StageDir"

# ── Optional: what does it import? ───────────────────────────────────────────
# Worth re-running after a dependency bump. A new import is how a crate that
# works on the desktop starts failing inside the app container, and the import
# table is the only place that shows up before the console does.
if ($Imports) {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    $dumpbin = $null
    if (Test-Path $vswhere) {
        $vs = & $vswhere -latest -products * -property installationPath
        if ($vs) {
            $dumpbin = Get-ChildItem "$vs\VC\Tools\MSVC" -Recurse -Filter dumpbin.exe -ErrorAction SilentlyContinue |
                       Where-Object { $_.FullName -like "*HostX64\x64*" } |
                       Select-Object -First 1 -ExpandProperty FullName
        }
    }
    if (-not $dumpbin) {
        $dumpbin = Get-ChildItem "C:\Program Files (x86)\Microsoft Visual Studio" -Recurse -Filter dumpbin.exe -ErrorAction SilentlyContinue |
                   Where-Object { $_.FullName -like "*HostX64\x64*" } |
                   Select-Object -First 1 -ExpandProperty FullName
    }
    if ($dumpbin) {
        Write-Host "`n--- imports ---" -ForegroundColor Cyan
        & $dumpbin /DEPENDENTS (Join-Path $StageDir "echo_xbox.dll") |
            Select-String -Pattern "^\s{4}\S+\.dll" |
            ForEach-Object { "  " + $_.Line.Trim() }
        Write-Host "`nAny vcruntime140.dll or msvcp140.dll here means crt-static did not apply." -ForegroundColor Yellow
    } else {
        Write-Host "`ndumpbin not found — skipping the import listing." -ForegroundColor Yellow
    }
}

Write-Host "`nNext: open xbox\EchoXbox.sln, set the target to 'Remote Machine' with the" -ForegroundColor Cyan
Write-Host "console's Device Portal address, and deploy." -ForegroundColor Cyan
