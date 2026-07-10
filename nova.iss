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


; ── [Tasks] — optional, user-selectable install choices ───────────────────────
;
; Secure-desktop UAC choice.
;   By default Windows draws UAC elevation prompts on the SECURE desktop
;   (WinSta0\Winlogon), which Nova's primary WGC capture backend cannot see —
;   during a UAC prompt a remote operator gets a black screen at the exact
;   moment they need to click "Yes". Nova's full fix is the DDA secure-desktop
;   backend, but many remote-admin users (as with RDP / AnyDesk / TeamViewer)
;   simply prefer to move the prompt onto the normal desktop so it streams like
;   anything else.
;
;   This task is UNCHECKED by default — it is a deliberate security trade-off
;   (the secure desktop defeats UAC-spoofing malware) and must be opted into.
;   The [Registry] entry below is written only when this task is selected, and
;   the value is removed on uninstall (Windows then reverts to its default =
;   secure desktop on), so the choice is fully reversible.
[Tasks]
Name: "disablesecuredesktop"; \
    Description: "Show UAC prompts on the normal desktop (lets Nova stream elevation prompts without a capture-backend switch)"; \
    GroupDescription: "Remote administration"; \
    Flags: unchecked


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


; ── [Registry] ────────────────────────────────────────────────────────────────
;
; PromptOnSecureDesktop = 0  — moves UAC prompts to the normal desktop so WGC can
; capture them. Written ONLY when the "disablesecuredesktop" task is selected.
;
; Flags:
;   uninsdeletevalue  — remove the value on uninstall. With the value gone,
;                       Windows uses its built-in default (secure desktop ON),
;                       so uninstalling cleanly restores stock UAC behaviour.
;
; This is HKLM machine policy, which is why the installer requires admin
; (PrivilegesRequired=admin above). No reboot is needed; the change takes effect
; at the next UAC prompt.
[Registry]
Root: HKLM; \
    Subkey: "SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System"; \
    ValueType: dword; \
    ValueName: "PromptOnSecureDesktop"; \
    ValueData: "0"; \
    Tasks: disablesecuredesktop; \
    Flags: uninsdeletevalue


; ── [Icons] ───────────────────────────────────────────────────────────────────
; Nova is headless (system tray only) — no Start menu shortcut needed.
[Icons]


; ── [Run] — installation steps in strict order ────────────────────────────────
;
;  Order matters:
;   1. devcon install        — register Root\MttVDD in the device tree
;   2. nova --install-service — register the NovaService SYSTEM launcher
;   3. sc start NovaService  — start it now (spawns the host for this session)
;
;  Deployment model (Phase 15.2c): Nova now ships as a LocalSystem launcher
;  SERVICE ("NovaService") that spawns the interactive host into the active
;  console session with a SYSTEM-derived elevated token. This is what gives the
;  host the privilege to attach a capture thread to the secure desktop
;  (WinSta0\Winlogon) for DDA capture of UAC / logon screens — a plain
;  scheduled task cannot provide that token.
;
;  `nova --install-service` removes the legacy NovaServerBoot scheduled task as
;  part of registration, so the two models never both spawn a host. The task
;  path (`nova --install`) is retained in the binary as a DOCUMENTED FALLBACK
;  for environments where a service is undesirable (run it manually instead of
;  step 2/3), but it does NOT provide secure-desktop capture.
;
;  Why devcon runs HERE instead of inside Nova:
;   The Inno Setup process holds a genuine interactive-admin token and can pass
;   it to child processes directly: devcon.exe inherits it, installs the driver,
;   and exits — no UAC dialog, no silent failure.
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

; ── 2. Register the NovaService SYSTEM launcher ──────────────────────────────
; --install-service performs, in one shot:
;   a) Ghost Protocol — removes stale nova_shim.dll from System32/SysWOW64
;   b) Task removal   — deletes NovaServerBoot + legacy task names (the service
;                       and the task must never both spawn a host)
;   c) Service create — registers "NovaService" as LocalSystem, AUTO_START,
;                       binary path `"<exe>" --service`. If it already exists
;                       (upgrade), the binary path is updated in place.
; Runs with the installer's admin token (CreateServiceW needs admin).
; NOTE: any previously-running NovaService/host was already stopped in the
; [Code] PrepareToInstall hook below, BEFORE [Files] overwrote the exe/dll.
Filename: "{app}\{#AppExe}"; \
    Parameters: "--install-service"; \
    Flags: runhidden waituntilterminated; \
    StatusMsg: "Registering NovaService launcher..."

; ── 3. Start the service now (spawns the host for this session) ───────────────
; sc start returns as soon as NovaService reaches START_PENDING; the service's
; worker then spawns the host into the active console session (this installer's
; session) with the elevated user token — exercising the exact production path
; rather than a one-off direct launch. waituntilterminated waits only for the
; short-lived sc.exe, not for Nova. Not postinstall: this must run with the
; installer's admin token (sc start needs admin), and postinstall entries are
; de-elevated by Inno.
Filename: "{sys}\sc.exe"; \
    Parameters: "start NovaService"; \
    Flags: runhidden waituntilterminated; \
    StatusMsg: "Starting Nova..."


; ── [UninstallRun] ────────────────────────────────────────────────────────────
[UninstallRun]

; 1. Stop and remove the NovaService launcher (stops the service, which
;    terminates the host it manages, then deletes the service). Idempotent.
Filename: "{app}\{#AppExe}"; \
    Parameters: "--uninstall-service"; \
    Flags: runhidden waituntilterminated; \
    RunOnceId: "NovaServiceUninstall"

; 2. Remove the legacy scheduled task too (belt-and-suspenders — covers boxes
;    upgraded from a task-based install, or ones that used the task fallback).
Filename: "{app}\{#AppExe}"; \
    Parameters: "--uninstall"; \
    Flags: runhidden waituntilterminated; \
    RunOnceId: "NovaUninstall"

; 3. Force-kill any remaining Nova process (belt-and-suspenders).
Filename: "{sys}\taskkill.exe"; \
    Parameters: "/F /IM {#AppExe}"; \
    Flags: runhidden waituntilterminated; \
    RunOnceId: "NovaKill"

; 4. Remove the Root\MttVDD device node.
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


; ── [Code] ────────────────────────────────────────────────────────────────────
[Code]

// PrepareToInstall runs AFTER the wizard pages but BEFORE any file is copied —
// the correct place to release locks on the files [Files] is about to
// overwrite. On an upgrade the running NovaService holds nova-server.exe /
// nova_shim.dll open; without stopping it first, the copy fails (or Inno shows
// the "files in use / reboot required" prompt). We stop the service (which
// terminates the host it manages) and belt-and-suspenders kill any stray host
// and stop a legacy task-based instance too.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  // Stop the launcher service if present (idempotent — sc returns non-zero when
  // the service doesn't exist or is already stopped; we ignore the code).
  Exec(ExpandConstant('{sys}\sc.exe'), 'stop NovaService', '',
    SW_HIDE, ewWaitUntilTerminated, ResultCode);

  // Legacy task-based install: end the task so its host lets go of the files.
  Exec(ExpandConstant('{sys}\schtasks.exe'), '/end /tn NovaServerBoot', '',
    SW_HIDE, ewWaitUntilTerminated, ResultCode);

  // Belt-and-suspenders: terminate any remaining host process.
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM nova-server.exe', '',
    SW_HIDE, ewWaitUntilTerminated, ResultCode);

  // Give the SCM/OS a moment to release the file handles before [Files] copies.
  Sleep(1500);

  Result := '';  // empty = proceed with installation
end;
