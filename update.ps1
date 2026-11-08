# update.ps1 - compile HaTray puis met a jour la version installee, en une commande.
# (ASCII uniquement : PowerShell 5.1 lit mal les accents sans BOM.)

param(
    [string]$InstallDir = "$env:LOCALAPPDATA\Programs\HaTray"
)

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

Write-Host "1/4 Compilation..." -ForegroundColor Cyan
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "La compilation a echoue : rien n'a ete modifie." }

Write-Host "2/4 Arret de HaTray..." -ForegroundColor Cyan
Get-Process ha-tray -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

Write-Host "3/4 Copie dans $InstallDir ..." -ForegroundColor Cyan
if (-not (Test-Path $InstallDir)) { New-Item -ItemType Directory -Path $InstallDir | Out-Null }
Copy-Item "target\release\ha-tray.exe" "$InstallDir\ha-tray.exe" -Force

Write-Host "4/4 Relance..." -ForegroundColor Cyan
Start-Process "$InstallDir\ha-tray.exe"

Write-Host "Mise a jour terminee." -ForegroundColor Green
