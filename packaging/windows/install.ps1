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
# Where RDM registers the browser extension's connector (native messaging host) at start.
$nativeHosts = @(
    'Google\Chrome', 'Chromium', 'Microsoft\Edge', 'BraveSoftware\Brave-Browser', 'Vivaldi',
    'Mozilla', 'Waterfox', 'LibreWolf'
) | ForEach-Object { "HKCU:\Software\$_\NativeMessagingHosts\rdm.bridge" }

$exe = @("$PSScriptRoot\rdm.exe", "$PSScriptRoot\..\..\target\release\rdm.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1

# A running RDM closes cleanly first (downloads saved), asked by the new binary — an older one
# would not know the option; whatever is left (browser connectors included) is ended.
if ($exe) { & $exe --quit | Out-Null }
# Another account's RDM (fast user switching) cannot be ended from here: not a reason to stop.
# Only RDM's own copies (the installed one, the one being installed): another program that happens
# to be named rdm.exe is left alone.
$ours = @((Join-Path $dir 'rdm.exe'), $exe) | Where-Object { $_ } | ForEach-Object { [IO.Path]::GetFullPath($_) }
Get-Process rdm -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -and ($ours -contains [IO.Path]::GetFullPath($_.Path)) } |
    Stop-Process -Force -ErrorAction SilentlyContinue

if ($Uninstall) {
    Remove-Item (@($dir) + $shortcuts) -Recurse -Force -ErrorAction SilentlyContinue
    Remove-ItemProperty $runKey -Name RDM -ErrorAction SilentlyContinue
    Remove-Item 'HKCU:\Software\Classes\AppUserModelId\RDM.DownloadManager' -Recurse -ErrorAction SilentlyContinue
    Remove-Item $nativeHosts -Recurse -ErrorAction SilentlyContinue
    Write-Output 'RDM uninstalled.'
    return
}

if (-not $exe) { throw 'rdm.exe not found: run "cargo build --release" or put rdm.exe next to this script.' }

New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item $exe (Join-Path $dir 'rdm.exe') -Force

$shell = New-Object -ComObject WScript.Shell
foreach ($path in $shortcuts) {
    $link = $shell.CreateShortcut($path)
    $link.TargetPath = Join-Path $dir 'rdm.exe'
    $link.WorkingDirectory = $dir
    $link.Save()
}

Write-Output "RDM installed in $dir (Start menu and Desktop: RDM)."
