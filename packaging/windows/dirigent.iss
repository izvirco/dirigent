; Dirigent's Windows installer (Inno Setup 6), which is also how Dirigent updates: the app
; downloads the next version's installer and runs it with
;   /SILENT /SUPPRESSMSGBOXES /NORESTART /UPDATE /WAITPID=<its pid>
; then quits (dirigent_desktop/src/update.rs). Setup waits for that process to exit,
; replaces the files (closing a runtime that still has them open), and with /UPDATE starts
; Dirigent again. Every update thus runs the new version's installer: the files, registry
; entries and this script itself can all change in any release.
;
; Never change: AppId (it is how an update finds the install), the flags above.
;
; Built by the release workflow (.github/workflows/release.yml):
;   iscc /DVersion=0.1.0 /DDist=<dir> packaging\windows\dirigent.iss
; <dir> holds Dirigent.exe, runtime\ (dirigent_runtime/build.ts) and licenses\; the
; installer is written there as Dirigent-Setup-<version>.exe.

#ifndef Version
  #define Version "0.0.0"
#endif
#ifndef Dist
  #define Dist "..\..\dist"
#endif

[Setup]
AppId={{62E36A4A-63C4-495C-8340-00B3023A0729}
AppName=Dirigent
AppVersion={#Version}
AppVerName=Dirigent {#Version}
AppPublisher=Dirigent
AppPublisherURL=https://dirigent.sebba.dev
; Per user, no admin: %LOCALAPPDATA%\Programs\Dirigent (data stays in %LOCALAPPDATA%\dirigent).
PrivilegesRequired=lowest
DefaultDirName={autopf}\Dirigent
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableReadyPage=yes
OutputDir={#Dist}
OutputBaseFilename=Dirigent-Setup-{#Version}
SetupIconFile=..\..\dirigent_desktop\assets\dirigent.ico
UninstallDisplayIcon={app}\Dirigent.exe
UninstallDisplayName=Dirigent
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Files still in use once Dirigent has exited (its runtime shutting down) are closed.
CloseApplications=force
RestartApplications=no

[Tasks]
Name: contextmenu; Description: "Add ""Open in Dirigent"" to the folder right-click menu"
Name: desktopicon; Description: "Create a desktop shortcut"; Flags: unchecked

[InstallDelete]
; The runtime's files are replaced wholesale: nothing of an older pi lingers.
Type: filesandordirs; Name: "{app}\runtime"

[Files]
Source: "{#Dist}\Dirigent.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Dist}\runtime\*"; DestDir: "{app}\runtime"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#Dist}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Dirigent"; Filename: "{app}\Dirigent.exe"
Name: "{autodesktop}\Dirigent"; Filename: "{app}\Dirigent.exe"; Tasks: desktopicon

[Registry]
; "Open in Dirigent" on a folder, on a folder's background (inside it) and on a drive. The
; app takes the folder as its argument and hands it to the running instance if there is one.
; Windows 11 lists these under "Show more options".
Root: HKA; Subkey: "Software\Classes\Directory\shell\Dirigent"; ValueType: string; ValueName: ""; ValueData: "Open in Dirigent"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\shell\Dirigent"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\Dirigent.exe"",0"; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\shell\Dirigent\command"; ValueType: string; ValueName: ""; ValueData: """{app}\Dirigent.exe"" ""%V"""; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\Dirigent"; ValueType: string; ValueName: ""; ValueData: "Open in Dirigent"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\Dirigent"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\Dirigent.exe"",0"; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\Dirigent\command"; ValueType: string; ValueName: ""; ValueData: """{app}\Dirigent.exe"" ""%V"""; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Drive\shell\Dirigent"; ValueType: string; ValueName: ""; ValueData: "Open in Dirigent"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Drive\shell\Dirigent"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\Dirigent.exe"",0"; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Drive\shell\Dirigent\command"; ValueType: string; ValueName: ""; ValueData: """{app}\Dirigent.exe"" ""%V"""; Tasks: contextmenu

[Run]
; A first install offers to start it; an update starts it again.
Filename: "{app}\Dirigent.exe"; Description: "Start Dirigent"; Flags: nowait postinstall skipifsilent
Filename: "{app}\Dirigent.exe"; Flags: nowait; Check: IsUpdate

[UninstallDelete]
; Downloaded updates. Threads, settings and logs (%LOCALAPPDATA%\dirigent, %APPDATA%\dirigent) stay.
Type: filesandordirs; Name: "{localappdata}\dirigent\updates"

[Code]
const
  SYNCHRONIZE = $00100000;

function OpenProcess(Access: Cardinal; Inherit: Boolean; Pid: Cardinal): Cardinal;
  external 'OpenProcess@kernel32.dll stdcall';
function WaitForSingleObject(Handle: Cardinal; Milliseconds: Cardinal): Cardinal;
  external 'WaitForSingleObject@kernel32.dll stdcall';
function CloseHandle(Handle: Cardinal): Boolean;
  external 'CloseHandle@kernel32.dll stdcall';

{ The bare /UPDATE switch. Not a param constant: those only read /NAME=value. }
function IsUpdate(): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), '/UPDATE') = 0 then
      Result := True;
end;

{ Where pi looks for bash: Git for Windows in Program Files, else a bash.exe on PATH that
  isn't WSL's. }
function BashFound(): Boolean;
var
  OnPath: String;
begin
  OnPath := FileSearch('bash.exe', GetEnv('PATH'));
  Result := FileExists(ExpandConstant('{commonpf64}\Git\bin\bash.exe'))
    or FileExists(ExpandConstant('{commonpf32}\Git\bin\bash.exe'))
    or ((OnPath <> '') and (Pos(LowerCase(ExpandConstant('{sys}')), LowerCase(OnPath)) <> 1));
end;

function InitializeSetup(): Boolean;
var
  Pid: Cardinal;
  Handle: Cardinal;
begin
  { An update: let the Dirigent that started us exit first (it quits right after). }
  Pid := StrToIntDef(ExpandConstant('{param:WAITPID|0}'), 0);
  if Pid <> 0 then
  begin
    Handle := OpenProcess(SYNCHRONIZE, False, Pid);
    if Handle <> 0 then
    begin
      WaitForSingleObject(Handle, 30000);
      CloseHandle(Handle);
    end;
  end;
  if not WizardSilent() and not BashFound() then
    MsgBox('Dirigent''s agents run commands with Git for Windows'' bash, which isn''t installed.' + #13#10#13#10 +
      'Get it from https://git-scm.com/download/win (now or later; Dirigent installs either way).', mbInformation, MB_OK);
  Result := True;
end;
