# build-app.ps1 - build the UWP app and produce a signed, sideloadable .msix.
#
# Run build-bridge.ps1 FIRST (this script checks, and the .vcxproj also fails
# with a sentence rather than a wall of link errors).
#
#   .\build-app.ps1                 # signed Release .msix - the deliverable
#   .\build-app.ps1 -DebugBuild     # Debug; packages the debug CRT, bigger
#   .\build-app.ps1 -NewCert        # regenerate the self-signed sideload cert
#
# ---- What this needs installed -------------------------------------------
#
# Visual Studio with the "Universal Windows Platform development" workload AND
# its C++ UWP tools. The decisive artifact is the per-toolset UWP directory:
#
#   MSBuild\Microsoft\VC\<v170|v180>\Application Type\Windows Store\10.0\
#       Platforms\x64\PlatformToolsets\<v143|v145>
#
# If that path is absent the workload is not installed, and the build fails at
# Microsoft.Cpp.Default.props with an unhelpful message about the platform
# toolset. VS 2026 (v18) resolves this project's v143 toolset out of the v170
# tree, which is why the project can stay pinned to v143.
#
# ---- Signing -------------------------------------------------------------
#
# The certificate SUBJECT must equal Package.appxmanifest's Publisher exactly
# ("CN=Nova"). A mismatch is a build error late in packaging that talks about
# the publisher rather than about the certificate.
#
# The cert is self-signed, so the console must be told to trust it: upload the
# .cer alongside the .msix in the Xbox Device Portal. `signtool verify` on this
# box reports "terminated in a root certificate which is not trusted" - that is
# the expected and correct result, not a failure.

[CmdletBinding()]
param(
    [switch]$DebugBuild,
    [switch]$NewCert
)

$ErrorActionPreference = "Stop"

$ProjDir  = Join-Path $PSScriptRoot "EchoXbox"
$Proj     = Join-Path $ProjDir "EchoXbox.vcxproj"
$Config   = if ($DebugBuild) { "Debug" } else { "Release" }
$Pfx      = Join-Path $ProjDir "EchoXbox_TemporaryKey.pfx"
$Cer      = Join-Path $ProjDir "EchoXbox_TemporaryKey.cer"
$CertPass = "NovaEchoDev"   # a self-signed dev key; not a secret worth hiding
$Subject  = "CN=Nova"       # MUST match Package.appxmanifest Publisher

Write-Host "=== Echo Xbox app ===" -ForegroundColor Cyan
Write-Host "config : $Config"

# -- The bridge must exist first ------------------------------------------
if (-not (Test-Path (Join-Path $ProjDir "bridge\echo_xbox.dll.lib"))) {
    throw "The Rust bridge is not staged. Run .\build-bridge.ps1 first."
}

# -- Locate MSBuild --------------------------------------------------------
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$msbuild = $null
if (Test-Path $vswhere) {
    # -products * so Community/Professional/Enterprise and BuildTools all match.
    $msbuild = & $vswhere -latest -products * -requires Microsoft.Component.MSBuild `
                          -find "MSBuild\**\Bin\amd64\MSBuild.exe" | Select-Object -First 1
}
if (-not $msbuild) { throw "MSBuild not found. Is Visual Studio installed?" }
Write-Host "msbuild: $msbuild" -ForegroundColor DarkGray

# -- Signing certificate ---------------------------------------------------
if ($NewCert -and (Test-Path $Pfx)) { Remove-Item $Pfx, $Cer -Force -ErrorAction SilentlyContinue }

if (-not (Test-Path $Pfx)) {
    Write-Host "`ncreating self-signed sideload certificate ($Subject)" -ForegroundColor Yellow
    $cert = New-SelfSignedCertificate -Type Custom -Subject $Subject `
        -KeyUsage DigitalSignature -FriendlyName "Nova Echo Xbox (dev sideload)" `
        -CertStoreLocation "Cert:\CurrentUser\My" `
        -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3", "2.5.29.19={text}Subject Type:End Entity") `
        -NotAfter (Get-Date).AddYears(5)
    $secure = ConvertTo-SecureString -String $CertPass -Force -AsPlainText
    Export-PfxCertificate -Cert "Cert:\CurrentUser\My\$($cert.Thumbprint)" -FilePath $Pfx -Password $secure | Out-Null
    Export-Certificate  -Cert "Cert:\CurrentUser\My\$($cert.Thumbprint)" -FilePath $Cer -Type CERT | Out-Null
    Write-Host "  thumbprint $($cert.Thumbprint)" -ForegroundColor Green
}

