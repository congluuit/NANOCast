; NANOCast installer (Inno Setup 6).
;
; Build:   cargo build --release -p nanocast
;          ISCC.exe installer\nanocast.iss
; Output:  installer\out\NANOCast-Setup-<version>.exe
;
; The version is read from [workspace.package] in Cargo.toml unless passed
; with /DAppVersion=x.y.z. The exe location can be overridden with /DExeDir=...
;
; Installs per user by default (no admin): %LOCALAPPDATA%\Programs\NANOCast.
; The wizard lets the user switch to "all users" (Program Files, needs admin).

#ifndef AppVersion
  #define CargoToml AddBackslash(SourcePath) + "..\Cargo.toml"
  #define FileHandle FileOpen(CargoToml)
  #if !FileHandle
    #error Cannot open Cargo.toml; pass /DAppVersion=x.y.z
  #endif
  #define AppVersion ""
  #define Line ""
  #sub ReadVersionLine
    #expr Line = Trim(FileRead(FileHandle))
    #if Pos('version = "', Line) == 1
      #expr AppVersion = Copy(Line, 12, RPos('"', Line) - 12)
    #endif
  #endsub
  #for {0; AppVersion == "" && !FileEof(FileHandle); 0} ReadVersionLine
  #expr FileClose(FileHandle)
  #if AppVersion == ""
    #error No version found in Cargo.toml; pass /DAppVersion=x.y.z
  #endif
#endif

#ifndef ExeDir
  #define ExeDir "..\target\release"
#endif

#define AppName "NANOCast"
#define AppExe "nanocast.exe"
#define AppUrl "https://github.com/redharegames/NANOCast"

[Setup]
; Fixed AppId: keep it so upgrades replace the old install.
AppId={{6E8C1C3A-4F0B-4B7E-9D2A-5A1C0CA57A10}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=NANOCast contributors
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}/issues
AppUpdatesURL={#AppUrl}/releases
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=out
OutputBaseFilename=NANOCast-Setup-{#AppVersion}
SetupIconFile=..\assets\nanocast.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
WizardStyle=modern
Compression=lzma2/ultra64
SolidCompression=yes
CloseApplications=yes
ShowLanguageDialog=auto

[Languages]
Name: "vietnamese"; MessagesFile: "Languages\Vietnamese.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "{#ExeDir}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
