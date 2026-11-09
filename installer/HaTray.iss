; HaTray - script Inno Setup 6  (https://jrsoftware.org/isdl.php, gratuit)
; Compilation : ouvrir ce fichier dans Inno Setup puis Build > Compile,
;               ou en ligne de commande : iscc installer\HaTray.iss
; Pré-requis  : avoir lancé « cargo build --release » (l'exécutable est lu dans target\release).

#define AppName    "HaTray"
; La version est imposee par /DAppVersion=x.y.z lors d'une release (voir release.yml).
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#define AppExe     "ha-tray.exe"

[Setup]
AppId={{6F0B3E52-9C1A-4B6E-8E57-2D4A7C91B0A3}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppName}
; Installation par utilisateur : aucun droit administrateur, aucune écriture dans Program Files.
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\{#AppName}
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=output
OutputBaseFilename=HaTray-Setup
Compression=lzma2/ultra
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\{#AppExe}
; Icône de l'assistant d'installation
SetupIconFile=..\assets\icon.ico
; ZÉRO RÉSIDU : l'installateur n'écrit pas l'entrée de désinstallation dans le registre.
; Conséquence : HaTray n'apparaît pas dans « Paramètres > Applications ». La désinstallation
; se fait par le raccourci « Désinstaller HaTray » du menu Démarrer (ou unins000.exe).
; Pour retrouver l'entrée classique dans Windows, remplacer « no » par « yes ».
CreateUninstallRegKey=no

[Languages]
Name: "french"; MessagesFile: "compiler:Languages\French.isl"

[Tasks]
; Cases proposées dans l'assistant d'installation.
Name: "desktopicon"; Description: "Créer une icône sur le Bureau"; GroupDescription: "Icônes supplémentaires :"
Name: "autostart"; Description: "Lancer {#AppName} au démarrage de Windows"; GroupDescription: "Options :"; Flags: unchecked

[Files]
Source: "..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autoprograms}\Désinstaller {#AppName}"; Filename: "{uninstallexe}"
; Même emplacement et même nom que l'option « Lancer au démarrage » de l'application.
Name: "{userstartup}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: autostart

[Run]
Filename: "{app}\{#AppExe}"; Description: "Lancer {#AppName} maintenant"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Supprime aussi le raccourci de démarrage s'il a été créé depuis les paramètres de l'application.
Type: files; Name: "{userstartup}\{#AppName}.lnk"

[Code]
procedure StopApp();
var
  ResultCode: Integer;
begin
  { Ferme HaTray s'il tourne (installation par-dessus, désinstallation). }
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExe}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopApp();
  Result := '';
end;

function InitializeUninstall(): Boolean;
begin
  StopApp();
  Result := True;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Dir: String;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    Dir := ExpandConstant('{userappdata}\{#AppName}');
    if (not UninstallSilent()) and DirExists(Dir) then
    begin
      if MsgBox('Supprimer aussi la configuration (URL et jeton chiffré) ?' + #13#10 + Dir,
                mbConfirmation, MB_YESNO) = IDYES then
        DelTree(Dir, True, True, True);
    end;
  end;
end;
