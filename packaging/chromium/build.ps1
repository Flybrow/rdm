# Chromium flavour of the extension (Chrome, Brave, Opera, Edge, Vivaldi, Chromium):
#   .\build.ps1 [-Out <dir>]  (default: target\chromium)
# Same as build.sh: the folder loads as is ("Charger l'extension non empaquetée"), and
# rdm-chromium.zip next to it is that folder as one file. RDM itself installs the extension in one
# click (the extension window) and keeps it up to date.
param([string]$Out)
$ErrorActionPreference = 'Stop'

$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$src = Join-Path $root 'extension'
if (-not $Out) { $Out = Join-Path $root 'target\chromium' }

if (Test-Path $Out) { Remove-Item $Out -Recurse -Force }
New-Item -ItemType Directory -Force $Out | Out-Null
Copy-Item (Join-Path $src '*') $Out -Recurse -Force
Remove-Item (Join-Path $Out 'test') -Recurse -Force -ErrorAction SilentlyContinue

$manifest = [IO.File]::ReadAllText((Join-Path $Out 'manifest.json'))
if (-not $manifest.Contains('"service_worker": "background.js"')) { throw 'background service worker not found in extension\manifest.json' }

$zip = Join-Path (Split-Path $Out -Parent) 'rdm-chromium.zip'
if (Test-Path $zip) { Remove-Item $zip -Force }
Add-Type -AssemblyName System.IO.Compression.FileSystem
[IO.Compression.ZipFile]::CreateFromDirectory($Out, $zip)

Write-Output "Chromium extension: $Out (+ $zip)"
