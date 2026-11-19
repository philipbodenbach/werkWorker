<#
.SYNOPSIS
Read-only MXC ProcessContainer request/tier discovery; never launches a workload.
.DESCRIPTION
Snapshots the diagnostic sources into a fresh local NTFS lab and builds the pinned
supported-v1 SDK probe with native Windows Rust/MSVC. Records the request, selected
tier, warnings, errors and host facts. Does not certify actual launched isolation,
resource limits or cleanup, and never enables a product profile. No elevation,
feature installation, host ACL changes, capabilities or network grant are added.
Exit 0 requires base-container with no DACL augmentation, warning or error; it is
only a prerequisite result. Exit 2 means unavailable/incompatible/incomplete.
.PARAMETER Offline
Use only dependencies already present in the native Cargo cache.
#>
[CmdletBinding()]
param([switch]$Offline)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$lab = $null
try {
    if ($env:OS -ne 'Windows_NT' -or -not [Environment]::Is64BitProcess) { throw 'Native x64 Windows PowerShell is required.' }
    $encoder = Join-Path $PSScriptRoot 'psec-budget-repro\policy-encoder'
    $inputs = @('Cargo.toml', 'Cargo.lock', 'src\main.rs', 'src\bin\mxc-probe.rs')
    foreach ($inputFile in $inputs) {
        if (-not (Test-Path -LiteralPath (Join-Path $encoder $inputFile) -PathType Leaf)) { throw "Missing compiler input: $inputFile" }
    }
    $cargo = (Get-Command cargo.exe -ErrorAction Stop).Source
    $rustc = (& rustc.exe --version --verbose | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $rustc -notmatch '(?m)^host: x86_64-pc-windows-msvc\r?$') { throw 'Native x64 MSVC Rust toolchain required.' }
    $cargoVersion = (& $cargo --version --verbose | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'Could not identify Cargo.' }
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    $installation = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ($LASTEXITCODE -ne 0 -or -not $installation) { throw 'Installed MSVC tools are required.' }
    $msvcVersion = (Get-Content -LiteralPath (Join-Path $installation 'VC\Auxiliary\Build\Microsoft.VCToolsVersion.default.txt') -Raw).Trim()
    $lab = Join-Path ([IO.Path]::GetTempPath()) ('werkworker-mxc-probe-' + [Guid]::NewGuid().ToString('N'))
    $drive = [IO.Path]::GetPathRoot($lab)
    if ($drive -notmatch '^[A-Za-z]:\\$' -or [IO.DriveInfo]::new($drive).DriveFormat -ne 'NTFS') { throw 'A local NTFS temporary directory is required.' }
    if (Test-Path -LiteralPath $lab) { throw 'Refusing to reuse a lab.' }
    foreach ($subdirectory in @('', 'encoder', 'encoder\src', 'encoder\src\bin', 'runtime', 'workspace', 'scratch')) {
        [void](New-Item -ItemType Directory -Path (Join-Path $lab $subdirectory))
    }
    Write-Output "ARTIFACT_DIRECTORY $lab"
    $snapshot = Join-Path $lab 'encoder'
    foreach ($inputFile in $inputs) { Copy-Item -LiteralPath (Join-Path $encoder $inputFile) -Destination (Join-Path $snapshot $inputFile) }
    Copy-Item -LiteralPath $PSCommandPath -Destination (Join-Path $lab 'probe-windows-mxc.ps1')
    $version = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
    $manifest = [ordered]@{
        windows_build = ($version.CurrentBuild + '.' + $version.UBR)
        windows_display_version = $version.DisplayVersion
        native_target = 'x86_64-pc-windows-msvc'
        rustc = $rustc
        cargo = $cargoVersion
        visual_studio = $installation
        msvc_version = $msvcVersion
        source = $encoder
        lab = $lab
        workload_launched = $false
        hashes = @(@($inputs | ForEach-Object { Join-Path $snapshot $_ }) + @(Join-Path $lab 'probe-windows-mxc.ps1') | ForEach-Object {
            $hash = Get-FileHash -Algorithm SHA256 -LiteralPath $_
            [ordered]@{ path = $hash.Path; sha256 = $hash.Hash.ToLowerInvariant() }
        })
    }
    $manifest | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $lab 'manifest.json') -Encoding UTF8
    $arguments = @('build', '--locked', '--bin', 'mxc-probe', '--manifest-path', (Join-Path $snapshot 'Cargo.toml'), '--target', 'x86_64-pc-windows-msvc', '--target-dir', (Join-Path $lab 'target'))
    if ($Offline) { $arguments += '--offline' }
    $ErrorActionPreference = 'Continue'
    & $cargo @arguments 2>&1 | Tee-Object -FilePath (Join-Path $lab 'build.log')
    $buildExit = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($buildExit -ne 0) { throw "Native probe build failed: $buildExit" }
    $binary = Join-Path $lab 'target\x86_64-pc-windows-msvc\debug\mxc-probe.exe'
    Get-FileHash -Algorithm SHA256 -LiteralPath $binary | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $lab 'binary-hash.json') -Encoding UTF8
    & $binary $lab | Tee-Object -FilePath (Join-Path $lab 'probe.json')
    $probeExit = $LASTEXITCODE
    $report = Get-Content -LiteralPath (Join-Path $lab 'probe.json') -Raw | ConvertFrom-Json
    if ($probeExit -ne 0 -or -not $report.exact_base_container_prerequisite) { throw 'MXC prerequisite discovery rejected this request; inspect probe.json.' }
    # Check fields independently: a successful process exit alone is not evidence.
    $observation = $report.probe
    if ($observation.tier -ne 'base-container' -or $observation.needsDaclAugmentation -ne $false -or @($observation.warnings).Count -ne 0 -or ($observation.PSObject.Properties.Name -contains 'error')) { throw 'Unexpected tier, augmentation, warning or error.' }
    if (-not $report.probe_only -or $report.workload_launched -or $report.sandbox_qualified) { throw 'Unexpected probe scope.' }
    Write-Output 'PROBE_EXIT 0 (discovery only; no runtime isolation or cleanup qualification)'
    Write-Output "ARTIFACT_DIRECTORY $lab"
    exit 0
} catch {
    Write-Error -ErrorAction Continue $_
    if ($lab) { Write-Output "ARTIFACT_DIRECTORY $lab" }
    exit 2
}
