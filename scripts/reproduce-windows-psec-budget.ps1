<#
.SYNOPSIS
Check PSEC atomic Job assignment and the console-helper process-budget violation.
.DESCRIPTION
Requires native x64 Windows, MSVC C++ tools and Rust 1.89 or newer. Builds only
the fixed diagnostic fixture and pinned SDK policy encoder in a fresh temp lab.
Run from a complete repository checkout; this diagnostic is not packaged with
the product crate.
No elevation, feature installation, HCS, accounts, or host ACL changes are used.
The payload has explicit runtime/work/scratch paths and denies direct networking.
The lab and logs remain for inspection. Exit 1 means a measured containment violation;
exit 2 means setup or evidence was incomplete; exit 0 is not sandbox qualification.
An atomic Job-list positive control never executes an unrestricted payload.
When PSEC omits that Job assignment, its suspended payload is never resumed.
Separate diagnostic cases assign the suspended root explicitly to measure the
historical process-budget failure. This is not a product fallback.
.PARAMETER Offline
Build the policy encoder with Cargo's --offline option (dependencies must exist).
#>
[CmdletBinding()]
param([switch]$Offline)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$lab = $null
try {
    if ($env:OS -ne 'Windows_NT' -or -not [Environment]::Is64BitProcess) {
        throw 'Run from native 64-bit Windows PowerShell.'
    }
    $source = Join-Path $PSScriptRoot 'tests\windows_psec_budget.cpp'
    $encoderManifest = Join-Path $PSScriptRoot 'psec-budget-repro\policy-encoder\Cargo.toml'
    $encoderLock = Join-Path $PSScriptRoot 'psec-budget-repro\policy-encoder\Cargo.lock'
    foreach ($required in @($source, $encoderManifest, $encoderLock)) {
        if (-not (Test-Path -LiteralPath $required -PathType Leaf)) { throw "Missing source: $required" }
    }
    $cargo = (Get-Command cargo.exe -ErrorAction Stop).Source
    $rustVersion = & rustc.exe -vV
    if ($LASTEXITCODE -ne 0 -or $rustVersion -notcontains 'host: x86_64-pc-windows-msvc') {
        throw 'Use the native x86_64-pc-windows-msvc Rust toolchain.'
    }
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) { throw 'Existing Visual Studio C++ tools are required (vswhere.exe not found).' }
    $installation = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ($LASTEXITCODE -ne 0 -or -not $installation) { throw 'No installed MSVC x64 C++ toolchain found.' }
    $vcvars = Join-Path $installation 'VC\Auxiliary\Build\vcvars64.bat'
    if (-not (Test-Path -LiteralPath $vcvars -PathType Leaf)) { throw "Missing toolchain setup: $vcvars" }

    $lab = Join-Path ([IO.Path]::GetTempPath()) ('werkworker-psec-budget-' + [Guid]::NewGuid().ToString('N'))
    if (Test-Path -LiteralPath $lab) { throw "Refusing to reuse an existing lab: $lab" }
    [void](New-Item -ItemType Directory -Path $lab)
    $runtime = Join-Path $lab 'runtime'
    $workspace = Join-Path $lab 'workspace'
    $scratch = Join-Path $lab 'scratch'
    foreach ($directory in @($runtime, $workspace, $scratch)) { [void](New-Item -ItemType Directory -Path $directory) }
    Write-Output "ARTIFACT_DIRECTORY $lab"
    # Build immutable copies and retain a complete, reusable source bundle.
    $snapshot = Join-Path $lab 'source\scripts'
    $encoderSnapshot = Join-Path $snapshot 'psec-budget-repro\policy-encoder'
    [void](New-Item -ItemType Directory -Path (Join-Path $snapshot 'tests'))
    [void](New-Item -ItemType Directory -Path (Join-Path $encoderSnapshot 'src'))
    Copy-Item -LiteralPath $PSCommandPath -Destination (Join-Path $snapshot 'reproduce-windows-psec-budget.ps1')
    Copy-Item -LiteralPath $source -Destination (Join-Path $snapshot 'tests\windows_psec_budget.cpp')
    foreach ($relative in @('Cargo.toml', 'Cargo.lock', 'src\main.rs')) {
        Copy-Item -LiteralPath (Join-Path (Split-Path $encoderManifest) $relative) -Destination (Join-Path $encoderSnapshot $relative)
    }
    $source = Join-Path $snapshot 'tests\windows_psec_budget.cpp'
    $encoderManifest = Join-Path $encoderSnapshot 'Cargo.toml'
    $encoderLock = Join-Path $encoderSnapshot 'Cargo.lock'
    $fixtureSource = Join-Path $lab 'windows_psec_budget.cpp'
    Copy-Item -LiteralPath $source -Destination $fixtureSource
    $executable = Join-Path $runtime 'windows-psec-budget.exe'
    $compile = Join-Path $lab 'compile.cmd'
    $compileText = @"
