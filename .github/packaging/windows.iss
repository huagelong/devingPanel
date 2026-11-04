#ifndef AppVersion
  #error AppVersion is required
#endif
#ifndef AppArch
  #error AppArch is required
#endif
#ifndef SourceExe
  #error SourceExe is required
#endif
#ifndef OutputDir
  #error OutputDir is required
#endif

#if AppArch == "arm64"
  #define AllowedArchitecture "arm64"
#else
  #define AllowedArchitecture "x64compatible"
#endif

[Setup]
AppId={{E6E0EF9A-8DBA-4B93-A964-7DA1BFC6B57A}
AppName=devingPanel
AppVersion={#AppVersion}
AppPublisher=devingPanel contributors
DefaultDirName={autopf}\devingPanel
DefaultGroupName=devingPanel
OutputDir={#OutputDir}
OutputBaseFilename=deving-panel_{#AppVersion}_windows_{#AppArch}_setup
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed={#AllowedArchitecture}
ArchitecturesInstallIn64BitMode={#AllowedArchitecture}
UninstallDisplayIcon={app}\deving-panel.exe

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\devingPanel"; Filename: "{app}\deving-panel.exe"
Name: "{autodesktop}\devingPanel"; Filename: "{app}\deving-panel.exe"; Tasks: desktopicon

[Tasks]
Name: desktopicon; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"
