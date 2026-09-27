# Per-user install without the MSI (no admin): .\install.ps1 [-Uninstall]
# Same places as the MSI: %LOCALAPPDATA%\Programs\RDM, Start menu and Desktop shortcuts.
param([switch]$Uninstall)
$ErrorActionPreference = 'Stop'

$dir = Join-Path $env:LOCALAPPDATA 'Programs\RDM'
$shortcuts = @(
    (Join-Path ([Environment]::GetFolderPath('Programs')) 'RDM.lnk'),
    (Join-Path ([Environment]::GetFolderPath('Desktop')) 'RDM.lnk')
)
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'

$exe = @("$PSScriptRoot\rdm.exe", "$PSScriptRoot\..\..\target\release\rdm.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1

# A running RDM closes cleanly first (downloads saved), asked by the new binary — an older one
# would not know the option; whatever is left is ended.
if ($exe) { & $exe --quit | Out-Null }
Get-Process rdm -ErrorAction SilentlyContinue | Stop-Process -Force

if ($Uninstall) {
    Remove-Item (@($dir) + $shortcuts) -Recurse -Force -ErrorAction SilentlyContinue
    Remove-ItemProperty $runKey -Name RDM -ErrorAction SilentlyContinue
    Remove-Item 'HKCU:\Software\Classes\AppUserModelId\RDM.DownloadManager' -Recurse -ErrorAction SilentlyContinue
    Write-Output 'RDM désinstallé.'
    return
}

if (-not $exe) { throw 'rdm.exe introuvable : lancez "cargo build --release" ou placez rdm.exe à côté de ce script.' }

New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item $exe (Join-Path $dir 'rdm.exe') -Force

$shell = New-Object -ComObject WScript.Shell
foreach ($path in $shortcuts) {
    $link = $shell.CreateShortcut($path)
    $link.TargetPath = Join-Path $dir 'rdm.exe'
    $link.WorkingDirectory = $dir
    $link.Save()
}

Write-Output "RDM installé dans $dir (menu Démarrer et Bureau : RDM)."
