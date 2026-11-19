#!/usr/bin/env pwsh
[CmdletBinding(DefaultParameterSetName = 'Qualification')]
param(
    [Parameter(Mandatory = $true)][string]$EvidenceRoot,
    [Parameter(Mandatory = $true, ParameterSetName = 'Qualification')][string]$BindAddress,
    [Parameter(ParameterSetName = 'Qualification')][string]$Archive,
    [Parameter(Mandatory = $true, ParameterSetName = 'Preflight')][switch]$PreflightOnly,
    [switch]$RequirePsec
)
$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $PSScriptRoot
$Identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$Principal = [Security.Principal.WindowsPrincipal]::new($Identity)
$EvidenceRoot = [IO.Path]::GetFullPath($EvidenceRoot)
if (Test-Path -LiteralPath $EvidenceRoot) { throw 'Choose a new evidence directory; previous installations are never reused.' }
New-Item -ItemType Directory -Path $EvidenceRoot | Out-Null
$Utf8 = [Text.UTF8Encoding]::new($false)
function Write-JsonFile($Path, $Value) {
    [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 20) + "`n", $Utf8)
}
$Python = (Get-Command python.exe -CommandType Application -ErrorAction Stop).Source
$Acceptance = Join-Path $PSScriptRoot 'validate-windows-process-tree.py'
$ResultFile = Join-Path $EvidenceRoot 'qualification-result.json'
$Result = [ordered]@{
    archive_sha256 = $null
    suite_setup_exit = $null
    mandatory_suite_exit = $null
    installed_setup_exit = $null
    installed_start_exit = $null
    qualified = $false
    status = 'NOT_RUN'
    host_preflight = $null
    preparation = @{ status = 'NOT_RUN' }
}
$PreflightDir = Join-Path $EvidenceRoot 'preflight'
$PreflightArgs = @('preflight', '--evidence', $PreflightDir)
if ($RequirePsec) { $PreflightArgs += '--require-psec' }
& $Python $Acceptance @PreflightArgs
$PreflightExit = $LASTEXITCODE
$PreflightFile = Join-Path $PreflightDir 'preflight-result.json'
$Preflight = if (Test-Path -LiteralPath $PreflightFile) {
    Get-Content -LiteralPath $PreflightFile -Raw -Encoding UTF8 | ConvertFrom-Json
}
$Result.host_preflight = @{
    exit_code = $PreflightExit
    status = $Preflight.status
    error = $Preflight.error
    evidence = 'preflight/preflight-result.json'
}
if ($PreflightExit -ne 0 -or $Preflight.status -ne 'READY') {
    $Result.status = 'INFRASTRUCTURE_BLOCKED'
    Write-JsonFile $ResultFile $Result
    throw "Native host preflight failed before package build or provisioning. $($Preflight.error)"
}
if ($PreflightOnly) {
    Write-JsonFile $ResultFile $Result
    Write-Host "Verified ordinary acceptance host. Evidence: $PreflightFile"
    return
}
if (-not $Principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    $Result.status = 'INFRASTRUCTURE_BLOCKED'
    $Result.error = 'An elevated provisioning context is required for full acceptance.'
    Write-JsonFile $ResultFile $Result
    throw $Result.error
}
$PreparationStage = 'package_build'
try {
    $SmokeBuildArgs = @()
    if (-not $Archive) {
        # Diagnostic source snapshots must never share the product's binary output.
        # Cargo freshness records alone do not prove a shared top-level EXE's origin.
        $PackageDirectory = Join-Path $EvidenceRoot 'built-package'
        $TreePreviousCargoTargetDir = $env:CARGO_TARGET_DIR
        try {
            $env:CARGO_TARGET_DIR = Join-Path $EvidenceRoot 'product-build'
            & (Join-Path $PSScriptRoot 'package-release.ps1') -OutputDirectory $PackageDirectory
            if ($LASTEXITCODE -ne 0) { throw 'Native package build failed.' }
        } finally {
            [Environment]::SetEnvironmentVariable('CARGO_TARGET_DIR', $TreePreviousCargoTargetDir, 'Process')
        }
        $Manifest = Get-Content -LiteralPath (Join-Path $RepoRoot 'Cargo.toml') -Raw
        $Version = [regex]::Match($Manifest, '(?ms)^\[package\]\s+.*?^version = "([0-9]+\.[0-9]+\.[0-9]+)"').Groups[1].Value
        if (-not $Version) { throw 'Missing stable package version.' }
        $BuiltBin = Join-Path $EvidenceRoot 'product-build/x86_64-pc-windows-msvc/release'
        $SmokeBuildArgs = @('--windows-build-bin', $BuiltBin)
        $Archive = Join-Path $PackageDirectory "werkWorker-v$Version-windows-x86_64.zip"
    }
    $PreparationStage = 'archive_verification'
    $Archive = (Resolve-Path -LiteralPath $Archive).Path
    $Result.archive_sha256 = (Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash.ToLowerInvariant()
    $PreparationStage = 'package_installation'
    $Installation = Join-Path $EvidenceRoot 'installation'
    & $Python (Join-Path $PSScriptRoot 'smoke-install.py') $Archive --retain $Installation @SmokeBuildArgs
    if ($LASTEXITCODE -ne 0) { throw 'Fresh package installation failed.' }
    $PreparationStage = 'toolchain_discovery'
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
    $PreparationStage = 'provisioning_request'
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
    $Result.preparation = @{ status = 'PASS' }
    $Result.status = 'FAIL'
    & (Join-Path $InstalledBin 'ww-windows-provision.exe') --request $Request
    $Result.suite_setup_exit = $LASTEXITCODE
    if ($Result.suite_setup_exit -ne 0) { throw 'Fresh provisioning from the installed helpers failed.' }
    $Provision = Join-Path $Runtime 'provisioning.json'
    & $Python $Acceptance suite --source $RepoRoot --manifest $Provision --evidence (Join-Path $EvidenceRoot 'suite') --bind-address $BindAddress
    $Result.mandatory_suite_exit = $LASTEXITCODE
    # Use another fresh workspace so the suite retains its crash receipt.
    $InstalledRequest = Get-Content -LiteralPath $Request -Raw | ConvertFrom-Json
    $InstalledRequest.provisioning_root = Join-Path $EvidenceRoot 'installed-runtime'
    $InstalledRequest.workspace = Join-Path $EvidenceRoot 'installed-workspace'
    $InstalledRequestFile = Join-Path $EvidenceRoot 'installed-setup-request.json'
    Write-JsonFile $InstalledRequestFile $InstalledRequest
    & (Join-Path $InstalledBin 'ww-windows-provision.exe') --request $InstalledRequestFile
    $Result.installed_setup_exit = $LASTEXITCODE
    if ($Result.installed_setup_exit -ne 0) { throw 'Separate fresh installed-start provisioning failed.' }
    $InstalledProvision = Join-Path $EvidenceRoot 'installed-runtime/provisioning.json'
    & $Python $Acceptance installed-start --source $RepoRoot --manifest $InstalledProvision --evidence (Join-Path $EvidenceRoot 'installed-start') --installed-bin $InstalledBin
    $Result.installed_start_exit = $LASTEXITCODE
    $Result.qualified = $Result.mandatory_suite_exit -eq 0 -and $Result.installed_start_exit -eq 0
    if ($Result.qualified) { $Result.status = 'PASS' }
} catch {
    if ($Result.preparation.status -ne 'PASS') {
        $Result.preparation = @{
            status = 'FAIL'
            stage = $PreparationStage
            error_id = $_.FullyQualifiedErrorId
            error = $_.Exception.Message
        }
    } else {
        $Result.error = $_.Exception.Message
    }
    throw
} finally {
    Write-JsonFile $ResultFile $Result
}
if (-not $Result.qualified) {
    throw "Process-tree qualification failed. Mandatory suite exit $($Result.mandatory_suite_exit), installed offline Cargo acceptance exit $($Result.installed_start_exit). Evidence: $EvidenceRoot"
}
Write-Host "Mandatory native tree suite and freshly installed profile passed. Evidence: $EvidenceRoot"