@echo off
call "$vcvars" >nul
if errorlevel 1 exit /b %errorlevel%
cd /d "$lab"
cl /nologo /Bv /W4 /WX /EHsc /MT /std:c++17 windows_psec_budget.cpp /Fe:"$executable" /link advapi32.lib
exit /b %errorlevel%
"@
    [IO.File]::WriteAllText($compile, $compileText, [Text.UTF8Encoding]::new($false))
    $previousPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & $env:ComSpec /d /c $compile 2>&1 | Tee-Object -FilePath (Join-Path $lab 'compile.log')
    $compileExit = $LASTEXITCODE
    $ErrorActionPreference = $previousPreference
    if ($compileExit -ne 0) { throw "MSVC compile failed: $compileExit" }

    $target = Join-Path $lab 'encoder-target'
    $cargoArguments = @('build', '--locked', '--bin', 'werkworker-psec-policy-encoder', '--manifest-path', $encoderManifest, '--target-dir', $target)
    if ($Offline) { $cargoArguments += '--offline' }
    $ErrorActionPreference = 'Continue'
    & $cargo @cargoArguments 2>&1 | Tee-Object -FilePath (Join-Path $lab 'encoder-build.log')
    $buildExit = $LASTEXITCODE
    $ErrorActionPreference = $previousPreference
    if ($buildExit -ne 0) { throw "SDK encoder build failed: $buildExit" }
    $encoder = Join-Path $target 'debug\werkworker-psec-policy-encoder.exe'
    $system = [Environment]::SystemDirectory
    $volume = [IO.Path]::GetPathRoot($system)
    $baseline = Join-Path $lab 'baseline.bin'
    $denied = Join-Path $lab 'conhost-denied.bin'
    $policyArguments = @('--readonly', $runtime, '--readonly', $system, '--readwrite', $workspace, '--readwrite', $scratch, '--deny', $volume)
    & $encoder @policyArguments --out $baseline
    if ($LASTEXITCODE -ne 0) { throw 'Baseline SDK serialization failed.' }
    & $encoder @policyArguments --deny (Join-Path $system 'conhost.exe') --out $denied
    if ($LASTEXITCODE -ne 0) { throw 'Deny-control SDK serialization failed.' }

    $manifest = [ordered]@{
        os_version = [Environment]::OSVersion.Version.ToString()
        windows_build = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' | Select-Object CurrentBuild, UBR, DisplayVersion
        architecture = $env:PROCESSOR_ARCHITECTURE
        rustc = $rustVersion
        cargo = (& $cargo --version | Out-String).Trim()
        visual_studio = $installation
        lab = $lab
        policy = 'PSEC 1.1; terminate-on-close; no explicit capabilities; Win32k deny; UI 0x3ff; egress/ingress/loopback deny'
        readonly = @($runtime, $system)
        readwrite = @($workspace, $scratch)
        baseline_deny = @($volume)
        control_extra_deny = (Join-Path $system 'conhost.exe')
        hashes = @(@((Join-Path $snapshot 'reproduce-windows-psec-budget.ps1'), $source, $fixtureSource, $encoderManifest, $encoderLock, (Join-Path (Split-Path $encoderManifest) 'src\main.rs'), $baseline, $denied, $executable, $encoder) | ForEach-Object {
            $hash = Get-FileHash -Algorithm SHA256 -LiteralPath $_
            [ordered]@{ path = $hash.Path; sha256 = $hash.Hash.ToLowerInvariant() }
        })
    }
    $manifest | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $lab 'manifest.json') -Encoding UTF8
    $ErrorActionPreference = 'Continue'
    & $executable $baseline $denied $workspace $scratch 2>&1 | Tee-Object -FilePath (Join-Path $lab 'repro.log')
    $result = $LASTEXITCODE
    $ErrorActionPreference = $previousPreference
    Write-Output "REPRO_EXIT $result"
    Write-Output "ARTIFACT_DIRECTORY $lab"
    if ($result -notin @(0, 1, 2)) { throw "Unexpected fixture exit: $result" }
    exit $result
} catch {
    Write-Error -ErrorAction Continue $_
    if ($lab) { Write-Output "ARTIFACT_DIRECTORY $lab" }
    exit 2
}
