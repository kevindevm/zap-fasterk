﻿; No longer shipped: releases ship only the Windows Installer package
; (zap-fasterk.wxs), which takes over a copy this script installed.
; test-installer.ps1 still builds it to make such a copy to take over.
;
; Windows installer built from a release binary with Inno Setup 6.3 or newer:
;
;   iscc /DVersion=0.1.0 /DArch=x86_64 /DBinary=...\zap-fasterk.exe ^
;        /DOutputDir=dist packaging\windows\zap-fasterk.iss
;
; Arch matches the Rust target: x86_64 or aarch64. Installation uses the
; current user's Programs folder and does not need administrator rights.
; Updates close a running copy before replacing it.

#ifndef Version
  #error Version must be defined on the ISCC command line
#endif
#ifndef Arch
  #error Arch must be defined on the ISCC command line (x86_64 or aarch64)
#endif
#ifndef NumericVersion
  #define NumericVersion Version
#endif
#ifndef Binary
  #error Binary must be defined on the ISCC command line
#endif
#ifndef OutputDir
  #error OutputDir must be defined on the ISCC command line
#endif
#if Arch == "aarch64"
  #define InnoArch "arm64"
#else
  #define InnoArch "x64compatible"
#endif

#define AppName "Zap Fasterk"
#define AppExeName "zap-fasterk.exe"

[Setup]
; Never change: this is how Windows tells an update from a new program.
AppId={{AE0F1449-0366-4CA8-98B4-A0186CB798A8}
AppName={#AppName}
AppVersion={#Version}
AppVerName={#AppName} {#Version}
AppPublisher=kevindevm
AppCopyright=© 2026 Carmine Paolino
AppPublisherURL=https://github.com/kevindevm/zap-fasterk
AppSupportURL=https://github.com/kevindevm/zap-fasterk/issues
AppUpdatesURL=https://github.com/kevindevm/zap-fasterk/releases
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed={#InnoArch}
ArchitecturesInstallIn64BitMode={#InnoArch}
MinVersion=10.0
LicenseFile=..\..\LICENSE
OutputDir={#OutputDir}
OutputBaseFilename=zap-fasterk-v{#Version}-{#Arch}-pc-windows-msvc-setup
SetupIconFile=zap-fasterk.ico
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
UninstallDisplayIcon={app}\{#AppExeName}
VersionInfoVersion={#NumericVersion}.0

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
Source: "{#Binary}"; DestDir: "{app}"; Flags: ignoreversion
; Updaters up to 0.16.5 relaunch the executable they were started from, so an
; update begun as fastsapp.exe needs that file to come back (as Spotifast's
; #582). The app deletes the copy once it starts as zap-fasterk.exe with no update
; running, and later updaters relaunch zap-fasterk.exe themselves.
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\THIRD-PARTY-NOTICES.md"; DestDir: "{app}"; Flags: ignoreversion

Source: "zap-fasterk-installer.txt"; DestDir: "{app}"; Flags: ignoreversion

[InstallDelete]
; AppId keeps upgrades in the existing installation directory. Remove the
; previous shortcuts; fastsapp.exe is kept above for older updaters.

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExeName}"; AppUserModelID: "io.github.kevindevm.ZapFasterk"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon; AppUserModelID: "io.github.kevindevm.ZapFasterk"

[Run]
Filename: "{app}\{#AppExeName}"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent
