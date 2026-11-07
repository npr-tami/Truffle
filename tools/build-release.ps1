# Builds truffle.exe for a release, without this PC's folder names inside it.
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File tools\build-release.ps1 [-TargetDir <folder>]
# The exe ends up in <TargetDir>\release\truffle.exe (default: target\dist\release).
param([string]$TargetDir = (Join-Path (Split-Path $PSScriptRoot) 'target\dist'))

$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }
$TargetDir = [IO.Path]::GetFullPath($TargetDir)

# Panic messages carry source paths; map the local folders to neutral names.
$env:RUSTFLAGS = @(
    "--remap-path-prefix=$cargoHome=cargo"
    "--remap-path-prefix=$rustupHome=rustup"
    "--remap-path-prefix=$TargetDir=target"
    "--remap-path-prefix=$repo=truffle"
) -join ' '

Push-Location $repo
try {
    cargo build --release --target-dir $TargetDir
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
} finally {
    Pop-Location
}

$exe = Join-Path $TargetDir 'release\truffle.exe'
$bytes = [IO.File]::ReadAllBytes($exe)
$text = [Text.Encoding]::ASCII.GetString($bytes)
foreach ($path in @($env:USERPROFILE, $env:USERNAME)) {
    if ($path -and $text.Contains($path)) { throw "truffle.exe still contains '$path'" }
}
Write-Host "Built $exe ($([math]::Round($bytes.Length / 1MB, 1)) MB)"
