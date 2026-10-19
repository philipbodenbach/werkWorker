# Adapted from werk1112's native installer; see NOTICE.
$ErrorActionPreference = 'Stop'
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT -or
    [Environment]::GetEnvironmentVariable('PROCESSOR_ARCHITECTURE') -ne 'AMD64') {
    throw 'This installer supports native Windows x86_64.'
}
$Repo = if ($env:WERK_WORKER_REPO) { $env:WERK_WORKER_REPO } else { 'philipbodenbach/werkWorker' }
$VersionInput = if ($env:WERK_WORKER_VERSION) { $env:WERK_WORKER_VERSION } else { 'latest' }
$InstallDir = if ($env:WERK_WORKER_INSTALL_DIR) { $env:WERK_WORKER_INSTALL_DIR } else {
    if (-not $env:LOCALAPPDATA) { throw 'Set LOCALAPPDATA or WERK_WORKER_INSTALL_DIR' }
    Join-Path $env:LOCALAPPDATA 'Programs/werkWorker/bin'
}
if ($VersionInput -eq 'latest') {
    if ($env:WERK_WORKER_ARCHIVE) { throw 'Set WERK_WORKER_VERSION with a local archive' }
    $VersionInput = (Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest").tag_name
}
if ($VersionInput -notmatch '^v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') { throw 'Invalid stable release version' }
$Version = $VersionInput -replace '^v', ''
$Name = "werkWorker-v$Version-windows-x86_64.zip"
$Url = "https://github.com/$Repo/releases/download/v$Version/$Name"
$TempDir = Join-Path ([IO.Path]::GetTempPath()) ('werkWorker-install-' + [guid]::NewGuid().ToString('N'))
try {
    New-Item $TempDir -ItemType Directory | Out-Null
    $Archive = Join-Path $TempDir $Name
    if ($env:WERK_WORKER_ARCHIVE) {
        Copy-Item $env:WERK_WORKER_ARCHIVE $Archive
        Copy-Item "$env:WERK_WORKER_ARCHIVE.sha256" "$Archive.sha256"
    } else {
        Invoke-WebRequest $Url -OutFile $Archive -UseBasicParsing
        Invoke-WebRequest "$Url.sha256" -OutFile "$Archive.sha256" -UseBasicParsing
    }
    $Checksum = (Get-Content "$Archive.sha256" -Raw).Trim()
    if ($Checksum -notmatch ('^([a-fA-F0-9]{64})  ' + [regex]::Escape($Name) + '$')) { throw 'Invalid checksum file' }
    if ((Get-FileHash $Archive -Algorithm SHA256).Hash -ne $Matches[1]) { throw 'Checksum verification failed' }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $Zip = [IO.Compression.ZipFile]::OpenRead($Archive)
    try {
        $Expected = @('LICENSE', 'NOTICE', 'README.md', 'werk-worker.exe', 'ww.exe')
        $Names = @($Zip.Entries | ForEach-Object { $_.FullName } | Sort-Object)
        if (($Names -join '|') -cne ($Expected -join '|')) { throw 'Unexpected archive entries' }
        foreach ($Entry in $Zip.Entries) {
            $Kind = ($Entry.ExternalAttributes -shr 16) -band 0xF000
            if ($Kind -ne 0 -and $Kind -ne 0x8000) { throw 'Archive contains a non-regular entry' }
        }
    } finally { $Zip.Dispose() }
    Expand-Archive -LiteralPath $Archive -DestinationPath (Join-Path $TempDir 'extracted')
    New-Item $InstallDir -ItemType Directory -Force | Out-Null
    foreach ($Binary in @('werk-worker.exe', 'ww.exe')) {
        $Destination = Join-Path $InstallDir $Binary
        if (Test-Path $Destination -PathType Container) { throw "Destination is a directory: $Destination" }
        $Staged = Join-Path $InstallDir ('.werkWorker-' + [guid]::NewGuid().ToString('N'))
        try {
            Copy-Item (Join-Path $TempDir "extracted/$Binary") $Staged
            Move-Item $Staged $Destination -Force
        } finally { Remove-Item $Staged -Force -ErrorAction SilentlyContinue }
    }
    $UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $OnPath = @($env:Path -split ';' | ForEach-Object { $_.TrimEnd('\') }) -contains $InstallDir.TrimEnd('\')
    if (-not $OnPath) { Write-Warning "$InstallDir is not on PATH." }
    if ($env:WERK_WORKER_ADD_TO_PATH -eq '1' -and
        -not (@($UserPath -split ';' | ForEach-Object { $_.TrimEnd('\') }) -contains $InstallDir.TrimEnd('\'))) {
        [Environment]::SetEnvironmentVariable('Path', "$UserPath;$InstallDir".Trim(';'), 'User')
        Write-Host 'Added to user PATH. Open a new terminal.'
    }
    Write-Host "Installed werk-worker and ww in $InstallDir. Run: ww"
} finally { Remove-Item $TempDir -Recurse -Force -ErrorAction SilentlyContinue }
