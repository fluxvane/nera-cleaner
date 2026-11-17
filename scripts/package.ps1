# Windows: build nera-cleaner.exe and package it as a zip.
# (macOS: scripts/bundle.sh.)
#   scripts\package.ps1          # release build -> dist\NeraCleaner-<version>-windows-<arch>.zip
#   scripts\package.ps1 -Debug   # debug build, same layout (keeps the console window)
#
# The icon and version info are embedded by crates/nera-app/build.rs,
# so the .exe is self-contained: the zip holds just "Nera Cleaner.exe".
param([switch]$Debug)

$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

$profileDir = if ($Debug) { 'debug' } else { 'release' }
$cargoArgs = @('build', '-p', 'nera-app')
if (-not $Debug) { $cargoArgs += '--release' }
cargo @cargoArgs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$versionLine = Select-String -Path 'Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $versionLine) { throw 'No version found in Cargo.toml' }
$version = $versionLine.Matches[0].Groups[1].Value
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }

$name = "NeraCleaner-$version-windows-$arch"
$stage = Join-Path 'dist' $name
$archive = Join-Path 'dist' "$name.zip"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
if (Test-Path $archive) { Remove-Item -Force $archive }
New-Item -ItemType Directory -Force $stage | Out-Null

Copy-Item "target\$profileDir\nera-cleaner.exe" (Join-Path $stage 'Nera Cleaner.exe')
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $archive

Write-Output (Join-Path $stage 'Nera Cleaner.exe')
Write-Output $archive
