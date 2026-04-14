# PowerShell script to remove the onair startup shortcut.
#
# Usage:
#   .\uninstall-startup.ps1

$startupDir = [Environment]::GetFolderPath("Startup")
$shortcutPath = Join-Path $startupDir "onair.lnk"

if (Test-Path $shortcutPath) {
    Remove-Item $shortcutPath
    Write-Host "Removed:  $shortcutPath" -ForegroundColor Green
} else {
    Write-Host "No shortcut found at:  $shortcutPath" -ForegroundColor Yellow
}
