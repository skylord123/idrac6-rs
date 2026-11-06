; Inno Setup script for the Windows installer.
;
; Build (from the repository root, after `cargo build --release`):
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

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#Root}\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"; Comment: "Virtual console, virtual media and web UI proxy for Dell iDRAC6"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
