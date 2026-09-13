#requires -Version 5.1
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string] $SmokeProbe,
    [string] $EvidencePath,
    [ValidateRange(1, 100)] [int] $Cycles = 3,
    [switch] $Execute,
    [switch] $Independent
)
$ErrorActionPreference = 'Stop'

if ($Independent) {
    Write-Output 'Client-crash recovery: terminate one newly spawned render/capture client after both independent active-PCM handshakes.'
} else {
    Write-Output 'Client-crash recovery: terminate only a newly spawned round-trip helper after its active-PCM handshake.'
}
Write-Output 'No boot, service, device, default audio endpoint, or existing application settings are changed.'
if (-not $Execute -or $WhatIfPreference) {
    if ($Independent) {
        Write-Output "Plan only: $Cycles app-cable and $Cycles relay-cable independent render/capture survivor crashes, each followed by both-cable verification."
    } else {
        Write-Output "Plan only: $Cycles app-cable and $Cycles relay-cable crashes, each followed by both-cable verification."
    }
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
    scope = if ($Independent) {
        'Render and capture clients run in separate newly spawned processes. Each row terminates one owned client, verifies the other remains alive, then verifies recovery. Not application-backend-crash or Verifier evidence.'
    } else {
        'Both render and capture clients die in the same owned process. Not independent-client, active-survivor, application-backend-crash, or Verifier evidence.'
    }
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

function Start-HeldClient([string] $Flow, [string] $Role, $Row) {
    $child = New-Object System.Diagnostics.Process
    $child.StartInfo = New-Object System.Diagnostics.ProcessStartInfo
    $option = if ($Flow -eq 'render') { '--hold-render-role' } else { '--hold-capture-role' }
    $child.StartInfo.FileName = $smokePath
    $child.StartInfo.Arguments = "$option $Role --duration-ms 60000"
    $child.StartInfo.UseShellExecute = $false
    $child.StartInfo.CreateNoWindow = $true
    $child.StartInfo.RedirectStandardOutput = $true
    $child.StartInfo.RedirectStandardError = $true
    if (-not $child.Start()) { throw "Could not start independent $Flow smoke child." }
    $client = [pscustomobject]@{
        flow = $Flow
        role = $Role
        process = $child
        stdout_task = $child.StandardOutput.ReadLineAsync()
        stderr_task = $child.StandardError.ReadToEndAsync()
        output = @()
        ready = $false
        ready_line = $null
    }
    if ($Flow -eq 'render') { $Row.render_pid = $child.Id } else { $Row.capture_pid = $child.Id }
    return $client
}

function Wait-HeldHandshake($Client, $Row) {
    $startup = [Diagnostics.Stopwatch]::StartNew()
    while ($startup.Elapsed.TotalSeconds -lt 15 -and -not $Client.ready) {
        if ($Client.stdout_task.Wait(100)) {
            $line = $Client.stdout_task.Result
            if ($null -eq $line) {
                $details = if ($Client.stderr_task.IsCompleted) { $Client.stderr_task.Result } else { '' }
                throw "Independent $($Client.flow) child ended before active PCM: $details"
            }
            $Client.output += $line
            $Row.output += "$($Client.flow): $line"
            if ($Client.output.Count -gt 50) { throw 'Unexpected excessive output from independent child.' }
            if ($line -match '^QPWGRAPH_STREAM_ACTIVE pid=([0-9]+) flow=([a-z]+) frames=([0-9]+) peak=') {
                if ([int64]$Matches[1] -ne $Client.process.Id) {
                    throw "Independent $($Client.flow) handshake PID $($Matches[1]) did not match owned PID $($Client.process.Id)."
                }
                if ($Matches[2] -ne $Client.flow -or [uint64]$Matches[3] -eq 0) {
                    throw "Independent handshake did not confirm active $($Client.flow) PCM: $line"
                }
                $Client.ready = $true
                $Client.ready_line = $line
                if ($Client.flow -eq 'render') { $Row.render_ready_line = $line } else { $Row.capture_ready_line = $line }
            } else {
                $Client.stdout_task = $Client.process.StandardOutput.ReadLineAsync()
            }
        }
        if ($Client.process.HasExited -and -not $Client.ready) {
            $details = if ($Client.stderr_task.IsCompleted) { $Client.stderr_task.Result } else { '' }
            throw "Independent $($Client.flow) child exited before active PCM (exit $($Client.process.ExitCode)): $details"
        }
    }
    if (-not $Client.ready -or $Client.process.HasExited) {
        throw "Independent $($Client.flow) child did not report active PCM before timeout."
    }
}

function Stop-HeldClient($Client, $Row, [bool] $WasTarget) {
    if ($null -eq $Client) { return }
    try {
        if (-not $Client.process.HasExited) {
            $Client.process.Kill()
            if (-not $Client.process.WaitForExit(5000)) {
                throw "Could not terminate independent $($Client.flow) helper PID $($Client.process.Id)"
            }
        }
        $exitCode = $Client.process.ExitCode
        if ($WasTarget) { $Row.target_exit_code = $exitCode } else { $Row.survivor_exit_code = $exitCode }
    } finally {
        $Client.process.Dispose()
    }
}

function Invoke-IndependentCrash([string] $Cable, [string] $RenderRole, [string] $CaptureRole, [string] $TargetFlow, $Row) {
    $render = $null
    $capture = $null
    $target = $null
    $survivor = $null
    try {
        $render = Start-HeldClient 'render' $RenderRole $Row
        $capture = Start-HeldClient 'capture' $CaptureRole $Row
        Wait-HeldHandshake $render $Row
        Wait-HeldHandshake $capture $Row
        $target = if ($TargetFlow -eq 'render') { $render } else { $capture }
        $survivor = if ($TargetFlow -eq 'render') { $capture } else { $render }
        $Row.target_pid = $target.process.Id
        $Row.survivor_pid = $survivor.process.Id
        if ($target.process.HasExited -or $survivor.process.HasExited) {
            throw "Independent $Cable clients were not both alive after their handshakes."
        }
        $target.process.Kill()
        if (-not $target.process.WaitForExit(5000)) {
            throw "Independent $TargetFlow client did not exit after termination."
        }
        $Row.target_terminated = $true
        Start-Sleep -Milliseconds 750
        $Row.survivor_alive_after_target_ms = -not $survivor.process.HasExited
        if (-not $Row.survivor_alive_after_target_ms) {
            throw "Independent survivor $($survivor.flow) exited after the $TargetFlow client was terminated."
        }
    } finally {
        Stop-HeldClient $target $Row $true
        if ($survivor -ne $target) { Stop-HeldClient $survivor $Row $false }
        if ($render -ne $target -and $render -ne $survivor) { Stop-HeldClient $render $Row $false }
        if ($capture -ne $target -and $capture -ne $survivor) { Stop-HeldClient $capture $Row $false }
    }
}

try {
    $evidence.preflight = Invoke-Probe @('--verify-cables', '--duration-ms', '1000')
    if ($Independent) {
        $cables = @(
            [ordered]@{ name = 'app'; render_role = 'app-render'; capture_role = 'app-monitor' },
            [ordered]@{ name = 'relay'; render_role = 'relay-render'; capture_role = 'relay-capture' }
        )
        foreach ($cable in $cables) {
            foreach ($targetFlow in @('render', 'capture')) {
                for ($cycle = 1; $cycle -le $Cycles; $cycle++) {
                    $row = [ordered]@{
                        cable = $cable.name
                        target_flow = $targetFlow
                        cycle = $cycle
                        status = 'running'
                        output = @()
                        started_utc = [DateTime]::UtcNow.ToString('o')
                    }
                    $evidence.rows += $row
                    try {
                        Invoke-IndependentCrash $cable.name $cable.render_role $cable.capture_role $targetFlow $row
                        # No automatic retries: a failure to open, carry audio, or keep the survivor alive is retained.
                        $row.recovery = Invoke-Probe @('--verify-cables', '--duration-ms', '1000')
                        $row.status = 'passed'
                        Write-Output "independent $($cable.name) $targetFlow crash/recovery cycle $cycle/$Cycles passed (target PID $($row.target_pid), survivor PID $($row.survivor_pid))."
                    } catch {
                        $row.status = 'failed'
                        $row.failure = $_.Exception.Message
                        throw
                    } finally { $row.completed_utc = [DateTime]::UtcNow.ToString('o') }
                }
            }
        }
    } else {
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
