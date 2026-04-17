; Sirin Windows Installer — Inno Setup 6.x
; Build: iscc sirin.iss
; Output: Output\SirinSetup-x.y.z.exe

#define MyAppName       "Sirin"
#define MyAppPublisher  "Redandan"
#define MyAppURL        "https://github.com/Redandan/Sirin"
#define MyAppExeName    "sirin.exe"

; Version injected by CI: iscc /DMyAppVersion=0.2.0 sirin.iss
; Falls back to 0.2.0 if not passed.
#ifndef MyAppVersion
  #define MyAppVersion "0.2.0"
#endif

[Setup]
AppId={{E4A1C2D3-7B8F-4E5A-9C6D-1F2A3B4C5D6E}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}/issues
AppUpdatesURL={#MyAppURL}/releases
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
LicenseFile=LICENSE
OutputDir=Output
OutputBaseFilename=SirinSetup-{#MyAppVersion}
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
ArchitecturesInstallIn64BitMode=x64compatible
ArchitecturesAllowed=x64compatible
; Minimum Windows 10
MinVersion=10.0

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon";  Description: "{cm:CreateDesktopIcon}";  GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "startupentry"; Description: "自動登入時啟動 Sirin (Start with Windows)"; GroupDescription: "開機行為"; Flags: unchecked

[Files]
; Main binary — always fresh copy
Source: "target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion

; Default env template — only copy if user doesn't have one yet
; (user's actual .env lives in %LOCALAPPDATA%\Sirin\.env, managed by the app)
Source: ".env.example"; DestDir: "{localappdata}\Sirin"; DestName: ".env.example"; Flags: ignoreversion onlyifdoesntexist; Check: AlwaysTrue

; Default config files — only install if missing (preserves user edits on upgrade)
Source: "config\agents.yaml";   DestDir: "{localappdata}\Sirin\config"; Flags: ignoreversion onlyifdoesntexist
Source: "config\persona.yaml";  DestDir: "{localappdata}\Sirin\config"; Flags: ignoreversion onlyifdoesntexist

; Bundled YAML skills (overwrite on upgrade — these are app-managed, not user-editable)
; Source: "config\skills\*"; DestDir: "{localappdata}\Sirin\config\skills"; Flags: ignoreversion recursesubdirs

[Icons]
Name: "{group}\{#MyAppName}";         Filename: "{app}\{#MyAppExeName}"
Name: "{group}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{commondesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
; Auto-start entry (created only if task "startupentry" selected)
Root: HKCU; Subkey: "SOFTWARE\Microsoft\Windows\CurrentVersion\Run"; \
  ValueType: string; ValueName: "{#MyAppName}"; \
  ValueData: """{app}\{#MyAppExeName}"""; \
  Tasks: startupentry; Flags: uninsdeletevalue

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; \
  Flags: nowait postinstall skipifsilent

[Code]
function AlwaysTrue: Boolean;
begin
  Result := True; // always run — the actual guard is the onlyifdoesntexist flag
end;
