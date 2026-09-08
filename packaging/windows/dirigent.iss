; Build with /DChannel=... /DReleaseVersion=... /DBinaryName=... /DStageDir=... /DOutputDir=...
; StageDir contains dirigent.exe, current.json, and versions/<channel>-<version>/dirigent_desktop.exe.
#if Channel == "stable"
  #define AppName "Dirigent"
  #define OpenHereLabel "Open dirigent here"
#else
  #define AppName "Dirigent (" + Channel + ")"
  #define OpenHereLabel "Open dirigent here (" + Channel + ")"
#endif

[Setup]
AppId=Dirigent-{#Channel}
AppName={#AppName}
AppVersion={#ReleaseVersion}
AppPublisher=Seb
AppPublisherURL=https://dirigent.sebba.dev
DefaultDirName={localappdata}\Programs\Dirigent\{#Channel}
DefaultGroupName={#AppName}
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
DisableDirPage=yes
DisableProgramGroupPage=yes
OutputDir={#OutputDir}
OutputBaseFilename={#BinaryName}-setup
SetupIconFile=..\..\dirigent_desktop\asset\icon.ico
UninstallDisplayIcon={app}\dirigent.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
AppMutex=Dirigent-{#Channel}
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: checkedonce

[Files]
Source: "{#StageDir}\dirigent.exe"; DestDir: "{app}"; Flags: ignoreversion
; Published version directories are immutable, including across installer reruns.
Source: "{#StageDir}\versions\{#Channel}-{#ReleaseVersion}\dirigent_desktop.exe"; DestDir: "{app}\versions\{#Channel}-{#ReleaseVersion}"; Flags: onlyifdoesntexist
Source: "{#StageDir}\current.json"; DestDir: "{app}"; DestName: "current.next.json"; Flags: ignoreversion uninsneveruninstall

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\dirigent.exe"; AppUserModelID: "dirigent-{#Channel}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\dirigent.exe"; Tasks: desktopicon; AppUserModelID: "dirigent-{#Channel}"

[Registry]
; Per-user, channel-owned verbs: uninstalling one channel leaves the others alone.
Root: HKCU; Subkey: "Software\Classes\Directory\shell\Dirigent-{#Channel}"; ValueType: string; ValueName: ""; ValueData: "{#OpenHereLabel}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Directory\shell\Dirigent-{#Channel}"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\dirigent.exe"""
; The trailing \. keeps drive roots/trailing backslashes from escaping the closing quote.
Root: HKCU; Subkey: "Software\Classes\Directory\shell\Dirigent-{#Channel}\command"; ValueType: string; ValueName: ""; ValueData: """{app}\dirigent.exe"" --open-project ""%1\."""
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\Dirigent-{#Channel}"; ValueType: string; ValueName: ""; ValueData: "{#OpenHereLabel}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\Dirigent-{#Channel}"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\dirigent.exe"""
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\Dirigent-{#Channel}\command"; ValueType: string; ValueName: ""; ValueData: """{app}\dirigent.exe"" --open-project ""%V\."""

[Run]
Filename: "{app}\dirigent.exe"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Only installation-owned files. Channel data/configuration are intentionally retained.
Type: filesandordirs; Name: "{app}\versions"
Type: files; Name: "{app}\current.json"
Type: files; Name: "{app}\current.next.json"
Type: files; Name: "{app}\install.lock"
Type: dirifempty; Name: "{app}"

[Code]
function MoveFileEx(ExistingFileName, NewFileName: String; Flags: Cardinal): Boolean;
  external 'MoveFileExW@kernel32.dll stdcall';

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then begin
    { Publish only after all executables have been installed. Never truncate the live selection. }
    if not MoveFileEx(ExpandConstant('{app}\current.next.json'),
      ExpandConstant('{app}\current.json'), 1 or 8) then
      RaiseException('Could not select the installed version. Close Dirigent and retry setup.');
  end;
end;
