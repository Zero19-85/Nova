; ═══════════════════════════════════════════════════════════════════════════════
;  Nova Game Streaming — Inno Setup installer
;  Place this file at the project root (alongside Cargo.toml).
;
;  Build machine pre-requisites:
;    1. cargo build --release          → produces target\release\nova-server.exe
;                                         and target\release\nova_shim.dll
;    2. Copy C:\VDD.Control.25.7.23\  → <project root>\VirtualDisplayDriver\
;       (rename the folder, keep the internal structure intact)
;    3. Open Inno Setup Compiler, load this file, press Compile.
;
;  Resulting installer: Output\NovaSetup-<version>.exe
; ═══════════════════════════════════════════════════════════════════════════════

; ── Preprocessor constants ────────────────────────────────────────────────────
#define AppName    "Nova Game Streaming"
#define AppVersion "0.1.0"
#define AppExe     "nova-server.exe"
#define AppDll     "nova_shim.dll"

; VDD constants — adjust the INF sub-path if a future VDD release restructures
; the package. devcon.exe always lives in Dependencies\ regardless of version.
#define VDDDevcon  "{app}\VirtualDisplayDriver\Dependencies\devcon.exe"
#define VDDInfX64  "{app}\VirtualDisplayDriver\SignedDrivers\x86\VDD\MttVDD.inf"
#define VDDInfARM  "{app}\VirtualDisplayDriver\SignedDrivers\ARM64\VDD\MttVDD.inf"
#define VDDWorkX64 "{app}\VirtualDisplayDriver\SignedDrivers\x86\VDD"
#define VDDWorkARM "{app}\VirtualDisplayDriver\SignedDrivers\ARM64\VDD"
#define VDDHwId    "Root\MttVDD"


