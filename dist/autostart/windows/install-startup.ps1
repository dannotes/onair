# PowerShell script to install onair as a Windows startup item.
#
# Drops a shortcut into the Startup folder, so onair launches on every login.
# Does NOT require admin rights.
#
# Usage (in PowerShell):
#   .\install-startup.ps1
#
# Uninstall:
#   .\uninstall-startup.ps1
#   (or just delete the shortcut from %APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\)

$ErrorActionPreference = "Stop"

# Resolve the full path to onair.exe on PATH.
$onairExe = (Get-Command onair.exe -ErrorAction SilentlyContinue).Source
if (-not $onairExe) {
    Write-Host "onair.exe not found on PATH." -ForegroundColor Red
    Write-Host "Install it first via Scoop:  scoop install onair" -ForegroundColor Yellow
    Write-Host "Or download it from https://github.com/dannotes/onair/releases" -ForegroundColor Yellow
    exit 1
}

$startupDir = [Environment]::GetFolderPath("Startup")
$shortcutPath = Join-Path $startupDir "onair.lnk"

$WshShell = New-Object -ComObject WScript.Shell
$Shortcut = $WshShell.CreateShortcut($shortcutPath)
$Shortcut.TargetPath = $onairExe
$Shortcut.WorkingDirectory = Split-Path $onairExe
$Shortcut.WindowStyle = 7  # Minimized
$Shortcut.Description = "onair — Teams presence bulb"
$Shortcut.Save()

Write-Host "Installed startup shortcut:" -ForegroundColor Green
Write-Host "  $shortcutPath"
Write-Host ""
Write-Host "onair will now launch on every login. Open http://localhost:9876 to configure."
