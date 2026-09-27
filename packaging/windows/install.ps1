# Per-user install (no admin): .\install.ps1 [-Uninstall]
param([switch]$Uninstall)
$ErrorActionPreference = 'Stop'

$dir = Join-Path $env:LOCALAPPDATA 'Programs\RDM'
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'RDM.lnk'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'

Get-Process rdm -ErrorAction SilentlyContinue | Stop-Process -Force

if ($Uninstall) {
    Remove-Item $dir, $shortcut -Recurse -Force -ErrorAction SilentlyContinue
    Remove-ItemProperty $runKey -Name RDM -ErrorAction SilentlyContinue
    Remove-Item 'HKCU:\Software\Classes\AppUserModelId\RDM.DownloadManager' -Recurse -ErrorAction SilentlyContinue
    Write-Output 'RDM désinstallé.'
    return
}

$exe = @("$PSScriptRoot\rdm.exe", "$PSScriptRoot\..\..\target\release\rdm.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $exe) { throw 'rdm.exe introuvable : lancez "cargo build --release" ou placez rdm.exe à côté de ce script.' }

New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item $exe (Join-Path $dir 'rdm.exe') -Force

$link = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcut)
$link.TargetPath = Join-Path $dir 'rdm.exe'
$link.WorkingDirectory = $dir
$link.Save()

Write-Output "RDM installé dans $dir (menu Démarrer : RDM)."
