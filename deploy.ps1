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
#   .\deploy.ps1 -KeepBackups 5 # keep more (or fewer) backup pairs; default 3
#
# Nova is three processes from one binary (Master service / Worker / input
# helper). Stopping the service brings all of them down; the script waits for
# that to actually happen before copying, because a running Worker holds a lock
# on both files.

param(
    [switch]$SkipBuild,
    [switch]$Watch,
    [switch]$Rollback,
    [ValidateRange(1, 50)]
    [int]$KeepBackups = 3
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

# Backup stamps (yyyyMMdd-HHmmss), newest first. Ordered by the stamp in the
# NAME, never LastWriteTime: Copy-Item preserves the source's timestamp, so a
# backup's LastWriteTime is when that binary was BUILT, not when it was saved.
function Get-BackupStamps {
    @(Get-ChildItem -Path $InstallDir -File |
        ForEach-Object {
            if ($_.Name -match '^(nova-server\.exe|nova_shim\.dll)\.hotpatch-(\d{8}-\d{6})$') { $Matches[2] }
        } |
        Sort-Object -Unique -Descending)
}

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
    $stamps = Get-BackupStamps
    if ($stamps.Count -eq 0) {
        Write-Bad "No hot-patch backup found in $InstallDir"
        exit 1
    }
    $bStamp = $stamps[0]
    $bExe   = Join-Path $InstallDir "nova-server.exe.hotpatch-$bStamp"
    $bDll   = Join-Path $InstallDir "nova_shim.dll.hotpatch-$bStamp"
    Write-Step "Rolling back to the backup taken $bStamp"
    if (-not ((Test-Path $bExe) -and (Test-Path $bDll))) {
        Write-Bad "Backup $bStamp is not a complete exe + dll pair - refusing a half rollback"
        exit 1
    }
    Stop-Nova
    Copy-Item $bExe       $DstExe -Force
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

# -- Prune old backups -------------------------------------------------------
# Only now, with the new pair verified: a deploy that fails above keeps every
# fallback it had. Pruned by stamp, so an exe and its dll always go together.
$stamps = Get-BackupStamps
if ($stamps.Count -gt $KeepBackups) {
    $doomed = $stamps[$KeepBackups..($stamps.Count - 1)]
    foreach ($s in $doomed) {
        foreach ($n in @("nova-server.exe.hotpatch-$s", "nova_shim.dll.hotpatch-$s")) {
            $p = Join-Path $InstallDir $n
            if (Test-Path $p) {
                try { Remove-Item $p -Force -ErrorAction Stop }
                catch { Write-Warn2 "Could not remove old backup $n : $($_.Exception.Message)" }
            }
        }
    }
    Write-Ok "Pruned $($doomed.Count) old backup pair(s); kept the newest $KeepBackups"
}

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
$svcLogPath = Join-Path $InstallDir "nova-service.log"
Write-Host "  [IR] is written when an ENCODER is created - on the next stream start,"
Write-Host "  not at service start. Start a 4K Moonlight stream, then:"
Write-Host ""
Write-Host "    Select-String -Path '$logPath' -Pattern '\[IR\]'"
Write-Host ""
Write-Host "  Expect, for Moonlight (an Echo 4K session still says requested=ON):"
Write-Host "    [IR] ... requested=OFF ... off for this GameStream client"
Write-Host ""
Write-Host "  Keyframe storms (Master log) and the restore that follows (Worker log):"
Write-Host "    Select-String -Path '$svcLogPath' -Pattern 'keyframe storm'"
Write-Host "    Select-String -Path '$logPath' -Pattern 'bitrate restored'"

if ($Watch) {
    Write-Host ""
    Write-Host "Following nova.log (Ctrl+C to stop)..." -ForegroundColor Cyan
    Get-Content $logPath -Tail 0 -Wait |
        Select-String -Pattern '\[IR\]|\[AV1\]|\[X\]|NVENC READY|socket buffer full|requested a keyframe|bitrate restored'
}
