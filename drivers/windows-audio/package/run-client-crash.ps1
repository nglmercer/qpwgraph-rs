#requires -Version 5.1
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string] $SmokeProbe,
    [string] $EvidencePath,
    [ValidateRange(1, 100)] [int] $Cycles = 3,
    [switch] $Execute
)
$ErrorActionPreference = 'Stop'

Write-Output 'Client-crash recovery: terminate only a newly spawned round-trip helper after its active-PCM handshake.'
Write-Output 'No boot, service, device, default audio endpoint, or existing application settings are changed.'
if (-not $Execute -or $WhatIfPreference) {
    Write-Output "Plan only: $Cycles app-cable and $Cycles relay-cable crashes, each followed by both-cable verification."
    return
}
if ([string]::IsNullOrWhiteSpace($EvidencePath)) {
    throw 'Pass a new -EvidencePath to retain both successes and failures.'
}
if (Test-Path -LiteralPath $EvidencePath) { throw 'EvidencePath already exists; use a new path to preserve earlier results.' }
if ([string]::IsNullOrWhiteSpace($SmokeProbe)) {
    $SmokeProbe = Join-Path $PSScriptRoot '../target/debug/qpwgraph-audio-smoke.exe'
}
$smokePath = (Resolve-Path -LiteralPath $SmokeProbe).Path
if (-not (Test-Path -LiteralPath $smokePath -PathType Leaf)) { throw 'SmokeProbe must be an executable file.' }
if (-not $PSCmdlet.ShouldProcess($smokePath, 'Start and forcibly terminate only owned active round-trip clients')) { return }
$evidence = [ordered]@{
    schema = 1
    kind = 'qpwgraph-windows-client-crash'
    started_utc = [DateTime]::UtcNow.ToString('o')
    completed = $false
    smoke_probe_sha256 = (Get-FileHash -LiteralPath $smokePath -Algorithm SHA256).Hash
    installed_sys_sha256 = $null
    cycles_per_cable = $Cycles
    rows = @()
    failure = $null
    scope = 'Both render and capture clients die in the same owned process. Not independent-client, active-survivor, application-backend-crash, or Verifier evidence.'
}
$installedSys = Join-Path $env:windir 'System32/drivers/qpwgraph_audio.sys'
if (Test-Path -LiteralPath $installedSys) {
    $evidence.installed_sys_sha256 = (Get-FileHash -LiteralPath $installedSys -Algorithm SHA256).Hash
}

function Invoke-Probe([string[]] $Arguments) {
    $previousPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = @(& $smokePath @Arguments 2>&1)
        $code = $LASTEXITCODE
    } finally { $ErrorActionPreference = $previousPreference }
    $details = ($output | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine
    if ($code -ne 0) { throw "Probe failed (exit $code): $details" }
    return $details
}

function Invoke-OwnedCrash([string] $Mode, $Row) {
    $child = New-Object System.Diagnostics.Process
    $child.StartInfo = New-Object System.Diagnostics.ProcessStartInfo
    $child.StartInfo.FileName = $smokePath
    $child.StartInfo.Arguments = "$Mode --duration-ms 60000"
    $child.StartInfo.UseShellExecute = $false
    $child.StartInfo.CreateNoWindow = $true
    $child.StartInfo.RedirectStandardOutput = $true
    $child.StartInfo.RedirectStandardError = $true
    $started = $false
    try {
        $started = $child.Start()
        if (-not $started) { throw 'Could not start owned smoke child.' }
        $Row.child_pid = $child.Id
        $stderr = $child.StandardError.ReadToEndAsync()
        $startup = [Diagnostics.Stopwatch]::StartNew()
        $ready = $false
        while ($startup.Elapsed.TotalSeconds -lt 15 -and -not $ready) {
            $lineTask = $child.StandardOutput.ReadLineAsync()
            while (-not $lineTask.Wait(100)) {
                if ($startup.Elapsed.TotalSeconds -ge 15) { throw 'Owned child did not report active PCM before timeout.' }
            }
            $line = $lineTask.Result
            if ($null -eq $line) { throw "Owned child ended before active PCM: $($stderr.Result)" }
            $Row.output += $line
            if ($Row.output.Count -gt 50) { throw 'Unexpected excessive output from owned child.' }
            $ready = Test-ActiveHandshake $line $child.Id
            if ($ready) { $Row.ready_line = $line }
        }
        if (-not $ready -or $child.HasExited) { throw 'Owned child was not alive with confirmed PCM; no crash was tested.' }
        $child.Kill()
        if (-not $child.WaitForExit(5000)) { throw 'Owned child did not exit after termination.' }
        $Row.child_exit_code = $child.ExitCode
        $Row.stderr = $stderr.Result
        $Row.terminated = $true
    } finally {
        if ($started -and -not $child.HasExited) {
            $child.Kill()
            if (-not $child.WaitForExit(5000)) { throw "Could not clean up owned helper PID $($Row.child_pid)" }
        }
        $child.Dispose()
    }
}

function Test-ActiveHandshake([string] $Line, [int] $ChildId) {
    if ($Line -notmatch '^QPWGRAPH_ROUND_TRIP_ACTIVE pid=([0-9]+) frames=([0-9]+)$') { return $false }
    return [int64]$Matches[1] -eq $ChildId -and [uint64]$Matches[2] -gt 0
}

try {
    $evidence.preflight = Invoke-Probe @('--verify-cables', '--duration-ms', '1000')
    foreach ($mode in @('--round-trip', '--relay-round-trip')) {
        for ($cycle = 1; $cycle -le $Cycles; $cycle++) {
            $row = [ordered]@{ mode = $mode; cycle = $cycle; status = 'running'; output = @(); started_utc = [DateTime]::UtcNow.ToString('o') }
            $evidence.rows += $row
            try {
                Invoke-OwnedCrash $mode $row
                # No automatic retries: a failure to open or carry audio is retained.
                $row.recovery = Invoke-Probe @('--verify-cables', '--duration-ms', '1000')
                $row.status = 'passed'
                Write-Output "$mode crash/reopen cycle $cycle/$Cycles passed (owned PID $($row.child_pid))."
            } catch {
                $row.status = 'failed'
                $row.failure = $_.Exception.Message
                throw
            } finally { $row.completed_utc = [DateTime]::UtcNow.ToString('o') }
        }
    }
    $evidence.completed = $true
} catch {
    $evidence.failure = $_.Exception.Message
    throw
} finally {
    $evidence.completed_utc = [DateTime]::UtcNow.ToString('o')
    $parent = Split-Path -Parent $EvidencePath
    if ($parent) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
    $evidence | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $EvidencePath -Encoding UTF8
    Write-Output "Client crash evidence saved: $EvidencePath"
}
