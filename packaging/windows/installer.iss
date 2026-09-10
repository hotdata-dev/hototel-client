; hotusage collector - Windows installer (Inno Setup)
; Installs per-user (no admin), registers the Run-key autostart via the
; binary's own `install` subcommand, and unregisters on uninstall.

#ifndef Version
  #define Version "0.0.0"
#endif

[Setup]
AppName=hotusage collector
AppId=dev.hotdata.hotusage-collector
AppVersion={#Version}
AppPublisher=hotdata
DefaultDirName={userpf}\hotusage-collector
PrivilegesRequired=lowest
DisableProgramGroupPage=yes
DisableDirPage=yes
OutputBaseFilename=hotusage-collector-setup
Compression=lzma2
SolidCompression=yes
ArchitecturesInstallIn64BitMode=x64compatible

[Files]
Source: "hotusage-collector.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\hotusage collector"; Filename: "{app}\hotusage-collector.exe"

[Run]
; register the Run-key autostart, then start the tray app now
Filename: "{app}\hotusage-collector.exe"; Parameters: "install"; Flags: runhidden
Filename: "{app}\hotusage-collector.exe"; Description: "Start hotusage collector"; Flags: postinstall nowait skipifsilent

[UninstallRun]
Filename: "{app}\hotusage-collector.exe"; Parameters: "uninstall"; Flags: runhidden; RunOnceId: "unregister"