# -- Build -----------------------------------------------------------------
# SideloadOnly + AppxBundle=Never: this is not going to the Store, and a bundle
# would only add a wrapper the Device Portal then has to unwrap.
$msbuildArgs = @(
    $Proj,
    "/p:Configuration=$Config",
    "/p:Platform=x64",
    "/p:AppxPackageSigningEnabled=true",
    "/p:PackageCertificateKeyFile=EchoXbox_TemporaryKey.pfx",
    "/p:PackageCertificatePassword=$CertPass",
    "/p:UapAppxPackageBuildMode=SideloadOnly",
    "/p:AppxBundle=Never",
    "/v:minimal",
    "/nologo"
)

Write-Host "`nbuilding..." -ForegroundColor DarkGray
& $msbuild @msbuildArgs
if ($LASTEXITCODE -ne 0) { throw "MSBuild failed with exit code $LASTEXITCODE" }

# -- Report ----------------------------------------------------------------
$pkgRoot = Join-Path $ProjDir "AppPackages\EchoXbox"
$msix = Get-ChildItem $pkgRoot -Recurse -Filter "*.msix" -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $msix) { throw "Build reported success but produced no .msix under $pkgRoot" }

Write-Host "`n=== package ===" -ForegroundColor Cyan
Write-Host ("  {0}" -f $msix.FullName)
Write-Host ("  {0:N2} MB" -f ($msix.Length / 1MB))

# The DLL must be at the package ROOT. It landed in bridge\ on the first build
# because <None Include="bridge\echo_xbox.dll"> preserves its relative path -
# the loader does not search subfolders, so that would have been a launch
# failure on the console rather than a build error here. <Link> fixes it; this
# check is what stops it regressing silently.
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip = [System.IO.Compression.ZipFile]::OpenRead($msix.FullName)
try {
    $names = $zip.Entries | ForEach-Object { $_.FullName }
} finally { $zip.Dispose() }

# Every implicitly-linked DLL, not just the bridge. The loader resolves them
# from the application directory and does not search subfolders, so one in a
# subfolder is a package that builds, deploys, and dies at launch naming the app
# rather than the DLL. FFmpeg joined the list on 2026-09-08.
foreach ($required in @("echo_xbox.dll", "avcodec-61.dll", "avutil-59.dll")) {
    if ($names -contains $required) {
        Write-Host "  $required at package root - OK" -ForegroundColor Green
    } else {
        $where = ($names | Where-Object { $_ -like "*$required" }) -join ", "
        throw "$required is NOT at the package root (found: '$where'). The app will fail to launch. Check the <Link> metadata on its None item in EchoXbox.vcxproj."
    }
}
if ($names -contains "AppxSignature.p7x") {
    Write-Host "  signed - OK" -ForegroundColor Green
} else {
    Write-Host "  NOT SIGNED" -ForegroundColor Yellow
}

# -- Bundle it for the phone ------------------------------------------------
#
# The Device Portal upload form wants three files together, and the practical
# way to reach that form is a phone browser - so the deliverable is one archive
# rather than a directory to go hunting through.
#
# The .cer is REQUIRED, not a nicety: the package is self-signed, so a console
# that has not been given the certificate refuses the install with an error
# that does not mention certificates.
$bundleDir = Join-Path $PSScriptRoot "dist"
New-Item -ItemType Directory -Force -Path $bundleDir | Out-Null
$zipPath = Join-Path $bundleDir ("EchoXbox-sideload-" + $msix.BaseName + ".zip")

