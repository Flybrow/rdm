# Firefox flavour of the extension: .\build.ps1 [-Out <dir>]  (default: target\firefox)
# Same as build.sh: Chrome/Brave load extension\ as is (background service worker); Firefox needs
# `background.scripts`, which Chrome flags as an error, hence this derived copy.
param([string]$Out)
$ErrorActionPreference = 'Stop'

$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$src = Join-Path $root 'extension'
if (-not $Out) { $Out = Join-Path $root 'target\firefox' }

if (Test-Path $Out) { Remove-Item $Out -Recurse -Force }
New-Item -ItemType Directory -Force $Out | Out-Null
Copy-Item (Join-Path $src '*') $Out -Recurse -Force
Remove-Item (Join-Path $Out 'test') -Recurse -Force -ErrorAction SilentlyContinue

$manifest = [IO.File]::ReadAllText((Join-Path $src 'manifest.json'))
$manifest = $manifest.Replace('"service_worker": "background.js"', '"scripts": ["background.js"]')
$manifest = $manifest -replace '(?m)^  "(key|minimum_chrome_version)":.*\r?\n', ''
if (-not $manifest.Contains('"scripts": ["background.js"]')) { throw 'background entry not found in extension\manifest.json' }
# UTF-8 without BOM, like the source.
[IO.File]::WriteAllText((Join-Path $Out 'manifest.json'), $manifest, (New-Object Text.UTF8Encoding $false))

Write-Output "Firefox extension: $Out"
