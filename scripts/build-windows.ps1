#!/usr/bin/env pwsh
# Native build and temporary CLI validation, following werk1112; see NOTICE.
$ErrorActionPreference = 'Stop'
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'Build on native Windows with the x64 MSVC C++ tools.'
}
$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot
if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) {
    throw 'Open x64 Developer PowerShell with Visual Studio C++ Build Tools and Windows SDK installed.'
}
if ($env:VSCMD_ARG_TGT_ARCH -and $env:VSCMD_ARG_TGT_ARCH -ne 'x64') {
    throw 'Use x64 Developer PowerShell, not an x86 or ARM64 MSVC environment.'
}
foreach ($Command in @('cargo', 'rustc')) {
    if (-not (Get-Command $Command -ErrorAction SilentlyContinue)) { throw "Required command not found: $Command" }
}
$RustVersion = rustc -vV
if ($LASTEXITCODE -ne 0 -or $RustVersion -notcontains 'host: x86_64-pc-windows-msvc') {
    throw 'Use the native x86_64-pc-windows-msvc Rust toolchain.'
}
$Python = $null
$PythonArgs = @()
foreach ($Candidate in @('py', 'python', 'python3')) {
    $Command = Get-Command $Candidate -ErrorAction SilentlyContinue
    if ($Command) {
        $Python = $Command.Source
        if ($Candidate -eq 'py') { $PythonArgs = @('-3') }
        break
    }
}
if (-not $Python) { throw 'Python 3.10+ is required (py, python or python3 on PATH).' }
& $Python @PythonArgs -c 'import sys; sys.exit(sys.version_info < (3, 10))'
if ($LASTEXITCODE -ne 0) { throw 'A working Python 3.10+ installation is required.' }
cargo build-windows
if ($LASTEXITCODE -ne 0) { throw 'Windows release build failed' }
$BuildRoot = Join-Path $RepoRoot 'target'
if ($env:CARGO_TARGET_DIR) {
    $BuildRoot = if ([IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) {
        $env:CARGO_TARGET_DIR
    } else {
        Join-Path $RepoRoot $env:CARGO_TARGET_DIR
    }
}
$Build = Join-Path $BuildRoot 'x86_64-pc-windows-msvc/release'
foreach ($Binary in @('werk-worker.exe', 'ww.exe')) {
    $BinaryPath = Join-Path $Build $Binary
    if (-not (Test-Path $BinaryPath -PathType Leaf)) { throw "Expected executable not found: $BinaryPath" }
    & $BinaryPath --version
    if ($LASTEXITCODE -ne 0) { throw "Binary smoke failed: $Binary" }
}
foreach ($Helper in @(
    @{ Name = 'ww-windows-provision.exe'; Refusal = 2 },
    @{ Name = 'ww-windows-runner.exe'; Refusal = 125 }
)) {
    $BinaryPath = Join-Path $Build $Helper.Name
    if (-not (Test-Path $BinaryPath -PathType Leaf)) { throw "Expected helper not found: $BinaryPath" }
    # An inert invocation must load and refuse. This performs no provisioning,
    # works with elevated/non-elevated build users and never requests UAC.
    $Probe = New-Object System.Diagnostics.Process
    try {
        $Probe.StartInfo.FileName = $BinaryPath
        $Probe.StartInfo.UseShellExecute = $false
        $Probe.StartInfo.RedirectStandardOutput = $true
        $Probe.StartInfo.RedirectStandardError = $true
        if (-not $Probe.Start()) { throw "Could not start helper: $($Helper.Name)" }
        $Output = $Probe.StandardOutput.ReadToEnd()
        $ErrorOutput = $Probe.StandardError.ReadToEnd()
        $Probe.WaitForExit()
        if ($Probe.ExitCode -ne $Helper.Refusal) {
            throw "Helper refusal failed: $($Helper.Name), exit $($Probe.ExitCode): $Output $ErrorOutput"
        }
    } finally { $Probe.Dispose() }
}
$Reference = Join-Path ([IO.Path]::GetTempPath()) ('werkWorker-reference-' + [guid]::NewGuid().ToString('N'))
try {
    New-Item $Reference -ItemType Directory | Out-Null
    & $Python @PythonArgs (Join-Path $PSScriptRoot 'generate-cli-reference.py') (Join-Path $Build 'werk-worker.exe') --output-dir $Reference
    if ($LASTEXITCODE -ne 0) { throw 'CLI reference validation failed' }
} finally {
    Remove-Item $Reference -Recurse -Force -ErrorAction SilentlyContinue
}
Write-Host "Built and validated both CLI commands and Windows sandbox helpers in $Build"
