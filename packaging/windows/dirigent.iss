#ifndef Version
  #define Version "0.0.0"
#endif
#ifndef Dist
  #define Dist "..\..\dist"
#endif
#if Pos("-nightly.", Version) > 0
  #define Id "{{92A54733-5874-4F63-BB88-EDBA04F3B7EE}"
  #define Name "Dirigent Nightly"
  #define ShellKey "DirigentNightly"
  #define PathCommand "dirigent-nightly"
  #define DataDirName "dirigent-nightly"
  #define Icon "dirigent_nightly.ico"
#else
  #define Id "{{62E36A4A-63C4-495C-8340-00B3023A0729}"
  #define Name "Dirigent"
  #define ShellKey "Dirigent"
  #define PathCommand "dirigent"
  #define DataDirName "dirigent"
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
CloseApplications=force
RestartApplications=no
; Tells running programs (Explorer, so new terminals) that PATH changed, on install and uninstall.
ChangesEnvironment=yes

[Tasks]
Name: contextmenu; Description: "Add ""Open in {#Name}"" to the folder right-click menu"
Name: desktopicon; Description: "Create a desktop shortcut"; Flags: unchecked
; Older installs get this ticked on their next update: Inno restores each task's previous state, and a
; task it hasn't seen before keeps its default.
Name: addtopath; Description: "Add the {#PathCommand} command to PATH"
Name: service; Description: "Run the {#Name} service when I sign in, so that other machines can use this PC as a host"; Flags: unchecked

[InstallDelete]
Type: filesandordirs; Name: "{app}\runtime"
Type: files; Name: "{app}\Dirigent.exe"

[Files]
Source: "{#Dist}\dirigent_desktop.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Dist}\dirigent.exe"; DestDir: "{app}"; DestName: "{#PathCommand}.exe"; Flags: ignoreversion
Source: "{#Dist}\dirigent_service.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Dist}\cua-driver.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Dist}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#Name}"; Filename: "{app}\dirigent_desktop.exe"
Name: "{autodesktop}\{#Name}"; Filename: "{app}\dirigent_desktop.exe"; Tasks: desktopicon

[Registry]
Root: HKA; Subkey: "Software\Classes\Directory\shell\{#ShellKey}"; ValueType: string; ValueName: ""; ValueData: "Open in {#Name}"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\shell\{#ShellKey}"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\dirigent_desktop.exe"",0"; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\shell\{#ShellKey}\command"; ValueType: string; ValueName: ""; ValueData: """{app}\dirigent_desktop.exe"" ""%V"""; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\{#ShellKey}"; ValueType: string; ValueName: ""; ValueData: "Open in {#Name}"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\{#ShellKey}"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\dirigent_desktop.exe"",0"; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\{#ShellKey}\command"; ValueType: string; ValueName: ""; ValueData: """{app}\dirigent_desktop.exe"" ""%V"""; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Drive\shell\{#ShellKey}"; ValueType: string; ValueName: ""; ValueData: "Open in {#Name}"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Drive\shell\{#ShellKey}"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\dirigent_desktop.exe"",0"; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Drive\shell\{#ShellKey}\command"; ValueType: string; ValueName: ""; ValueData: """{app}\dirigent_desktop.exe"" ""%V"""; Tasks: contextmenu

Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#Name} Service"; ValueData: """{app}\dirigent_service.exe"""; Flags: uninsdeletevalue; Tasks: service
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "{#Name} Service"; Flags: deletevalue; Tasks: not service

[Run]
; MUST KILL: rearchitecture: StartsService's line must stay above the desktop's -> a desktop that yields to a service started after it
Filename: "{app}\dirigent_service.exe"; Flags: nowait; Check: StartsService
Filename: "{app}\dirigent_desktop.exe"; Description: "Start {#Name}"; Flags: nowait postinstall skipifsilent
Filename: "{app}\dirigent_desktop.exe"; Flags: nowait; Check: RestartsDesktop

[UninstallRun]
; A service still running would keep its exe from being removed.
Filename: "powershell.exe"; Parameters: "-NoProfile -NonInteractive -Command ""Get-Process dirigent_service -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -eq '{app}\dirigent_service.exe' } | Stop-Process -Force"""; Flags: runhidden; RunOnceId: "StopService"

[UninstallDelete]
Type: filesandordirs; Name: "{localappdata}\{#DataDirName}\updates"
Type: filesandordirs; Name: "{localappdata}\{#DataDirName}\updates-service"

[Code]
const
  SYNCHRONIZE = $00100000;

function OpenProcess(Access: Cardinal; Inherit: Boolean; Pid: Cardinal): Cardinal;
  external 'OpenProcess@kernel32.dll stdcall';
function WaitForSingleObject(Handle: Cardinal; Milliseconds: Cardinal): Cardinal;
  external 'WaitForSingleObject@kernel32.dll stdcall';
function CloseHandle(Handle: Cardinal): Boolean;
  external 'CloseHandle@kernel32.dll stdcall';

{ A bare switch. Not a param constant: those only read /NAME=value. }
function HasSwitch(Name: String): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), Name) = 0 then
      Result := True;
end;

function RestartsDesktop(): Boolean;
begin
  Result := HasSwitch('/UPDATE');
end;

function StartsService(): Boolean;
begin
  Result := WizardIsTaskSelected('service') or HasSwitch('/SERVICE');
end;

function IsWslBash(Bash: String): Boolean;
begin
  Result := Pos(LowerCase(ExpandConstant('{sys}')), LowerCase(Bash)) = 1;
end;

function BashFound(): Boolean;
var
  OnPath: String;
begin
  OnPath := FileSearch('bash.exe', GetEnv('PATH'));
  Result := FileExists(ExpandConstant('{commonpf64}\Git\bin\bash.exe'))
    or FileExists(ExpandConstant('{commonpf32}\Git\bin\bash.exe'))
    or ((OnPath <> '') and not IsWslBash(OnPath));
end;

function ReadUserPath(var Paths: String): Boolean;
begin
  Result := True;
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Paths) then
  begin
    Result := not RegValueExists(HKEY_CURRENT_USER, 'Environment', 'Path');
    Paths := '';
  end;
end;

procedure SetOnPath(Add: Boolean);
var
  Paths, Dir, Padded: String;
  P: Integer;
begin
  Dir := ExpandConstant('{app}');
  if not ReadUserPath(Paths) then
    Exit;
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
  UpdaterPid: Cardinal;
  Handle: Cardinal;
begin
  UpdaterPid := StrToIntDef(ExpandConstant('{param:WAITPID|0}'), 0);
  if UpdaterPid <> 0 then
  begin
    Handle := OpenProcess(SYNCHRONIZE, False, UpdaterPid);
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