; ── [Setup] ───────────────────────────────────────────────────────────────────
[Setup]
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Nova Project
DefaultDirName={autopf}\Nova
DefaultGroupName=Nova
UninstallDisplayIcon={app}\{#AppExe}
OutputBaseFilename=NovaSetup-{#AppVersion}
OutputDir=Output

; Compression: LZMA2 solid — best ratio for the ~4 MB driver package.
Compression=lzma2/ultra64
SolidCompression=yes

; The installer must run as Administrator so devcon.exe can modify the
; device tree without triggering a UAC child-process escalation.
PrivilegesRequired=admin

; Allow the user to escalate to admin if they accidentally ran without it.
PrivilegesRequiredOverridesAllowed=dialog

; Target: 64-bit Windows 10 1803+ (minimum for Windows Graphics Capture).
; The installer itself is compiled as x86 by Inno Setup but sets the
; 64-bit registry/file view for all operations via this flag.
ArchitecturesInstallIn64BitMode=x64compatible

; Windows 10 1803 = build 17134. WGC requires 1803; bsdtar for VDD extraction
; also arrived in 1803. Do not lower this without testing both.
MinVersion=10.0.17134

; Write an install log to %TEMP% — helpful when diagnosing a failed devcon run.
SetupLogging=yes


; ── [Languages] ──────────────────────────────────────────────────────────────
[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"


; ── [Files] ───────────────────────────────────────────────────────────────────
[Files]
; ── Nova binaries ─────────────────────────────────────────────────────────────
; {#SourcePath} resolves to the directory containing this .iss file (project root).
Source: "{#SourcePath}target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourcePath}target\release\{#AppDll}";  DestDir: "{app}"; Flags: ignoreversion

; ── Virtual Display Driver package ────────────────────────────────────────────
; Source: the pre-extracted VDD.Control.25.7.23 package renamed to
; VirtualDisplayDriver\ at the project root (see build pre-requisites above).
;
; DestDir: {app}\VirtualDisplayDriver\  — Nova's ensure_installed() searches
; this tree recursively (up to 6 levels) for MttVDD.inf and devcon.exe at
; runtime, so the original VDD package layout is preserved as-is.
;
; Excludes: the VDD GUI tool and its PDB are dev-only and add ~4 MB for nothing.
;
; CRITICAL: devcon.exe install (in [Run] below) runs from this deployed tree
; at install time, so every file devcon needs — MttVDD.dll, mttvdd.cat — must
; land in DestDir before [Run] fires. Inno Setup copies [Files] before [Run],
; so this ordering is guaranteed.
Source: "{#SourcePath}VirtualDisplayDriver\*"; \
    DestDir: "{app}\VirtualDisplayDriver"; \
    Flags: ignoreversion recursesubdirs createallsubdirs; \
    Excludes: "VDD Control.exe,VDD Control.pdb"


; ── [Icons] ───────────────────────────────────────────────────────────────────
; Nova is headless (system tray only) — no Start menu shortcut needed.
[Icons]


; ── [Run] — installation steps in strict order ────────────────────────────────
;
;  Order matters:
;   1. devcon install  — register Root\MttVDD in the device tree
;   2. nova --install  — register the ONLOGON scheduled task
;   3. nova (start)    — launch the server for this session
;
;  Why devcon runs HERE instead of inside Nova:
;   When Nova launches via a scheduled task with the runhidden flag, Windows
;   may suppress child-process UAC elevation even when the parent token is
;   already elevated — the child's escalation request has no desktop to
;   display a prompt on, so it silently fails. The Inno Setup process holds
;   a genuine interactive-admin token and can pass it to child processes
;   directly: devcon.exe inherits it, installs the driver, and exits —
;   no UAC dialog, no silent failure.
[Run]

; ── 1. Install the Virtual Display Driver (MttVDD) ────────────────────────────
; devcon.exe install <INF path> <hardware ID>
;
; WorkingDir MUST be the directory that contains MttVDD.inf so that Windows
; PnP can resolve the co-installer (MttVDD.dll) and catalog (mttvdd.cat)
; relative to the INF — even though the INF path itself is absolute.
;
; x64 (Intel/AMD — covers all NVENC-capable machines):
Filename: "{#VDDDevcon}"; \
    Parameters: "install ""{#VDDInfX64}"" {#VDDHwId}"; \
    WorkingDir: "{#VDDWorkX64}"; \
    Flags: runhidden waituntilterminated; \
    StatusMsg: "Installing Virtual Display Driver..."; \
    Check: not IsARM64

; ARM64 (Surface Pro X, Snapdragon laptops — no NVENC, provided for completeness):
Filename: "{#VDDDevcon}"; \
    Parameters: "install ""{#VDDInfARM}"" {#VDDHwId}"; \
    WorkingDir: "{#VDDWorkARM}"; \
    Flags: runhidden waituntilterminated; \
    StatusMsg: "Installing Virtual Display Driver (ARM64)..."; \
    Check: IsARM64

; ── 2. Register the Nova ONLOGON scheduled task ───────────────────────────────
; --install also runs the Ghost Protocol (removes any stale nova_shim.dll from
; System32/SysWOW64) and retires any old NovaServer SCM service left over from
; a previous install strategy.
Filename: "{app}\{#AppExe}"; \
    Parameters: "--install"; \
    Flags: runhidden waituntilterminated; \
    StatusMsg: "Registering Nova startup task..."

; ── 3. Launch Nova for this session ───────────────────────────────────────────
; nowait so the installer exits immediately; runhidden because Nova is a
; tray-resident process with no console window.
; postinstall + skipifsilent shows a "Launch Nova now" checkbox on the finish
; page but skips it during silent (/SILENT or /VERYSILENT) deployments.
Filename: "{app}\{#AppExe}"; \
    Flags: nowait runhidden postinstall skipifsilent; \
    Description: "Launch {#AppName} now"


; ── [UninstallRun] ────────────────────────────────────────────────────────────
[UninstallRun]

; 1. Stop Nova and remove the scheduled task.
Filename: "{app}\{#AppExe}"; \
    Parameters: "--uninstall"; \
    Flags: runhidden waituntilterminated; \
    RunOnceId: "NovaUninstall"

; 2. Force-kill any remaining Nova process (belt-and-suspenders).
Filename: "{sys}\taskkill.exe"; \
    Parameters: "/F /IM {#AppExe}"; \
    Flags: runhidden waituntilterminated; \
    RunOnceId: "NovaKill"

; 3. Remove the Root\MttVDD device node.
;    devcon "remove" is idempotent: exits 0 if the device is gone already.
;    Only remove on x64; ARM64 uninstall mirrors the same pattern.
Filename: "{#VDDDevcon}"; \
    Parameters: "remove {#VDDHwId}"; \
    WorkingDir: "{#VDDWorkX64}"; \
    Flags: runhidden waituntilterminated; \
    RunOnceId: "VDDRemove"; \
    Check: not IsARM64

Filename: "{#VDDDevcon}"; \
    Parameters: "remove {#VDDHwId}"; \
    WorkingDir: "{#VDDWorkARM}"; \
    Flags: runhidden waituntilterminated; \
    RunOnceId: "VDDRemoveARM"; \
    Check: IsARM64
