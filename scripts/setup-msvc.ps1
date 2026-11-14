#!/usr/bin/env pwsh
# Configure native MSVC for this and subsequent GitHub Actions steps.
$ErrorActionPreference = 'Stop'
if (-not $env:GITHUB_ENV) {
    throw 'GITHUB_ENV is required to persist the MSVC environment.'
}
$Before = [Environment]::GetEnvironmentVariables('Process')
$VsWhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
$Installation = & $VsWhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if ($LASTEXITCODE -ne 0 -or -not $Installation) {
    throw 'Visual Studio with the x64 MSVC C++ tools was not found.'
}
Import-Module (Join-Path $Installation 'Common7/Tools/Microsoft.VisualStudio.DevShell.dll')
Enter-VsDevShell -VsInstallPath $Installation -SkipAutomaticLocation -DevCmdArguments '-arch=x64 -host_arch=x64' | Out-Null
if ($env:VSCMD_ARG_TGT_ARCH -ne 'x64' -or $env:VSCMD_ARG_HOST_ARCH -ne 'x64') {
    throw 'Visual Studio did not configure the x64 host and target tools.'
}
foreach ($Tool in @('cl.exe', 'link.exe')) {
    $Command = Get-Command $Tool -CommandType Application -ErrorAction Stop | Select-Object -First 1
    $ToolDirectory = Join-Path $env:VCToolsInstallDir 'bin/Hostx64/x64'
    if ([IO.Path]::GetDirectoryName($Command.Source) -ne [IO.Path]::GetFullPath($ToolDirectory)) {
        throw "The selected $Tool is not from the configured MSVC toolchain: $($Command.Source)"
    }
}
# Export only variables changed by Developer PowerShell, not the whole runner
# environment. UTF-8 without a BOM also works under Windows PowerShell 5.1.
$Utf8 = [Text.UTF8Encoding]::new($false)
foreach ($Entry in [Environment]::GetEnvironmentVariables('Process').GetEnumerator()) {
    if ($Before[$Entry.Key] -cne $Entry.Value) {
        $Delimiter = 'MSVC_' + [guid]::NewGuid().ToString('N')
        $Record = "$($Entry.Key)<<$Delimiter`n$($Entry.Value)`n$Delimiter`n"
        [IO.File]::AppendAllText($env:GITHUB_ENV, $Record, $Utf8)
    }
}
Write-Host 'Configured MSVC x64 for subsequent workflow steps.'
