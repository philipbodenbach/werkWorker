[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$PairDirectory,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [ValidateSet('outer', 'leaf-budget', 'assign-suspended')][string]$JobMode = 'outer'
)
$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run from the verified ordinary host.'
}
$out = [IO.DirectoryInfo]([IO.Path]::GetFullPath($OutputDirectory))
if ($out.Exists) { throw 'Choose a new output directory.' }
$out.Create()
$setup = Get-Content -LiteralPath (Join-Path $PairDirectory 'setup.json') -Raw | ConvertFrom-Json
$image = Join-Path $setup.stage 'runtime\cargo.exe'
$policy = (Resolve-Path -LiteralPath (Join-Path $PairDirectory 'B.bin')).Path
if ((Get-FileHash -LiteralPath $image).Hash -ne 'c37545ec61d48d31bdefce53280ecab61c4ea54eaace372fac6a0316c6e165d9' -or
    (Get-FileHash -LiteralPath $policy).Hash -ne '19605f8e2210a3bbb301447eb8ebab964b9426be9b1564ae460450227d6e9083') {
    throw 'Original Cargo or policy B identity mismatch.'
}
$source = Join-Path $PSScriptRoot 'suspended_child_policy.cpp'
Copy-Item -LiteralPath $source -Destination $out.FullName
$vswhere = Join-Path ([Environment]::GetEnvironmentVariable('ProgramFiles(x86)')) 'Microsoft Visual Studio\Installer\vswhere.exe'
$vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$vcvars = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
$compile = Join-Path $out.FullName 'compile.cmd'
[IO.File]::WriteAllLines($compile, @('@echo off', ('call "' + $vcvars + '"'),
    'if errorlevel 1 exit /b %errorlevel%',
    'cl /nologo /W4 /WX /EHsc /MT /std:c++17 /wd4191 suspended_child_policy.cpp'), [Text.Encoding]::ASCII)
Push-Location -LiteralPath $out.FullName
try {
    & cmd.exe /d /c compile.cmd > compile.log
    if ($LASTEXITCODE -ne 0) { throw 'Compile failed. See compile.log.' }
    $arguments = @($image, $policy)
    if ($JobMode -ne 'outer') { $arguments += $JobMode }
    & .\suspended_child_policy.exe @arguments | Tee-Object -FilePath result.log
    $code = $LASTEXITCODE
    [ordered]@{
        exit_code = $code; cargo = $image; policy = $policy; payload_resumed = $false
        compiler_setup = $vcvars; job_mode = $JobMode
        files = @(Get-FileHash -LiteralPath $image,$policy,$source,'suspended_child_policy.exe','result.log' |
            Select-Object Path,Hash)
    } | ConvertTo-Json -Depth 5 | Set-Content -Encoding UTF8 receipt.json
    exit $code
} finally { Pop-Location }
