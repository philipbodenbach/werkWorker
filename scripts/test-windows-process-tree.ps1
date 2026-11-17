#!/usr/bin/env pwsh
param(
    [Parameter(Mandatory = $true)][string]$EvidenceRoot,
    [Parameter(Mandatory = $true)][string]$BindAddress,
    [string]$Archive
)
$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $PSScriptRoot
$Identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$Principal = [Security.Principal.WindowsPrincipal]::new($Identity)
if (-not $Principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'An already elevated administrator token is required for fresh test provisioning.'
}
$EvidenceRoot = [IO.Path]::GetFullPath($EvidenceRoot)
if (Test-Path -LiteralPath $EvidenceRoot) { throw 'Choose a new evidence directory; previous installations are never reused.' }
New-Item -ItemType Directory -Path $EvidenceRoot | Out-Null
$Utf8 = [Text.UTF8Encoding]::new($false)
function Write-JsonFile($Path, $Value) {
    [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 20) + "`n", $Utf8)
}
$Python = (Get-Command python.exe -CommandType Application -ErrorAction Stop).Source
if (-not $Archive) {
    # Diagnostic source snapshots must never share the product's binary output.
    # Cargo freshness records alone do not prove a shared top-level EXE's origin.
    $TreePreviousCargoTargetDir = $env:CARGO_TARGET_DIR
    try {
        $env:CARGO_TARGET_DIR = Join-Path $EvidenceRoot 'product-build'
        & (Join-Path $PSScriptRoot 'package-release.ps1')
        if ($LASTEXITCODE -ne 0) { throw 'Native package build failed.' }
    } finally {
        [Environment]::SetEnvironmentVariable('CARGO_TARGET_DIR', $TreePreviousCargoTargetDir, 'Process')
    }
    $Manifest = Get-Content -LiteralPath (Join-Path $RepoRoot 'Cargo.toml') -Raw
    $Version = [regex]::Match($Manifest, '(?ms)^\[package\]\s+.*?^version = "([0-9]+\.[0-9]+\.[0-9]+)"').Groups[1].Value
    if (-not $Version) { throw 'Missing stable package version.' }
    $Archive = Join-Path $RepoRoot "releases/werkWorker-v$Version-windows-x86_64.zip"
}
$Archive = (Resolve-Path -LiteralPath $Archive).Path
$Installation = Join-Path $EvidenceRoot 'installation'
& $Python (Join-Path $PSScriptRoot 'smoke-install.py') $Archive --retain $Installation
if ($LASTEXITCODE -ne 0) { throw 'Fresh package installation failed.' }
$RustRoot = (& rustc --print sysroot)
if ($LASTEXITCODE -ne 0) { throw 'Cannot resolve the native Rust sysroot.' }
if (-not $env:VCToolsInstallDir -or -not $env:WindowsSdkDir -or -not $env:WindowsSDKVersion) {
    throw 'Run in the x64 MSVC developer environment with a Windows SDK.'
}
$SdkVersion = $env:WindowsSDKVersion.TrimEnd('\', '/')
$Sources = @(
    $RustRoot,
    (Join-Path $env:VCToolsInstallDir 'bin/Hostx64/x64'),
    (Join-Path $env:VCToolsInstallDir 'lib/x64'),
    (Join-Path $env:WindowsSdkDir "Lib/$SdkVersion/um/x64"),
    (Join-Path $env:WindowsSdkDir "Lib/$SdkVersion/ucrt/x64")
)
foreach ($Source in $Sources) {
    if (-not (Test-Path -LiteralPath $Source -PathType Container)) { throw "Missing explicit toolchain input: $Source" }
}
$InstalledBin = Join-Path $Installation 'bin'
$Runtime = Join-Path $EvidenceRoot 'runtime'
$Request = Join-Path $EvidenceRoot 'setup-request.json'
Write-JsonFile $Request @{
    provisioning_root = $Runtime
    workspace = (Join-Path $EvidenceRoot 'workspace')
    runner_source = (Join-Path $InstalledBin 'ww-windows-runner.exe')
    host_sid = $Identity.User.Value
    toolchain_sources = $Sources
    toolchain_environment = @{
        PATH = '{toolchain0}\bin;{toolchain1}'
        RUSTC = '{toolchain0}\bin\rustc.exe'
        CARGO_HOME = '{temp}\cargo'
        LIB = '{toolchain2};{toolchain3};{toolchain4}'
    }
}
& (Join-Path $InstalledBin 'ww-windows-provision.exe') --request $Request
if ($LASTEXITCODE -ne 0) { throw 'Fresh provisioning from the installed helpers failed.' }
$Provision = Join-Path $Runtime 'provisioning.json'
$Acceptance = Join-Path $PSScriptRoot 'validate-windows-process-tree.py'
& $Python $Acceptance suite --source $RepoRoot --manifest $Provision --evidence (Join-Path $EvidenceRoot 'suite') --bind-address $BindAddress
if ($LASTEXITCODE -ne 0) { throw 'The mandatory native process-tree suite failed; the profile is not qualified.' }
# The suite deliberately retains its crash receipt. Use another freshly
# provisioned installation workspace; never erase that evidence to admit a run.
$InstalledRequest = Get-Content -LiteralPath $Request -Raw | ConvertFrom-Json
$InstalledRequest.provisioning_root = Join-Path $EvidenceRoot 'installed-runtime'
$InstalledRequest.workspace = Join-Path $EvidenceRoot 'installed-workspace'
$InstalledRequestFile = Join-Path $EvidenceRoot 'installed-setup-request.json'
Write-JsonFile $InstalledRequestFile $InstalledRequest
& (Join-Path $InstalledBin 'ww-windows-provision.exe') --request $InstalledRequestFile
if ($LASTEXITCODE -ne 0) { throw 'Separate fresh installed-start provisioning failed.' }
$InstalledProvision = Join-Path $EvidenceRoot 'installed-runtime/provisioning.json'
& $Python $Acceptance installed-start --source $RepoRoot --manifest $InstalledProvision --evidence (Join-Path $EvidenceRoot 'installed-start') --installed-bin $InstalledBin
if ($LASTEXITCODE -ne 0) { throw 'The real installed profile startup/offline Cargo acceptance failed.' }
Write-Host "Mandatory native tree suite and freshly installed profile passed. Evidence: $EvidenceRoot"
