; =====================================================================
; Fukidashi MCP & Editor - Inno Setup Installer Script
; =====================================================================

#define MyAppName "Fukidashi"
#define MyAppVersion "1.0.0"
#define MyAppPublisher "Yozora"
#define MyAppURL "https://github.com/Kyokkei/fukidashi-mcp"
#define MyAppExeName "fukidashi-editor.exe"
#define MyAppMcpName "fukidashi-mcp.exe"

; Source directories
#define RepoDir "D:\coding\fukidashi-mcp"
#define ModelsDir "E:\Fukidashi\models"
#define RuntimeDllDir "E:\Fukidashi\runtime\onnxruntime-gpu-1.26-cu98-jit\python\onnxruntime\capi"

[Setup]
; Basic Application Info
AppId={{E5B20892-D8E9-44F3-9D27-66F53D75BF1C}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}

; Destination Folder (defaults to LocalAppData, but user can Browse... to D:, E:, etc.)
DefaultDirName={localappdata}\{#MyAppName}
DisableDirPage=no
UsePreviousAppDir=yes

; Standard Start Menu & Desktop
DefaultGroupName={#MyAppName}
AllowNoIcons=yes

; Privileges: 'lowest' allows installation without annoying Admin UAC prompt!
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog

; Output Configuration
OutputDir={#RepoDir}\dist
OutputBaseFilename=Fukidashi-Setup
SetupIconFile={#RepoDir}\assets\fukidashi.ico
UninstallDisplayIcon={app}\assets\fukidashi.ico

; Compression & Progress Bar
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
ShowLanguageDialog=no

; Architectures
ArchitecturesInstallIn64BitMode=x64

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "configure_clients"; Description: "Automatically configure AI Clients (Claude Desktop, Claude Code, Codex, Gemini)"; GroupDescription: "AI Client Integration:"; Flags: checkedonce
Name: "install_skill"; Description: "Install Comic Translation Skill (fukidashi-comic-translation)"; GroupDescription: "AI Client Integration:"; Flags: checkedonce
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}:"; Flags: checkedonce
Name: "addtopath"; Description: "Add Fukidashi to user PATH environment variable"; GroupDescription: "System Integration:"; Flags: checkedonce

[Files]
; --- Binaries & Helpers ---
Source: "{#RepoDir}\target\release\{#MyAppMcpName}"; DestDir: "{app}\bin"; Flags: ignoreversion
Source: "{#RepoDir}\target\release\{#MyAppExeName}"; DestDir: "{app}\bin"; Flags: ignoreversion
Source: "{#RepoDir}\assets\install-skills.cmd"; DestDir: "{app}\bin"; Flags: ignoreversion

; --- ONNX Runtime DLL ---
Source: "{#RuntimeDllDir}\onnxruntime.dll"; DestDir: "{app}\bin"; Flags: ignoreversion skipifsourcedoesntexist

; --- App Assets & Icon ---
Source: "{#RepoDir}\assets\fukidashi.ico"; DestDir: "{app}\assets"; Flags: ignoreversion
Source: "{#RepoDir}\assets\editor.html"; DestDir: "{app}\assets"; Flags: ignoreversion
Source: "{#RepoDir}\assets\fonts\*"; DestDir: "{app}\assets\fonts"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#RepoDir}\assets\editor-icons\*"; DestDir: "{app}\assets\editor-icons"; Flags: ignoreversion recursesubdirs createallsubdirs skipifsourcedoesntexist

; --- AI Skill Definition ---
Source: "{#RepoDir}\assets\fukidashi-comic-translation\SKILL.md"; DestDir: "{app}\skills\fukidashi-comic-translation"; Flags: ignoreversion

; --- Pre-trained ONNX Models (Detection, Inpainting, OCR) ---
Source: "{#ModelsDir}\*"; DestDir: "{app}\models"; Flags: ignoreversion recursesubdirs createallsubdirs skipifsourcedoesntexist

[Icons]
; Start Menu Shortcuts
Name: "{autoprograms}\{#MyAppName}\Fukidashi Editor"; Filename: "{app}\bin\{#MyAppExeName}"; IconFilename: "{app}\assets\fukidashi.ico"
Name: "{autoprograms}\{#MyAppName}\Uninstall Fukidashi"; Filename: "{uninstallexe}"; IconFilename: "{app}\assets\fukidashi.ico"

; Desktop Shortcut (if selected)
Name: "{autodesktop}\Fukidashi Editor"; Filename: "{app}\bin\{#MyAppExeName}"; IconFilename: "{app}\assets\fukidashi.ico"; Tasks: desktopicon

[Registry]
; Add {app}\bin to User PATH if selected
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}\bin"; Tasks: addtopath; Check: NeedsAddPath(ExpandConstant('{app}\bin'))

[Run]
; 1. Anchor storage_root to the installed directory so models & jobs stay on chosen drive
Filename: "{app}\bin\{#MyAppMcpName}"; Parameters: "config-set --storage-root ""{app}"""; Flags: runhidden; StatusMsg: "Configuring storage paths..."

; 2. Automatically register MCP server in all detected AI clients
Filename: "{app}\bin\{#MyAppMcpName}"; Parameters: "install --all"; Flags: runhidden; Tasks: configure_clients; StatusMsg: "Configuring Claude Desktop, Claude Code, Codex, and Gemini..."

; 3. Deploy comic translation skill to AI skill directories
Filename: "{app}\bin\install-skills.cmd"; Flags: runhidden; Tasks: install_skill; StatusMsg: "Installing AI comic translation skills..."



[UninstallRun]
; Cleanly unregister from all AI clients before deleting files
Filename: "{app}\bin\{#MyAppMcpName}"; Parameters: "uninstall --all"; Flags: runhidden

[Code]
// Helper function to check if {app}\bin is already in HKCU PATH
function NeedsAddPath(Param: string): boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', OrigPath)
  then begin
    Result := True;
    exit;
  end;
  // Check if already in PATH
  Result := Pos(';' + UpperCase(Param) + ';', ';' + UpperCase(OrigPath) + ';') = 0;
end;

