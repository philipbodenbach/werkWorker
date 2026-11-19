[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$PairDirectory,
    [Parameter(Mandatory)][string]$ObjectOwner,
    [Parameter(Mandatory)][string]$NewEvidence
)
$ErrorActionPreference = 'Stop'
$token = [Security.Principal.WindowsIdentity]::GetCurrent()
if ([Security.Principal.WindowsPrincipal]::new($token).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run from the real normal host session.'
}
$ownerPath = (Resolve-Path -LiteralPath $ObjectOwner).Path
if ((Get-FileHash -LiteralPath $ownerPath).Hash -ne '276cb07196ca714b221ea116b7bf8d0d937c40c43798fe1f4432c34e4a53e7d4') {
    throw 'Unknown object provisioner.'
}
$trial = [guid]::NewGuid().ToString('N')
$receiptDir = (New-Item -ItemType Directory -Path $NewEvidence -ErrorAction Stop).FullName
$receipt = [ordered]@{ ordinary_sid = $token.User.Value; session = (Get-Process -Id $PID).SessionId; trial = $trial;
    provision = 'NOT_RUN'; ordinary_version = 'NOT_RUN'; provision_exit = $null; version_launcher_exit = $null }
$ready = [Threading.EventWaitHandle]::new($false, [Threading.EventResetMode]::ManualReset, "Local\ww-cargo-ui-ready-$trial")
$release = [Threading.EventWaitHandle]::new($false, [Threading.EventResetMode]::ManualReset, "Local\ww-cargo-ui-release-$trial")
$admin = $null
try {
    $scriptPath = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot 'provision-cargo-version-ui.ps1')).Path
    $scriptHash = (Get-FileHash -LiteralPath $scriptPath).Hash
    $receipt.provision_script_sha256 = $scriptHash
    # The elevated shell parses the same bytes it just hashed. Cargo stays here.
    $code = '$b=[IO.File]::ReadAllBytes(''' + $scriptPath.Replace("'","''") + ''');' +
        '$h=[BitConverter]::ToString([Security.Cryptography.SHA256]::Create().ComputeHash($b)).Replace(''-'','''');' +
        'if($h -ne ''' + $scriptHash + '''){exit 91};' +
        '& ([ScriptBlock]::Create([Text.Encoding]::UTF8.GetString($b))) -Trial ''' + $trial +
        ''' -ObjectOwner ''' + $ownerPath.Replace("'","''") + ''''
    if ($code.Contains('"')) { throw 'Unexpected quoting in provisioning request.' }
    $psExe = Join-Path ([Environment]::SystemDirectory) 'WindowsPowerShell/v1.0/powershell.exe'
    $admin = Start-Process -FilePath $psExe -ArgumentList ('-NoProfile -NonInteractive -Command "' + $code + '"') -WorkingDirectory ([Environment]::SystemDirectory) -Verb RunAs -WindowStyle Hidden -PassThru
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while (-not $ready.WaitOne(100)) {
        if ($admin.HasExited) { throw "Private provisioner exited before readiness with $($admin.ExitCode)." }
        if ($timer.Elapsed.TotalSeconds -ge 30) { throw 'Private object provisioning timed out.' }
    }
    $receipt.provision = 'READY'
    & python.exe -B (Join-Path $PSScriptRoot 'check-cargo-writer-version.py') $PairDirectory (Join-Path $receiptDir 'version') --private-dirs --private-desktop --match-token-flags --provided-stations $trial
    $receipt.version_launcher_exit = $LASTEXITCODE
    $native = Get-Content -Encoding UTF8 -LiteralPath (Join-Path $receiptDir 'version/version.log')
    $receipt.ordinary_version = if ($receipt.version_launcher_exit -eq 0) { 'PASS' } elseif (@($native | Select-String '^WORKLOAD_EXIT ').Count -eq 2) { 'FAIL' } else { 'NOT_RUN' }
} catch {
    $receipt.error = $_.Exception.Message
    throw
} finally {
    [void]$release.Set()
    if ($admin) {
        if ($admin.WaitForExit(12000)) { $receipt.provision_exit = $admin.ExitCode }
        $admin.Dispose()
    }
    $release.Dispose()
    $ready.Dispose()
    $receipt | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $receiptDir 'provision-result.json') -Encoding UTF8
}
if ($receipt.provision_exit -ne 0 -or $receipt.ordinary_version -ne 'PASS') { exit 1 }
