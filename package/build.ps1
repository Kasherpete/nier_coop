# Builds the release DLL and makes dist\nier_coop.zip (the zip to send to a friend).
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = Join-Path $root 'target' }
Push-Location $root
# cmd /c: Windows PowerShell treats cargo's progress on stderr as an error.
try { cmd /c 'cargo build --release --target x86_64-pc-windows-msvc 2>&1'; if ($LASTEXITCODE) { throw 'cargo build failed' } } finally { Pop-Location }

$out = Join-Path $root 'dist\nier_coop'
Remove-Item (Join-Path $root 'dist') -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $out | Out-Null
# The game loads dinput8.dll from its own folder at startup, so the mod needs no injector.
Copy-Item (Join-Path $env:CARGO_TARGET_DIR 'x86_64-pc-windows-msvc\release\nier_coop.dll') (Join-Path $out 'dinput8.dll')
Copy-Item (Join-Path $PSScriptRoot '*') $out -Exclude 'build.ps1'
Compress-Archive -Path $out -DestinationPath (Join-Path $root 'dist\nier_coop.zip') -Force
Write-Host "Made $(Join-Path $root 'dist\nier_coop.zip')"
