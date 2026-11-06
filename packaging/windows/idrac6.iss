; Inno Setup script for the Windows installer.
;
; Build (from the repository root) after building target\release\idrac6.exe
; (`cargo build --release`) and target\release\idrac6-cli.exe (the same
; with `--features console`, copied to that name):
;   iscc /DAppVersion=0.1.0 packaging\windows\idrac6.iss
; The installer is written to target\installer\.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#define AppName "iDRAC6 Console"
#define AppExe "idrac6.exe"
#define Root "..\.."

[Setup]
AppId={{EFF0571B-FB0D-4BF3-8FEE-4810558B40C5}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=idrac6-rs
AppPublisherURL=https://github.com/skylord123/idrac6-rs
AppSupportURL=https://github.com/skylord123/idrac6-rs/issues
AppUpdatesURL=https://github.com/skylord123/idrac6-rs/releases
DefaultDirName={autopf}\idrac6-rs
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
; Installs for all users with admin rights, or just for the current user
; without them.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=commandline dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
LicenseFile={#Root}\LICENSE
SetupIconFile={#Root}\assets\idrac6.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
OutputDir={#Root}\target\installer
OutputBaseFilename=idrac6-windows-x86_64-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Close a running idrac6 before replacing it.
CloseApplications=yes
RestartApplications=no
; The "Add to PATH" task changes the environment.
ChangesEnvironment=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "addtopath"; Description: "Add to PATH, to use idrac6-cli from a terminal"; GroupDescription: "Command line:"; Flags: unchecked

[Files]
Source: "{#Root}\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\target\release\idrac6-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"; Comment: "Virtual console, virtual media and web UI proxy for Dell iDRAC6"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

[Code]
// "Add to PATH": the user's PATH for a per-user install, the system PATH
// for an all-users install. Removed again on uninstall.

function PathRoot: Integer;
begin
  if IsAdminInstallMode then
    Result := HKEY_LOCAL_MACHINE
  else
    Result := HKEY_CURRENT_USER;
end;

function PathKey: String;
begin
  if IsAdminInstallMode then
    Result := 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
  else
    Result := 'Environment';
end;

function PathHas(Paths, Dir: String): Boolean;
begin
  Result := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';') > 0;
end;

procedure AddToPath(Dir: String);
var
  Paths: String;
begin
  if not RegQueryStringValue(PathRoot, PathKey, 'Path', Paths) then
    Paths := '';
  if PathHas(Paths, Dir) then
    exit;
  if (Paths <> '') and (Copy(Paths, Length(Paths), 1) <> ';') then
    Paths := Paths + ';';
  RegWriteExpandStringValue(PathRoot, PathKey, 'Path', Paths + Dir);
end;

procedure RemoveFromPath(Dir: String);
var
  Paths: String;
  P: Integer;
begin
  if not RegQueryStringValue(PathRoot, PathKey, 'Path', Paths) then
    exit;
  Paths := ';' + Paths + ';';
  P := Pos(';' + Uppercase(Dir) + ';', Uppercase(Paths));
  if P = 0 then
    exit;
  Delete(Paths, P, Length(Dir) + 1);
  Paths := Copy(Paths, 2, Length(Paths) - 2);
  RegWriteExpandStringValue(PathRoot, PathKey, 'Path', Paths);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    AddToPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemoveFromPath(ExpandConstant('{app}'));
end;
