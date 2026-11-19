# Administrative half of the fixed Cargo version check. The normal host owns
# the readiness/release events. Only the archived object's owner mode runs here.
param(
    [Parameter(Mandatory)][ValidatePattern('^[0-9a-f]{32}$')][string]$Trial,
    [Parameter(Mandatory)][string]$ObjectOwner
)
$ErrorActionPreference = 'Stop'
$principal = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Private station creation requires administrative provisioning.'
}
$ready = [Threading.EventWaitHandle]::OpenExisting("Local\ww-cargo-ui-ready-$Trial")
$release = [Threading.EventWaitHandle]::OpenExisting("Local\ww-cargo-ui-release-$Trial")
$expected = '276cb07196ca714b221ea116b7bf8d0d937c40c43798fe1f4432c34e4a53e7d4'
$children = @()
$stage = $null
$exe = $null
try {
    # Never elevate an executable directly from an ordinary-user writable tree.
    # Execute only the pinned bytes, in a fresh administrator-controlled directory.
    $bytes = [IO.File]::ReadAllBytes($ObjectOwner)
    $sha = [Security.Cryptography.SHA256]::Create()
    try { $actual = [BitConverter]::ToString($sha.ComputeHash($bytes)).Replace('-', '').ToLowerInvariant() }
    finally { $sha.Dispose() }
    if ($actual -ne $expected) { throw 'Unrecognized private object provisioner.' }
    $parent = [Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles)
    $stage = (New-Item -ItemType Directory -Path (Join-Path $parent ("WerkWorker-UiProbe-" + [guid]::NewGuid().ToString('N'))) -ErrorAction Stop).FullName
    $exe = [IO.Path]::Combine($stage, 'ui-inspect.exe')
    [IO.File]::WriteAllBytes($exe, $bytes)
    foreach ($suffix in '0','1') {
        $start = [Diagnostics.ProcessStartInfo]::new()
        $start.FileName = $exe
        $start.Arguments = '--own-fresh-objects'
        $start.WorkingDirectory = [Environment]::SystemDirectory
        $start.UseShellExecute = $false
        $start.CreateNoWindow = $true
        $start.RedirectStandardInput = $true
        $start.RedirectStandardOutput = $true
        $start.EnvironmentVariables.Clear()
        $start.EnvironmentVariables['SystemRoot'] = [Environment]::GetEnvironmentVariable('SystemRoot')
        $start.EnvironmentVariables['PATH'] = [Environment]::SystemDirectory
        $start.EnvironmentVariables['WW_UI_TRIAL_NONCE'] = $Trial.Substring(0,31) + $suffix
        $child = [Diagnostics.Process]::Start($start)
        $children += $child
        do {
            $line = $child.StandardOutput.ReadLineAsync()
            if (-not $line.Wait(10000) -or $null -eq $line.Result) { throw 'Object owner did not reach readiness.' }
        } until ($line.Result -eq 'PROVISIONER_READY')
    }
    [void]$ready.Set()
    if (-not $release.WaitOne(90000)) { throw 'Normal host did not release within the fixture deadline.' }
} finally {
    foreach ($child in $children) {
        $child.StandardInput.Close()
        if (-not $child.WaitForExit(5000)) { $child.Kill(); $child.WaitForExit() }
        $child.Dispose()
    }
    # Remove only the file created above and its newly created, empty directory.
    if ($exe) { [IO.File]::Delete($exe) }
    if ($stage) { [IO.Directory]::Delete($stage, $false) }
    $ready.Dispose()
    $release.Dispose()
}