# A GUID-named staging directory, and files deleted individually rather than by
# removing a tree: this box's sandbox refuses Remove-Item on directories it
# cannot prove are ours.
$stage = Join-Path $env:TEMP ("echo-bundle-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $stage | Out-Null
try {
    Copy-Item $msix.FullName -Destination $stage

    # The .cer beside the package is the one MSBuild exported for THIS build.
    # Preferring it over the project copy matters after -NewCert: the two
    # differ, and only one of them signed the package being shipped.
    $cerBeside = Join-Path $msix.DirectoryName ($msix.BaseName + ".cer")
    if (Test-Path $cerBeside) {
        Copy-Item $cerBeside -Destination $stage
    } elseif (Test-Path $Cer) {
        Copy-Item $Cer -Destination (Join-Path $stage ($msix.BaseName + ".cer"))
    } else {
        throw "No .cer found to bundle - the console cannot trust the package without one."
    }

    # VCLibs is the Store C++ runtime. The bridge is crt-static and needs none
    # of it, but the C++/WinRT app itself links vcruntime140_app.dll, which
    # lives in this framework package and nowhere else on a console that has
    # never had a sideloaded C++ app.
    $vclibs = Get-ChildItem (Join-Path $msix.DirectoryName "Dependencies") -Recurse `
                            -Filter "Microsoft.VCLibs.x64.14.00.appx" -ErrorAction SilentlyContinue |
              Select-Object -First 1
    if (-not $vclibs) {
        throw "Microsoft.VCLibs.x64.14.00.appx not found under $($msix.DirectoryName)\Dependencies"
    }
    Copy-Item $vclibs.FullName -Destination $stage

    $readme = @"
Echo for Xbox - sideload bundle
===============================

Xbox Device Portal -> Home -> Add, then supply all three of these:

  1. App package  : $($msix.Name)
  2. Certificate  : $($msix.BaseName).cer
  3. Dependency   : Microsoft.VCLibs.x64.14.00.appx

Install straight over an existing copy. LocalState survives an upgrade, so
hosts.json and this console's identity are kept - there is no need to pair
again.

First run: five probe lines, then the host list. Pick a host, press Pair, type
the PIN into Nova on the PC, and the stream starts by itself.

While streaming there is no on-screen UI at all.

  Hold VIEW (the two-rectangles button) ~1s : the overlay - resolution,
                                              frame rate, disconnect,
                                              diagnostics
  MENU + VIEW together                      : controller mouse mode on/off

In mouse mode the right stick drives the PC cursor, the right trigger is a
left click and the left trigger a right click; the game sees no pad at all
until you press the chord again. A short press of VIEW on its own still
reaches the PC.
"@
    Set-Content -Path (Join-Path $stage "README.txt") -Value $readme -Encoding utf8

    if (Test-Path $zipPath) { [System.IO.File]::Delete($zipPath) }
    Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zipPath
} finally {
    Get-ChildItem $stage -File -ErrorAction SilentlyContinue |
        ForEach-Object { [System.IO.File]::Delete($_.FullName) }
    Remove-Item $stage -Force -ErrorAction SilentlyContinue
}

$zipInfo = Get-Item $zipPath
Write-Host "`n=== sideload bundle ===" -ForegroundColor Cyan
Write-Host ("  {0}" -f $zipInfo.FullName)
Write-Host ("  {0:N2} MB - msix + cer + VCLibs + README" -f ($zipInfo.Length / 1MB)) -ForegroundColor Green
Write-Host "`nSend that one file to a phone; upload all three from Device Portal -> Home -> Add." -ForegroundColor Cyan
