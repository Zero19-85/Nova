# deploy.ps1 - Nova hot-patch deployment
#
# Copies the freshly built release artifacts into the live service directory and
# restarts NovaService. Both files ship together, always: nova_shim.dll carries
# the NVENC pipeline and nova-server.exe links against its exports, so deploying
# one without the other is how you get a mismatch that presents as a capture or
# encoder fault.
#
# Usage (ELEVATED PowerShell, from the repo root):
#   .\deploy.ps1                # build, deploy, restart, print the checklist
#   .\deploy.ps1 -SkipBuild     # deploy whatever is already in target\release
#   .\deploy.ps1 -Watch         # ...then follow nova.log for the new markers
#   .\deploy.ps1 -Rollback      # restore the most recent backup and restart
#
# Nova is three processes from one binary (Master service / Worker / input
# helper). Stopping the service brings all of them down; the script waits for
# that to actually happen before copying, because a running Worker holds a lock
# on both files.

param(
    [switch]$SkipBuild,
    [switch]$Watch,
    [switch]$Rollback
)

$ErrorActionPreference = "Stop"

$ServiceName = "NovaService"
$InstallDir  = "C:\Program Files\Nova Server"
$SrcExe      = "target\release\nova-server.exe"
$SrcDll      = "target\release\nova_shim.dll"
$DstExe      = Join-Path $InstallDir "nova-server.exe"
$DstDll      = Join-Path $InstallDir "nova_shim.dll"
$Stamp       = Get-Date -Format "yyyyMMdd-HHmmss"

function Write-Step($msg)  { Write-Host "  $msg" }
function Write-Ok($msg)    { Write-Host "  [OK] $msg"   -ForegroundColor Green }
function Write-Warn2($msg) { Write-Host "  [WARN] $msg" -ForegroundColor Yellow }
function Write-Bad($msg)   { Write-Host "  [FAIL] $msg" -ForegroundColor Red }

Write-Host ""
Write-Host "=== Nova hot-patch deploy ===" -ForegroundColor Cyan

# -- Elevation ---------------------------------------------------------------
# Copying into Program Files and driving the SCM both need it, and failing here
# with a clear message beats failing halfway with a locked file.
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$pr = New-Object Security.Principal.WindowsPrincipal($id)
if (-not $pr.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Bad "Not elevated. Re-run this script from an Administrator PowerShell."
    exit 1
}
Write-Ok "Elevated"

if (-not (Test-Path $InstallDir)) {
    Write-Bad "Install directory not found: $InstallDir"
    exit 1
}

# -- Stop the service --------------------------------------------------------
function Stop-Nova {
    $svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if ($null -eq $svc) {
        Write-Warn2 "Service '$ServiceName' is not installed - copying anyway"
    } elseif ($svc.Status -ne "Stopped") {
        Write-Step "Stopping $ServiceName (graceful teardown restores the display)..."
        try {
            Stop-Service -Name $ServiceName -Force -ErrorAction Stop
        } catch {
            Write-Warn2 "Stop-Service reported: $($_.Exception.Message)"
        }
        try {
            (Get-Service $ServiceName).WaitForStatus("Stopped", (New-TimeSpan -Seconds 30))
        } catch {
            Write-Warn2 "Service did not report Stopped within 30s"
        }
    } else {
        Write-Step "Service already stopped"
    }

    # The Master gets HOST_GRACEFUL_EXIT_MS to tear the Worker down. Wait for the
    # processes themselves, not just the SCM status - the SCM reports Stopped
    # before every child has unwound, and a surviving Worker holds the file lock.
    $deadline = (Get-Date).AddSeconds(20)
    while ((Get-Date) -lt $deadline) {
        $procs = @(Get-Process -Name "nova-server" -ErrorAction SilentlyContinue)
        if ($procs.Count -eq 0) { break }
        Start-Sleep -Milliseconds 250
    }
    $procs = @(Get-Process -Name "nova-server" -ErrorAction SilentlyContinue)
    if ($procs.Count -gt 0) {
        Write-Warn2 "$($procs.Count) nova-server process(es) still alive - force-terminating"
        $procs | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 1
    }
    Write-Ok "All nova-server processes down"
}

# -- Rollback ----------------------------------------------------------------
if ($Rollback) {
    $backups = @(Get-ChildItem -Path $InstallDir -Filter "nova-server.exe.hotpatch-*" |
                 Sort-Object LastWriteTime -Descending)
    if ($backups.Count -eq 0) {
        Write-Bad "No hot-patch backup found in $InstallDir"
        exit 1
    }
    $b      = $backups[0]
    $bStamp = $b.Name -replace "^nova-server\.exe\.hotpatch-", ""
    $bDll   = Join-Path $InstallDir "nova_shim.dll.hotpatch-$bStamp"
    Write-Step "Rolling back to $($b.Name)"
    if (-not (Test-Path $bDll)) {
        Write-Bad "Matching DLL backup missing: $bDll - refusing a half rollback"
        exit 1
    }
    Stop-Nova
    Copy-Item $b.FullName $DstExe -Force
    Copy-Item $bDll       $DstDll -Force
    Write-Ok "Restored exe + dll from $bStamp"
    Start-Service -Name $ServiceName
    Write-Ok "$ServiceName started"
    exit 0
}

