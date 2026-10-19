#!/usr/bin/env pwsh
$ErrorActionPreference = 'Stop'
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'Build on native Windows with the x64 MSVC C++ tools.'
}
Set-Location (Split-Path -Parent $PSScriptRoot)
if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) {
    throw 'Open x64 Developer PowerShell with Visual Studio C++ Build Tools and Windows SDK installed.'
}
cargo build-windows
if ($LASTEXITCODE -ne 0) { throw 'Windows release build failed' }
