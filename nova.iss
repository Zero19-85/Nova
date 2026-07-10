; Nova Server Installer
; Updated for service-based deployment (Phase 15)
; Place this file in the root of your project (next to Cargo.toml)

#define MyAppName "Nova Server"
#define MyAppVersion "0.1.0 Alpha"
#define MyAppPublisher "Zero"
#define MyAppURL "Origin1985.com"
#define MyAppExeName "nova-server.exe"

[Setup]
AppId={{73D98A91-6B50-4367-9514-D155DB9DF4D6}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={autopf}\{#MyAppName}
UninstallDisplayIcon={app}\{#MyAppExeName}
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
DisableProgramGroupPage=yes
OutputBaseFilename=Nova_Alpha_Setup
SolidCompression=yes
WizardStyle=modern dynamic

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; Main binaries (relative to .iss location)
Source: "target\release\nova-server.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "target\release\nova_shim.dll"; DestDir: "{app}"; Flags: ignoreversion

; Virtual Display Driver files (adjust this path if your VDD folder is elsewhere)
Source: "VirtualDisplayDriver\*"; DestDir: "{app}\VirtualDisplayDriver"; Flags: ignoreversion recursesubdirs createallsubdirs; Excludes: "*.pdb,VDD Control.exe,*.iss"

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
; 1. Register NovaService (removes any old scheduled task first)
Filename: "{app}\{#MyAppExeName}"; \
    Parameters: "--install-service"; \
    Flags: runhidden waituntilterminated

; 2. Start the service
Filename: "{sys}\sc.exe"; Parameters: "start NovaService"; \
    Flags: runhidden waituntilterminated

[UninstallRun]
; Stop and remove the service first
Filename: "{app}\{#MyAppExeName}"; Parameters: "--uninstall-service"; \
    Flags: runhidden waituntilterminated

; Belt-and-suspenders: also remove old scheduled task
Filename: "{app}\{#MyAppExeName}"; Parameters: "--uninstall"; \
    Flags: runhidden waituntilterminated

; Force kill anything still running
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#MyAppExeName}"; \
    Flags: runhidden waituntilterminated

[Code]
procedure CurStepChanged(CurStep: TSetupStep);
var
  ResultCode: Integer;
begin
  if CurStep = ssInstall then
  begin
    // Stop the service before copying new files (prevents "file in use" on upgrade)
    Exec(ExpandConstant('{app}\nova-server.exe'), '--uninstall-service', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    
    // Kill any stray host process
    Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM nova-server.exe', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    
    Sleep(1500);
  end;
end;