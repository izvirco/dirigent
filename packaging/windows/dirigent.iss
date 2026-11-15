; Dirigent's Windows installer (Inno Setup 6), which is also how Dirigent updates: the app
; downloads the next version's installer and runs it with
;   /SILENT /SUPPRESSMSGBOXES /NORESTART /UPDATE /WAITPID=<its pid>
; then quits (dirigent_desktop/src/update.rs). Setup waits for that process to exit,
; replaces the files (closing a runtime that still has them open), and with /UPDATE starts
; Dirigent again. Every update thus runs the new version's installer: the files, registry
; entries and this script itself can all change in any release.
;
; Never change: the AppIds (each is how an update finds its install), the flags above.
;
; A nightly (a Version with "-nightly." in it, as the app itself tells: dirigent_protocol's
; release.rs) installs as an app of its own, Dirigent Nightly, next to a stable one: its own
; AppId, directory, shortcuts, right-click entry and command (dirigent-nightly), so neither
; touches the other. Its exes are nightly builds, with their own data dirs and icon.
;
; Built by the release workflow (.github/workflows/release.yml):
;   iscc /DVersion=0.1.0 /DDist=<dir> packaging\windows\dirigent.iss
; <dir> holds dirigent_desktop.exe (the app), dirigent.exe (the command line: `dirigent pi`,
; `dirigent [dir]`; on the user's PATH unless the task is unticked), runtime\ (dirigent_runtime/build.ts) and licenses\;
; the installer is written there as Dirigent-Setup-<version>.exe.

#ifndef Version
  #define Version "0.0.0"
#endif
#ifndef Dist
  #define Dist "..\..\dist"
#endif
; Name: what the user sees. Key: the right-click entry's registry key. Cli: the command on
; PATH. Data: the data dir's name in %LOCALAPPDATA%, which the app picks (release::app).
#if Pos("-nightly.", Version) > 0
  #define Id "{{92A54733-5874-4F63-BB88-EDBA04F3B7EE}"
  #define Name "Dirigent Nightly"
  #define Key "DirigentNightly"
  #define Cli "dirigent-nightly"
  #define Data "dirigent-nightly"
  #define Icon "dirigent_nightly.ico"
#else
  #define Id "{{62E36A4A-63C4-495C-8340-00B3023A0729}"
  #define Name "Dirigent"
  #define Key "Dirigent"
  #define Cli "dirigent"
  #define Data "dirigent"
  #define Icon "dirigent.ico"
#endif

[Setup]
AppId={#Id}
AppName={#Name}
AppVersion={#Version}
AppVerName={#Name} {#Version}
AppPublisher=Dirigent
AppPublisherURL=https://dirigent.sebba.dev
; Per user, no admin: %LOCALAPPDATA%\Programs\Dirigent (data stays in %LOCALAPPDATA%\dirigent).
PrivilegesRequired=lowest
DefaultDirName={autopf}\{#Name}
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableReadyPage=yes
OutputDir={#Dist}
OutputBaseFilename=Dirigent-Setup-{#Version}
SetupIconFile=..\..\dirigent_desktop\assets\{#Icon}
UninstallDisplayIcon={app}\dirigent_desktop.exe
UninstallDisplayName={#Name}
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Files still in use once Dirigent has exited (its runtime shutting down) are closed.
CloseApplications=force
RestartApplications=no
; Tells running programs (Explorer, so new terminals) that PATH changed, on install and uninstall.
ChangesEnvironment=yes

[Tasks]
Name: contextmenu; Description: "Add ""Open in {#Name}"" to the folder right-click menu"
Name: desktopicon; Description: "Create a desktop shortcut"; Flags: unchecked
; Older installs get this ticked on their next update: Inno restores each task's previous state, and a
; task it hasn't seen before keeps its default.
Name: addtopath; Description: "Add the {#Cli} command to PATH ({#Cli} pi install ...)"

[InstallDelete]
; The runtime's files are replaced wholesale: nothing of an older pi lingers.
Type: filesandordirs; Name: "{app}\runtime"
; The app's name in the first releases; dirigent.exe is the command line now (the same name on NTFS).
; A nightly never had one.
Type: files; Name: "{app}\Dirigent.exe"

[Files]
Source: "{#Dist}\dirigent_desktop.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Dist}\dirigent.exe"; DestDir: "{app}"; DestName: "{#Cli}.exe"; Flags: ignoreversion
Source: "{#Dist}\runtime\*"; DestDir: "{app}\runtime"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#Dist}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#Name}"; Filename: "{app}\dirigent_desktop.exe"
Name: "{autodesktop}\{#Name}"; Filename: "{app}\dirigent_desktop.exe"; Tasks: desktopicon

[Registry]
; "Open in Dirigent" on a folder, on a folder's background (inside it) and on a drive. The
; app takes the folder as its argument and hands it to the running instance if there is one.
; Windows 11 lists these under "Show more options".
Root: HKA; Subkey: "Software\Classes\Directory\shell\{#Key}"; ValueType: string; ValueName: ""; ValueData: "Open in {#Name}"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\shell\{#Key}"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\dirigent_desktop.exe"",0"; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\shell\{#Key}\command"; ValueType: string; ValueName: ""; ValueData: """{app}\dirigent_desktop.exe"" ""%V"""; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\{#Key}"; ValueType: string; ValueName: ""; ValueData: "Open in {#Name}"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\{#Key}"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\dirigent_desktop.exe"",0"; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\{#Key}\command"; ValueType: string; ValueName: ""; ValueData: """{app}\dirigent_desktop.exe"" ""%V"""; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Drive\shell\{#Key}"; ValueType: string; ValueName: ""; ValueData: "Open in {#Name}"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Drive\shell\{#Key}"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\dirigent_desktop.exe"",0"; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Drive\shell\{#Key}\command"; ValueType: string; ValueName: ""; ValueData: """{app}\dirigent_desktop.exe"" ""%V"""; Tasks: contextmenu

[Run]
; A first install offers to start it; an update starts it again.
Filename: "{app}\dirigent_desktop.exe"; Description: "Start {#Name}"; Flags: nowait postinstall skipifsilent
Filename: "{app}\dirigent_desktop.exe"; Flags: nowait; Check: IsUpdate

[UninstallDelete]
; Downloaded updates. Threads, settings and logs (%LOCALAPPDATA%\dirigent, %APPDATA%\dirigent) stay.
Type: filesandordirs; Name: "{localappdata}\{#Data}\updates"

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

{ Puts the install dir on the user's PATH (HKCU\Environment, where a per-user install may write) or
  takes it off, leaving the other entries as they are. A Path that isn't a string is left alone. }
procedure SetOnPath(Add: Boolean);
var
  Paths, Dir, Padded: String;
  P: Integer;
begin
  Dir := ExpandConstant('{app}');
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Paths) then
  begin
    if RegValueExists(HKEY_CURRENT_USER, 'Environment', 'Path') then
      Exit;
    Paths := '';
  end;
  Padded := ';' + Paths + ';';
  P := Pos(';' + Uppercase(Dir) + ';', Uppercase(Padded));
  if Add then
  begin
    if P = 0 then
    begin
      if Paths <> '' then
        if Paths[Length(Paths)] <> ';' then
          Paths := Paths + ';';
      RegWriteExpandStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Paths + Dir);
    end;
  end
  else if P <> 0 then
  begin
    { Drop ';<dir>' and the padding again. }
    Delete(Padded, P, Length(Dir) + 1);
    Paths := Copy(Padded, 2, Length(Padded) - 2);
    if Paths = '' then
      RegDeleteValue(HKEY_CURRENT_USER, 'Environment', 'Path')
    else
      RegWriteExpandStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Paths);
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    SetOnPath(WizardIsTaskSelected('addtopath'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    SetOnPath(False);
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
