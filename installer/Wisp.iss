#ifndef AppVersion
  #error Set AppVersion to the release version
#endif
#ifndef PayloadDir
  #error Set PayloadDir to the folder containing both Wisp executables
#endif
#ifndef OutputDir
  #error Set OutputDir to the installer output folder
#endif

[Setup]
AppId={{7D599CE0-01FA-4E7B-9E2A-E5C7325707AE}
AppName=Wisp
AppVersion={#AppVersion}
AppPublisher=Wisp
AppPublisherURL=https://github.com/aagzamov-dev/wisp
DefaultDirName={localappdata}\Programs\Wisp
DisableProgramGroupPage=yes
DisableStartupPrompt=yes
UninstallDisplayName=Wisp
UninstallDisplayIcon={app}\wisp.exe
SetupIconFile=..\assets\wisp.ico
OutputDir={#OutputDir}
OutputBaseFilename=WispSetup
PrivilegesRequired=lowest
MinVersion=10.0
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
AllowUNCPath=no
AllowNetworkDrive=no
AppMutex=Local\Wisp.Launcher.SingleInstance
SetupMutex=Local\Wisp.Setup
CloseApplications=no
RestartApplications=no

[Files]
Source: "{#AddBackslash(PayloadDir)}wisp.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#AddBackslash(PayloadDir)}wisp-settings.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\Wisp"; Filename: "{app}\wisp.exe"; WorkingDir: "{app}"
Name: "{userprograms}\Wisp Settings"; Filename: "{app}\wisp-settings.exe"; WorkingDir: "{app}"

[Code]
const
  InvalidHandle = $FFFFFFFF; // Inno Setup 6 runs as a 32-bit process.
  InvalidAttributes = $FFFFFFFF;
  GenericRead = $80000000;
  GenericWrite = $40000000;
  OpenAlways = 4;
  OpenExisting = 3;
  ShareAll = 7;
  AttributeDirectory = $10;
  AttributeReparsePoint = $400;
  OpenReparsePoint = $00200000;
  FileNotFound = 2;
  PathNotFound = 3;
  FixedDrive = 3;
  OrdinalEqual = 2;
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
  CloseWisp = 'Quit Wisp from its tray menu, close Wisp Settings, and retry.';
  InvalidInstallDir = 'Wisp needs a local folder inside your Windows user folder, with no redirected folders in its path.';

var
  UpdateLock: THandle;
  OwnsUpdateLock: Boolean;
  UpdateWorkspace: String;

function ReadAttributes(const Path: String): Cardinal;
  external 'GetFileAttributesW@kernel32.dll stdcall';
function ReadDriveType(const Root: String): Cardinal;
  external 'GetDriveTypeW@kernel32.dll stdcall';
function CompareOrdinal(const Left: String; LeftLength: Integer; const Right: String;
  RightLength, IgnoreCase: Integer): Integer;
  external 'CompareStringOrdinal@kernel32.dll stdcall';
function OpenFileHandle(const Path: String; Access, Share: Cardinal;
  Security: LongInt; Creation, Flags: Cardinal; Template: THandle): THandle;
  external 'CreateFileW@kernel32.dll stdcall';
function CloseFileHandle(Handle: THandle): LongInt;
  external 'CloseHandle@kernel32.dll stdcall';

function MissingPath: Boolean;
var
  ErrorCode: Integer;
begin
  ErrorCode := DLLGetLastError;
  Result := (ErrorCode = FileNotFound) or (ErrorCode = PathNotFound);
end;

function IsProfileInstallDir(const InstallDir: String): Boolean;
var
  Profile, Target, Drive, Directory, Component: String;
  Attributes: Cardinal;
begin
  Result := False;
  Profile := GetEnv('USERPROFILE');
  if Profile = '' then
    Exit;
  Profile := AddBackslash(ExpandFileName(Profile));
  Target := RemoveBackslashUnlessRoot(ExpandFileName(InstallDir));
  Drive := ExtractFileDrive(Target);
  if (Length(Drive) <> 2) or (Copy(Drive, 2, 1) <> ':') or
     (ReadDriveType(AddBackslash(Drive)) <> FixedDrive) or
     (CompareOrdinal(Target, -1, InstallDir, -1, 1) <> OrdinalEqual) or
     (Length(Target) <= Length(Profile)) or
     (CompareOrdinal(Target, Length(Profile), Profile, Length(Profile), 1) <> OrdinalEqual) then
    Exit;

  // An existing ancestor can redirect even when the selected folder does not exist.
  Directory := Target;
  while Length(Directory) > 3 do
  begin
    Component := ExtractFileName(Directory);
    if (Component = '') or (Pos(':', Component) <> 0) or
       (Copy(Component, Length(Component), 1) = '.') or
       (Copy(Component, Length(Component), 1) = ' ') then
      Exit;
    Attributes := ReadAttributes(Directory);
    if Attributes = InvalidAttributes then
    begin
      if not MissingPath then
        Exit;
    end
    else if (Attributes and (AttributeDirectory or AttributeReparsePoint)) <> AttributeDirectory then
      Exit;
    Directory := ExtractFileDir(Directory);
  end;
  Result := DirExists(Profile);
end;

function CanReplace(const Path: String): Boolean;
var
  Attributes: Cardinal;
  Handle: THandle;
begin
  Attributes := ReadAttributes(Path);
  if Attributes = InvalidAttributes then
  begin
    Result := MissingPath;
    Exit;
  end;
  Result := False;
  if (Attributes and (AttributeDirectory or AttributeReparsePoint)) <> 0 then
    Exit;

  // Windows denies write access while the file is mapped as a running image.
  Handle := OpenFileHandle(Path, GenericWrite, ShareAll, 0, OpenExisting, OpenReparsePoint, 0);
  if Handle <> InvalidHandle then
  begin
    CloseFileHandle(Handle);
    Result := True;
  end;
end;

procedure ReleaseUpdateLock;
begin
  if OwnsUpdateLock then
  begin
    CloseFileHandle(UpdateLock);
    OwnsUpdateLock := False;
  end;
end;

function CheckInstallation(const InstallDir: String): String;
var
  Attributes: Cardinal;
  Workspace, LockPath: String;
begin
  Result := InvalidInstallDir;
  if not IsProfileInstallDir(InstallDir) then
    Exit;
  Attributes := ReadAttributes(InstallDir);
  if Attributes = InvalidAttributes then
  begin
    if MissingPath then
      Result := '';
    Exit;
  end;
  if (Attributes and (AttributeDirectory or AttributeReparsePoint)) <> AttributeDirectory then
    Exit;

  Workspace := InstallDir + '\.wisp-update';
  Result := 'Setup cannot lock the Wisp update folder. Wait for the updater to finish and retry.';
  Attributes := ReadAttributes(Workspace);
  if Attributes = InvalidAttributes then
  begin
    if not MissingPath then
      Exit;
    if (ReadAttributes(InstallDir + '\wisp.exe') = InvalidAttributes) and MissingPath and
       (ReadAttributes(InstallDir + '\wisp-settings.exe') = InvalidAttributes) and MissingPath then
    begin
      Result := '';
      Exit;
    end;
    if not CreateDir(Workspace) then
      Exit;
    Attributes := ReadAttributes(Workspace);
  end;
  if (Attributes = InvalidAttributes) or
     ((Attributes and (AttributeDirectory or AttributeReparsePoint)) <> AttributeDirectory) then
    Exit;

  LockPath := Workspace + '\lock';
  Attributes := ReadAttributes(LockPath);
  if Attributes = InvalidAttributes then
  begin
    if not MissingPath then
      Exit;
  end
  else if (Attributes and (AttributeDirectory or AttributeReparsePoint)) <> 0 then
    Exit;

  // Match the in-app updater's lock so it cannot modify the pair during Setup.
  UpdateLock := OpenFileHandle(LockPath, GenericRead or GenericWrite, 0, 0, OpenAlways, OpenReparsePoint, 0);
  if UpdateLock = InvalidHandle then
    Exit;
  OwnsUpdateLock := True;
  UpdateWorkspace := Workspace;

  // A helper can already be starting while the Settings process hands off the lock.
  if not CanReplace(Workspace + '\wisp-update-helper.exe') then
    Exit;
  if FileExists(Workspace + '\prepared') and not FileExists(Workspace + '\committed') then
  begin
    Result := 'An earlier Wisp update needs recovery. Open Wisp Settings to recover it, then close Settings and retry.';
    Exit;
  end;
  Result := CloseWisp;
  if not CanReplace(InstallDir + '\wisp.exe') or not CanReplace(InstallDir + '\wisp-settings.exe') then
    Exit;
  Result := '';
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  ReleaseUpdateLock;
  Result := CheckInstallation(RemoveBackslashUnlessRoot(WizardDirValue));
  if Result <> '' then
    ReleaseUpdateLock;
end;

procedure DeinitializeSetup;
begin
  ReleaseUpdateLock;
end;

function InitializeUninstall: Boolean;
var
  Problem: String;
begin
  Problem := CheckInstallation(RemoveBackslashUnlessRoot(ExpandConstant('{app}')));
  Result := Problem = '';
  if not Result then
  begin
    ReleaseUpdateLock;
    Log(Problem);
    SuppressibleMsgBox(Problem, mbError, MB_OK, IDOK);
  end;
end;

procedure CurUninstallStepChanged(Step: TUninstallStep);
var
  StartCommand, InstalledLauncher: String;
begin
  if Step <> usPostUninstall then
    Exit;
  InstalledLauncher := ExpandConstant('{app}\wisp.exe');
  if not FileExists(InstalledLauncher) and
     RegQueryStringValue(HKCU, RunKey, 'Wisp', StartCommand) and
     (CompareText(StartCommand, '"' + InstalledLauncher + '"') = 0) then
    RegDeleteValue(HKCU, RunKey, 'Wisp');

  if not OwnsUpdateLock or FileExists(InstalledLauncher) or
     FileExists(ExpandConstant('{app}\wisp-settings.exe')) then
    Exit;

  // Remove only known updater artifacts after both installed executables are gone.
  DeleteFile(UpdateWorkspace + '\prepared');
  DeleteFile(UpdateWorkspace + '\committed');
  DeleteFile(UpdateWorkspace + '\wisp.exe');
  DeleteFile(UpdateWorkspace + '\wisp-settings.exe');
  DeleteFile(UpdateWorkspace + '\wisp-update-helper.exe');
  DeleteFile(UpdateWorkspace + '\old-wisp.exe');
  DeleteFile(UpdateWorkspace + '\old-wisp-settings.exe');
  DeleteFile(UpdateWorkspace + '\restore.tmp');
  DeleteFile(UpdateWorkspace + '\marker.tmp');
  ReleaseUpdateLock;
  DeleteFile(UpdateWorkspace + '\lock');
  RemoveDir(UpdateWorkspace);
  RemoveDir(ExpandConstant('{app}'));
end;

procedure DeinitializeUninstall;
begin
  ReleaseUpdateLock;
end;
