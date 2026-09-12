# =====================================================================
# Fukidashi MCP & Editor - Automated Installer Build Script
# =====================================================================

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $ScriptDir

Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host "   Fukidashi MCP & Editor - Building Windows Installer    " -ForegroundColor Cyan
Write-Host "==========================================================" -ForegroundColor Cyan

# 1. Locate Inno Setup Compiler (ISCC.exe)
$iscc = $null
$cmd = Get-Command iscc -ErrorAction SilentlyContinue
if ($cmd) {
    $iscc = $cmd.Source
}

if (-not $iscc) {
    $searchPaths = @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
        "${env:LOCALAPPDATA}\Programs\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles(x86)}\Inno Setup 7\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 7\ISCC.exe"
    )
    foreach ($p in $searchPaths) {
        if (Test-Path $p) {
            $iscc = $p
            break
        }
    }
}

if (-not $iscc) {
    Write-Host "Inno Setup compiler (ISCC.exe) not found on system." -ForegroundColor Yellow
    Write-Host "Attempting to install Inno Setup via winget..." -ForegroundColor Cyan
    winget install --id JRSoftware.InnoSetup -e --silent --accept-package-agreements --accept-source-agreements
    
    # Re-check paths after install
    $searchPaths = @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
        "${env:LOCALAPPDATA}\Programs\Inno Setup 6\ISCC.exe"
    )
    foreach ($p in $searchPaths) {
        if (Test-Path $p) {
            $iscc = $p
            break
        }
    }
    
    if (-not $iscc) {
        Write-Error "Please restart your shell or manually install Inno Setup 6 from: https://jrsoftware.org/isinfo.php"
        exit 1
    }
}

Write-Host "Found Inno Setup Compiler at: $iscc" -ForegroundColor Green

# 2. Verify Release Binaries
$mcpExe = Join-Path $ScriptDir "target\release\fukidashi-mcp.exe"
$editorExe = Join-Path $ScriptDir "target\release\fukidashi-editor.exe"
if (-not (Test-Path $mcpExe) -or -not (Test-Path $editorExe)) {
    Write-Host "Building release binaries with cargo..." -ForegroundColor Cyan
    cargo build --release --features editor
} else {
    Write-Host "Release binaries verified." -ForegroundColor Green
}

# 3. Ensure Output Directory
$distDir = Join-Path $ScriptDir "dist"
if (-not (Test-Path $distDir)) {
    New-Item -ItemType Directory -Path $distDir | Out-Null
}

# 4. Compile Installer
Write-Host "Compiling installer package (this may take a minute to compress ONNX models)..." -ForegroundColor Cyan
& $iscc "$ScriptDir\installer.iss"

if ($LASTEXITCODE -eq 0) {
    $setupExe = Join-Path $distDir "Fukidashi-Setup.exe"
    if (Test-Path $setupExe) {
        $sizeMB = [math]::Round(((Get-Item $setupExe).Length / 1MB), 2)
        Write-Host "==========================================================" -ForegroundColor Green
        Write-Host " SUCCESS! Installer generated at:" -ForegroundColor Green
        Write-Host " $setupExe ($sizeMB MB)" -ForegroundColor Yellow
        Write-Host "==========================================================" -ForegroundColor Green
    }
} else {
    Write-Error "Inno Setup compilation failed with exit code $LASTEXITCODE."
}