# -- Build -------------------------------------------------------------------
if (-not $SkipBuild) {
    Write-Step "cargo build --release -p nova-server"
    cargo build --release -p nova-server
    if ($LASTEXITCODE -ne 0) {
        Write-Bad "Build failed - nothing deployed"
        exit 1
    }
    Write-Ok "Build succeeded"
}

foreach ($f in @($SrcExe, $SrcDll)) {
    if (-not (Test-Path $f)) {
        Write-Bad "Missing build artifact: $f"
        exit 1
    }
}

# The DLL is built by build.rs via cl.exe. If shim.cpp is newer than the DLL then
# the C++ half did NOT rebuild, and you would deploy new Rust against an old shim
# - exactly the mismatch this project has been bitten by before.
$shimSrc = "nova-server\shim\shim.cpp"
if (Test-Path $shimSrc) {
    $srcT = (Get-Item $shimSrc).LastWriteTime
    $dllT = (Get-Item $SrcDll).LastWriteTime
    if ($srcT -gt $dllT) {
        Write-Bad "shim.cpp ($srcT) is NEWER than nova_shim.dll ($dllT) - the shim did not rebuild."
        Write-Bad "Run a clean build before deploying. Aborting."
        exit 1
    }
    Write-Ok "nova_shim.dll is newer than shim.cpp"
}

Write-Step "Deploying:"
Write-Step "  exe $((Get-Item $SrcExe).LastWriteTime)  ->  $DstExe"
Write-Step "  dll $((Get-Item $SrcDll).LastWriteTime)  ->  $DstDll"

Stop-Nova

# -- Backup ------------------------------------------------------------------
# Both halves under one shared stamp, so -Rollback can never restore a pair it
# did not actually save together.
if (Test-Path $DstExe) {
    Copy-Item $DstExe (Join-Path $InstallDir "nova-server.exe.hotpatch-$Stamp") -Force
}
if (Test-Path $DstDll) {
    Copy-Item $DstDll (Join-Path $InstallDir "nova_shim.dll.hotpatch-$Stamp") -Force
}
Write-Ok "Backed up current install as *.hotpatch-$Stamp"

# -- Copy + verify -----------------------------------------------------------
Copy-Item $SrcExe $DstExe -Force
Copy-Item $SrcDll $DstDll -Force

$okExe = (Get-FileHash $SrcExe).Hash -eq (Get-FileHash $DstExe).Hash
$okDll = (Get-FileHash $SrcDll).Hash -eq (Get-FileHash $DstDll).Hash
if (-not ($okExe -and $okDll)) {
    Write-Bad "Hash mismatch after copy - deployment is NOT trustworthy. Run -Rollback."
    exit 1
}
Write-Ok "Copied and hash-verified (exe + dll)"

# -- Start -------------------------------------------------------------------
Start-Service -Name $ServiceName
try {
    (Get-Service $ServiceName).WaitForStatus("Running", (New-TimeSpan -Seconds 30))
} catch {
    Write-Bad "Service did not reach Running within 30s - check nova-service.log"
    exit 1
}
Write-Ok "$ServiceName is Running"

$logPath = Join-Path $InstallDir "nova.log"

Write-Host ""
Write-Host "=== Deployed. What to look for ===" -ForegroundColor Cyan
Write-Host "  These markers are written when an ENCODER is created, which happens on"
Write-Host "  the next stream start - not at service start. Start an AV1 stream, then:"
Write-Host ""
Write-Host "    Select-String -Path '$logPath' -Pattern '\[IR\]|\[AV1\]|\[X\]'"
Write-Host ""
Write-Host "  Expect, once per session:"
Write-Host "    [IR]  Intra refresh for av1: cap=YES, requested=ON (period=300, sweep=299)"
Write-Host "    [AV1] tiles: numTileColumns=.. numTileRows=.. customTileConfig=0 at WxH"
Write-Host ""
Write-Host "  Watch for, and hope never to see:"
Write-Host "    [IR]  WARNING: intra refresh requested but UNSUPPORTED for av1"
Write-Host "    [X]   EncodeFrame TRUNCATED frame N ..."
Write-Host ""
Write-Host "  And the loss half of the diagnosis, in the same log:"
Write-Host "    Select-String -Path '$logPath' -Pattern 'socket buffer full'"

if ($Watch) {
    Write-Host ""
    Write-Host "Following nova.log (Ctrl+C to stop)..." -ForegroundColor Cyan
    Get-Content $logPath -Tail 0 -Wait |
        Select-String -Pattern '\[IR\]|\[AV1\]|\[X\]|NVENC READY|socket buffer full|requested a keyframe'
}
