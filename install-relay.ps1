# install-relay.ps1 - run by Nova.iss to set up the co-located Echo relay.
#
#   install-relay.ps1 -AppDir <dir>              # register, start, wire nova.toml
#   install-relay.ps1 -AppDir <dir> -Uninstall   # stop and unregister
#
# Why this exists: the relay used to be set up by hand, so a full uninstall +
# reinstall left NovaEchoRelay pointing at an exe that no longer existed and a
# fresh nova.toml with `[echo.signaling] url = ""` - Echo then reported "no
# relay" with nothing in the installer to explain it (2026-10-05).
#
# The relay generates its own identity (relay-data\relay.cert.der) on first
# run. The installer deliberately does NOT ship one: a private key inside a
# distributable is a key every copy shares. relay-data is runtime state, so an
# uninstall leaves it in place and a reinstall keeps the same pin - which is
# what lets already-configured Echo clients reconnect without re-entering it.
#
# nova.toml is only ever FILLED IN, never overridden: an operator who pointed
# `url` somewhere else keeps their setting.

param(
    [Parameter(Mandatory)] [string]$AppDir,
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"
$TaskName = "NovaEchoRelay"
$Log      = Join-Path $AppDir "install-relay.log"
function Say($m) { Add-Content -Path $Log -Value ("{0:s}  {1}" -f (Get-Date), $m) -Encoding UTF8 }

if ($Uninstall) {
    try { Stop-ScheduledTask -TaskName $TaskName -ErrorAction Stop } catch {}
    try { Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction Stop } catch {}
    Get-Process nova-relay -ErrorAction SilentlyContinue | Stop-Process -Force
    exit 0
}

Say "--- relay setup in $AppDir"

# 1. The task. Mirrors the hand-made original: SYSTEM, at boot, no time limit
#    (the default is 72 h, which would silently kill the relay three days in),
#    restart on failure. Output goes to nova-relay.log beside the other logs.
$exe    = Join-Path $AppDir "nova-relay.exe"
$relLog = Join-Path $AppDir "nova-relay.log"
$action = New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\cmd.exe" `
    -Argument "/c `"chcp 65001 >nul & `"$exe`" --data-dir relay-data >> `"$relLog`" 2>&1`"" `
    -WorkingDirectory $AppDir
$trigger   = New-ScheduledTaskTrigger -AtStartup
$principal = New-ScheduledTaskPrincipal -UserId "SYSTEM" -LogonType ServiceAccount -RunLevel Highest
$settings  = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger `
    -Principal $principal -Settings $settings -Force | Out-Null
Start-ScheduledTask -TaskName $TaskName
Say "task registered and started"

# 2. The relay's identity, which it writes on first run.
$cert = Join-Path $AppDir "relay-data\relay.cert.der"
for ($i = 0; $i -lt 30 -and -not (Test-Path $cert); $i++) { Start-Sleep -Milliseconds 500 }
if (-not (Test-Path $cert)) { Say "relay wrote no certificate in 15 s - see nova-relay.log"; exit 0 }
$pin = (Get-FileHash $cert -Algorithm SHA256).Hash.ToLower()
Say "relay pin $pin"

# 3. nova.toml, which the SERVICE writes on its first start - so wait for it.
$toml = Join-Path $AppDir "nova.toml"
for ($i = 0; $i -lt 60 -and -not (Test-Path $toml); $i++) { Start-Sleep -Milliseconds 500 }
if (-not (Test-Path $toml)) { Say "nova.toml never appeared - relay not wired"; exit 0 }

$text = [IO.File]::ReadAllText($toml)
$orig = $text
# Loopback, not the LAN or public address: `url` is how the HOST reaches its
# own relay. What clients are told is `advertise_url` / UPnP / the LAN rewrite.
$text = $text -replace '(?m)^url(\s*)= ""', 'url$1= "https://127.0.0.1:8443/v1/signal"'
$text = $text -replace '(?m)^relay_cert_sha256(\s*)= ""', ('relay_cert_sha256$1= "' + $pin + '"')
# A pin is only re-synced when `url` is THIS machine's relay: if relay-data was
# deleted, the relay minted a new identity and the old pin would refuse it.
if ($text -match '(?m)^url\s*=\s*"https://127\.0\.0\.1:8443/') {
    $text = $text -replace '(?m)^relay_cert_sha256(\s*)= "[0-9a-fA-F]{64}"', ('relay_cert_sha256$1= "' + $pin + '"')
}
if ($text -ne $orig) {
    [IO.File]::WriteAllText($toml, $text, (New-Object Text.UTF8Encoding $false))
    Say "nova.toml wired to the local relay - restarting NovaService"
    Restart-Service NovaService
} else {
    Say "nova.toml already has a relay configured - left alone"
}
exit 0
