; Inno Setup script for Reverything. Build with scripts\build-installer.ps1, which compiles the
; binaries first and passes AppVersion and BinDir. Needs Inno Setup 6
; (https://jrsoftware.org/isinfo.php).
;
; Installing needs administrator rights once, to register the index service. The search window
; then runs as the normal user. Uninstalling removes everything the app created.

#define AppName "Reverything"
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef BinDir
  #define BinDir "..\target\release"
#endif
#define AppExe "reverything.exe"
#define ServiceExe "reverything-service.exe"
#define RunKey "Software\Microsoft\Windows\CurrentVersion\Run"

[Setup]
AppId={{6B0E2F47-9C1D-4E4B-A6E8-3F2C8D9B5A71}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=tth05
AppPublisherURL=https://github.com/tth05/reverything
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\target\installer
OutputBaseFilename=reverything-setup-{#AppVersion}
SetupIconFile=..\assets\reverything.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
WizardStyle=modern
Compression=lzma2/ultra64
SolidCompression=yes
; The service and the window are stopped by the code below, not by the restart manager
CloseApplications=no
; The per-user parts (settings, UI log, autostart) belong to the user who runs the setup, which
; is the user of the app on a normal single user machine
UsedUserAreasWarning=no

[Tasks]
Name: autostart; Description: "Start Reverything when I log on (in the tray)"
Name: desktopicon; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "{#BinDir}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\{#ServiceExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
; Same value the app's own "Start with Windows" setting uses
Root: HKCU; Subkey: "{#RunKey}"; ValueType: string; ValueName: "{#AppName}"; \
    ValueData: """{app}\{#AppExe}"" --background"; Flags: uninsdeletevalue; Tasks: autostart
; Installed by a package manager (/MANAGED=winget or /MANAGED=scoop), which then also does the
; updates: the app does not check for them itself
Root: HKLM; Subkey: "Software\{#AppName}"; ValueType: string; ValueName: "ManagedBy"; \
    ValueData: "{param:MANAGED}"; Flags: uninsdeletekey; Check: IsManaged
Root: HKLM; Subkey: "Software\{#AppName}"; ValueType: none; ValueName: "ManagedBy"; \
    Flags: deletevalue; Check: not IsManaged

[Run]
; Registers (or updates) and starts the service
Filename: "{app}\{#ServiceExe}"; Parameters: "install"; Flags: runhidden waituntilterminated; \
    StatusMsg: "Installing the index service..."
Filename: "{app}\{#AppExe}"; Description: "Start {#AppName}"; \
    Flags: nowait postinstall skipifsilent runasoriginaluser
; Silent installs are updates started by the app, which closed the app first
Filename: "{app}\{#AppExe}"; Flags: nowait runasoriginaluser; Check: WizardSilent

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#AppExe}"; Flags: runhidden; RunOnceId: "StopApp"
Filename: "{app}\{#ServiceExe}"; Parameters: "uninstall"; Flags: runhidden waituntilterminated; \
    RunOnceId: "RemoveService"

[UninstallDelete]
; Saved index and service log
Type: filesandordirs; Name: "{commonappdata}\Reverything"
; settings.json
Type: filesandordirs; Name: "{userappdata}\Reverything"
; ui.log
Type: filesandordirs; Name: "{localappdata}\Reverything"

[Code]
function IsManaged: Boolean;
begin
  Result := ExpandConstant('{param:MANAGED}') <> '';
end;

procedure StopRunning();
var
  ResultCode: Integer;
begin
  // The window and the service keep their exe files open, stop both before replacing them.
  // Stopping the service also saves the index, so an upgrade does not need a full rescan.
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExe}', '', SW_HIDE,
    ewWaitUntilTerminated, ResultCode);
  if FileExists(ExpandConstant('{app}\{#ServiceExe}')) then
    Exec(ExpandConstant('{app}\{#ServiceExe}'), 'stop', '', SW_HIDE, ewWaitUntilTerminated,
      ResultCode);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
    StopRunning();
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  // The app's own "Start with Windows" setting writes the value without the installer knowing
  if CurUninstallStep = usPostUninstall then
    RegDeleteValue(HKEY_CURRENT_USER, '{#RunKey}', '{#AppName}');
end;
