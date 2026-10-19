#!/usr/bin/env pwsh
param([ValidateSet('windows')][string]$Target = 'windows')
$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot
& (Join-Path $PSScriptRoot 'build-windows.ps1')
$Manifest = Get-Content Cargo.toml -Raw
$Version = [regex]::Match($Manifest, '(?ms)^\[package\]\s+.*?^version = "([0-9]+\.[0-9]+\.[0-9]+)"').Groups[1].Value
if (-not $Version) { throw 'Missing package version' }
$BuildRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $RepoRoot 'target' }
$Build = Join-Path $BuildRoot 'x86_64-pc-windows-msvc/release'
$Staging = Join-Path ([IO.Path]::GetTempPath()) ('werkWorker-package-' + [guid]::NewGuid().ToString('N'))
try {
    New-Item $Staging -ItemType Directory | Out-Null
    foreach ($Binary in @('werk-worker.exe', 'ww.exe')) {
        & (Join-Path $Build $Binary) --version
        if ($LASTEXITCODE -ne 0) { throw "Binary smoke failed: $Binary" }
        Copy-Item (Join-Path $Build $Binary) $Staging
    }
    python scripts/generate-cli-reference.py (Join-Path $Build 'werk-worker.exe') --output-dir (Join-Path $Staging 'reference')
    if ($LASTEXITCODE -ne 0) { throw 'CLI reference validation failed' }
    foreach ($Name in @('README.md', 'LICENSE', 'NOTICE')) { Copy-Item $Name $Staging }
    New-Item releases -ItemType Directory -Force | Out-Null
    $Name = "werkWorker-v$Version-windows-x86_64.zip"
    $Artifact = Join-Path $RepoRoot "releases/$Name"
    $Files = @('werk-worker.exe', 'ww.exe', 'README.md', 'LICENSE', 'NOTICE') | ForEach-Object { Join-Path $Staging $_ }
    Compress-Archive -Path $Files -DestinationPath $Artifact -Force
    $Hash = (Get-FileHash $Artifact -Algorithm SHA256).Hash.ToLowerInvariant()
    "$Hash  $Name" | Set-Content "$Artifact.sha256" -Encoding ascii
    Write-Host "Created $Artifact and its .sha256"
} finally {
    Remove-Item $Staging -Recurse -Force -ErrorAction SilentlyContinue
}
