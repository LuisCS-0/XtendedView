; Instalador de XtendedView para Windows (Inno Setup 6).
; Se genera con tools\instalador.ps1, que antes compila la versión sin consola.
;
; Instala en Archivos de programa (con permiso de administrador, como cualquier programa) y
; permite las conexiones entrantes en el cortafuegos. Los certificados, el código y los ajustes
; de cada usuario van a %LOCALAPPDATA%\XtendedView: al desinstalar se pregunta si borrarlos.

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#define Root ".."
#define Exe "xtendedview.exe"

[Setup]
AppId={{6B0C1C3E-7F5A-4E2B-9C7D-58A2B3E4F901}
AppName=XtendedView
AppVersion={#AppVersion}
AppVerName=XtendedView {#AppVersion}
AppPublisher=XtendedView
AppComments=Usa un móvil, una tablet u otro dispositivo con navegador como pantalla de este ordenador.
DefaultDirName={autopf}\XtendedView
DefaultGroupName=XtendedView
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#Root}\dist
OutputBaseFilename=XtendedView-{#AppVersion}-instalador
SetupIconFile={#Root}\assets\xtendedview.ico
UninstallDisplayIcon={app}\{#Exe}
UninstallDisplayName=XtendedView
WizardStyle=modern
WizardSizePercent=110
Compression=lzma2/max
SolidCompression=yes
CloseApplications=no

[Languages]
Name: "es"; MessagesFile: "compiler:Languages\Spanish.isl"

[Tasks]
Name: "desktopicon"; Description: "Crear un acceso directo en el escritorio"; GroupDescription: "Accesos directos:"
Name: "autostart"; Description: "Abrir XtendedView al iniciar sesión (se queda junto al reloj, sin abrir el panel)"; GroupDescription: "Inicio:"; Flags: unchecked

[Files]
Source: "{#Root}\host-rs\target\gui\release\{#Exe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\web\*"; DestDir: "{app}\web"; Flags: ignoreversion recursesubdirs
Source: "{#Root}\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
; Para registrar resoluciones exactas del monitor virtual (lo lanza el panel, como administrador).
Source: "{#Root}\tools\vdd-resoluciones.ps1"; DestDir: "{app}\tools"; Flags: ignoreversion

[Icons]
; Acceso directo suelto en el menú Inicio (en una carpeta, Windows lo enseñaría con icono de
; carpeta). Se desinstala desde Configuración → Aplicaciones, como cualquier programa.
Name: "{autoprograms}\XtendedView"; Filename: "{app}\{#Exe}"; IconFilename: "{app}\{#Exe}"; Comment: "Usa otro dispositivo como pantalla de este ordenador"
Name: "{autodesktop}\XtendedView"; Filename: "{app}\{#Exe}"; IconFilename: "{app}\{#Exe}"; Tasks: desktopicon

[InstallDelete]
; La carpeta «XtendedView» del menú Inicio de la versión anterior del instalador.
Type: filesandordirs; Name: "{autoprograms}\XtendedView"

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "XtendedView"; \
  ValueData: """{app}\{#Exe}"" --no-panel"; Flags: uninsdeletevalue; Tasks: autostart

[Run]
; Los dispositivos se conectan a este ordenador. Todos los perfiles: la red propia de Windows
; aparece como red pública. El propio programa solo acepta conexiones con la pantalla encendida,
; por las conexiones activas, con el código de emparejamiento y aceptándolas en el panel.
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""XtendedView"""; Flags: runhidden
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""XtendedView"" dir=in action=allow program=""{app}\{#Exe}"" enable=yes profile=any"; \
  Flags: runhidden; StatusMsg: "Permitiendo las conexiones en el cortafuegos…"
Filename: "{app}\{#Exe}"; Description: "Abrir XtendedView"; Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/IM {#Exe} /F"; Flags: runhidden; RunOnceId: "Cerrar"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""XtendedView"""; Flags: runhidden; RunOnceId: "Cortafuegos"

[Code]
// Si está abierto, se cierra antes de sustituir el ejecutable (al actualizar).
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/IM {#Exe} /F', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Result := '';
end;

// Al desinstalar: los datos del usuario (certificados, código y ajustes) se borran solo si lo pide.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Data: String;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    Data := ExpandConstant('{localappdata}\XtendedView');
    if DirExists(Data) and (MsgBox('¿Borrar también los ajustes y certificados de XtendedView?' + #13#10#13#10 +
        'Si vas a volver a instalarlo, consérvalos: así no tendrás que instalar de nuevo el certificado en tus dispositivos.',
        mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES) then
      DelTree(Data, True, True, True);
  end;
end;
