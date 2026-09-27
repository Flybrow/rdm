# Builds RDM-<version>-x64.msi (per-user installer) at the repository root.
#   powershell -ExecutionPolicy Bypass -File packaging\windows\build-msi.ps1 [-NoBuild]
# Needs WiX 3 (candle.exe / light.exe): on PATH, in "WiX Toolset v3.x", or %LOCALAPPDATA%\Programs\wix3.
param([switch]$NoBuild)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
Set-Location $root

$candle = @(
    (Get-Command candle.exe -ErrorAction SilentlyContinue).Source,
    (Get-ChildItem "${env:ProgramFiles(x86)}\WiX Toolset v3*\bin\candle.exe" -ErrorAction SilentlyContinue | Select-Object -Last 1).FullName,
    (Join-Path $env:LOCALAPPDATA 'Programs\wix3\candle.exe')
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $candle) { throw 'WiX 3 introuvable (candle.exe).' }
$bin = Split-Path $candle

$manifest = Get-Content Cargo.toml -Raw
$version = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
$repository = [regex]::Match($manifest, '(?m)^repository\s*=\s*"([^"]+)"').Groups[1].Value
if (-not $NoBuild) { cargo build --release -p rdm; if ($LASTEXITCODE) { throw 'cargo build a échoué' } }

$obj = Join-Path $root 'target\wix'
New-Item -ItemType Directory -Force $obj | Out-Null
$out = Join-Path $root "RDM-$version-x64.msi"
& "$bin\candle.exe" -nologo -arch x64 -out "$obj\rdm.wixobj" `
    "-dVersion=$version" "-dRepository=$repository" "-dExe=$root\target\release\rdm.exe" "-dAssets=$root\crates\app\assets" `
    packaging\windows\rdm.wxs
if ($LASTEXITCODE) { throw 'candle a échoué' }
& "$bin\light.exe" -nologo -sice:ICE91 -cultures:fr-FR -out $out "$obj\rdm.wixobj"
if ($LASTEXITCODE) { throw 'light a échoué' }
Write-Output "MSI : $out"
