; Sirin Windows Installer — Inno Setup 6.x
; Build: iscc sirin.iss
; Output: Output\SirinSetup-x.y.z.exe

#define MyAppName       "Sirin"
#define MyAppPublisher  "Redandan"
#define MyAppURL        "https://github.com/Redandan/Sirin"
#define MyAppExeName    "sirin.exe"

; Version injected by CI: iscc /DMyAppVersion=0.3.0 sirin.iss
; Falls back to 0.3.0 if not passed.
#ifndef MyAppVersion
  #define MyAppVersion "0.3.0"
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
Name: "startupentry"; Description: "自動登入時啟動 Sirin (Start with Windows)"; GroupDescription: "開機行為"
Name: "iphoneusb"; Description: "安裝 Sirin 實體 iPhone USB 驗收能力（Driver、常駐任務、Codex MCP/Skill）"; GroupDescription: "裝置驗收"; Flags: unchecked

[Files]
; Main binary — always fresh copy
Source: "target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion

; Sirin-owned physical-iPhone component. The public entry is
; scripts\install-sirin-ios.ps1; the other scripts are private implementation.
Source: "scripts\install-sirin-ios.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "scripts\install-ios-driver-runtime.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "scripts\install-ios-driver-task.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "scripts\install-sirin-daemon-task.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "scripts\install-codex-ios-integration.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "scripts\soak-ios-device.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "scripts\switch-sirin-daemon.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "scripts\verify-sirin-ios-physical.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "integrations\ios-driver\README.md"; DestDir: "{app}\integrations\ios-driver"; Flags: ignoreversion
Source: "integrations\ios-driver\THIRD_PARTY_LICENSE"; DestDir: "{app}\integrations\ios-driver"; Flags: ignoreversion
Source: "integrations\ios-driver\requirements.txt"; DestDir: "{app}\integrations\ios-driver"; Flags: ignoreversion
Source: "integrations\ios-driver\scripts\unattended_host.py"; DestDir: "{app}\integrations\ios-driver\scripts"; Flags: ignoreversion
Source: "integrations\ios-driver\src\phone_harness\*"; DestDir: "{app}\integrations\ios-driver\src\phone_harness"; Excludes: "__pycache__\*,*.pyc"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "integrations\codex\skills\iphone-usb-validation\*"; DestDir: "{app}\integrations\codex\skills\iphone-usb-validation"; Flags: ignoreversion recursesubdirs createallsubdirs

; App-managed defaults. The Sirin post-install entry copies these into the
; original user's profile only when missing, so upgrades preserve user edits.
Source: ".env.example"; DestDir: "{app}\defaults"; DestName: ".env.example"; Flags: ignoreversion
Source: "config\agents.yaml";  DestDir: "{app}\defaults\config"; Flags: ignoreversion
Source: "config\persona.yaml"; DestDir: "{app}\defaults\config"; Flags: ignoreversion

; Bundled YAML skills (overwrite on upgrade — these are app-managed, not user-editable)
; Source: "config\skills\*"; DestDir: "{localappdata}\Sirin\config\skills"; Flags: ignoreversion recursesubdirs

[Icons]
Name: "{group}\{#MyAppName}";         Filename: "{app}\{#MyAppExeName}"
Name: "{group}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{commondesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
; Auto-launch after install — runs in both interactive and silent modes.
; Silent mode is used by Sirin's in-app self-update flow, so we MUST relaunch
; (otherwise the user sees Sirin disappear with no feedback).  Removing
; "skipifsilent" enables this; "runasoriginaluser" ensures we drop back from
; the elevated installer context to the user's normal session.
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; \
  Tasks: not iphoneusb; Flags: nowait postinstall runasoriginaluser

[UninstallRun]
; Remove only Sirin-owned tasks. Evidence, local runtime, Codex configuration and
; installer backups remain recoverable under the user's profile.
Filename: "powershell.exe"; \
  Parameters: "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File ""{app}\scripts\install-sirin-ios.ps1"" -Action Remove -Repo ""{app}"" -Binary ""{app}\{#MyAppExeName}"""; \
  Flags: runhidden waituntilterminated; RunOnceId: "SirinIosTasks"

[Code]
procedure CurStepChanged(CurStep: TSetupStep);
var
  ResultCode: Integer;
  PowerShellPath: String;
  Parameters: String;
begin
  if CurStep = ssPostInstall then
  begin
    PowerShellPath := ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe');
    if WizardIsTaskSelected('iphoneusb') then
      Parameters :=
        '-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "' +
        ExpandConstant('{app}\scripts\install-sirin-ios.ps1') +
        '" -Action Install -Repo "' + ExpandConstant('{app}') +
        '" -Binary "' + ExpandConstant('{app}\{#MyAppExeName}') +
        '" -RunNow -MigrateLegacySideTap'
    else
    begin
      Parameters :=
        '-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "' +
        ExpandConstant('{app}\scripts\install-sirin-ios.ps1') +
        '" -Action InitializeUser -Repo "' + ExpandConstant('{app}') +
        '" -Binary "' + ExpandConstant('{app}\{#MyAppExeName}') + '"';
      if WizardIsTaskSelected('startupentry') then
        Parameters := Parameters + ' -EnableStartup';
    end;

    { [Run] only waits; ExecAsOriginalUser lets Setup enforce the child result. }
    if not ExecAsOriginalUser(
      PowerShellPath,
      Parameters,
      ExpandConstant('{app}'),
      SW_HIDE,
      ewWaitUntilTerminated,
      ResultCode
    ) then
      RaiseException('Could not start the Sirin post-install process.')
    else if ResultCode <> 0 then
      RaiseException(
        'Sirin post-install readiness failed with exit code ' +
        IntToStr(ResultCode) + '. Any changed scheduled tasks were restored.'
      );
  end;
end;
