#requires -Version 5.1

[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'High')]
param(
    [Parameter(Mandatory = $false)]
    [string] $SmokeProbe,
    [Parameter(Mandatory = $false)]
    [string] $PackageRoot,
    [Parameter(Mandatory = $false)]
    [string] $EvidencePath,
    [Parameter(Mandatory = $false)]
    [ValidateRange(1, 10000)]
    [int] $Cycles = 100,
    [Parameter(Mandatory = $false)]
    [ValidateRange(50, 60000)]
    [int] $DurationMilliseconds = 250,
    [Parameter(Mandatory = $false)]
    [switch] $Execute,
    [Parameter(Mandatory = $false)]
    [switch] $AllowAudioServiceRestart,
    [Parameter(Mandatory = $false)]
    [switch] $AllowDeviceToggle
)

$ErrorActionPreference = 'Stop'

Write-Output 'QPWGraph Driver Verifier stress matrix'
Write-Output '  READ-ONLY: plan mode only inspects arguments and does not open audio clients.'
Write-Output '  STATE-MUTATING: -Execute opens/stops audio streams; optional service/device switches mutate system state.'
Write-Output '  REBOOT-REQUIRING: none is performed by this script; reboot and Verifier configuration are separate explicit steps.'

function Find-SmokeProbe([string] $RequestedPath) {
    if (-not [string]::IsNullOrWhiteSpace($RequestedPath)) {
        $resolved = (Resolve-Path -LiteralPath $RequestedPath -ErrorAction Stop).Path
        if (-not (Test-Path -LiteralPath $resolved -PathType Leaf)) {
            throw "Smoke probe was not found: $RequestedPath"
        }
        return $resolved
    }
    $root = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path
    $candidate = Join-Path $root 'drivers\windows-audio\target\debug\qpwgraph-audio-smoke.exe'
    if (Test-Path -LiteralPath $candidate -PathType Leaf) {
        return (Resolve-Path -LiteralPath $candidate).Path
    }
    return $null
}

function Invoke-Smoke([string[]] $Arguments) {
    $output = @(& $script:smokePath @Arguments 2>&1)
    $exitCode = $LASTEXITCODE
    if ($output.Count -gt 0) {
        Write-Verbose (($output | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine)
    }
    if ($exitCode -ne 0) {
        $details = ($output | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine
        throw "Smoke probe $($Arguments -join ' ') failed with exit code $exitCode. $details"
    }
}

function Get-Sha256([string] $Path) {
    $algorithm = [System.Security.Cryptography.SHA256]::Create()
    $stream = [System.IO.File]::OpenRead($Path)
    try {
        return ([System.BitConverter]::ToString($algorithm.ComputeHash($stream))).Replace('-', '')
    } finally {
        $stream.Dispose()
        $algorithm.Dispose()
    }
}

$script:stressRows = [ordered]@{
    app_cable = [ordered]@{
        requested_cycles = $Cycles
        completed_cycles = 0
        passed = $false
        status = 'not-run'
    }
    relay_cable = [ordered]@{
        requested_cycles = $Cycles
        completed_cycles = 0
        passed = $false
        status = 'not-run'
    }
    two_cable_isolation = [ordered]@{
        requested_cycles = $Cycles
        completed_cycles = 0
        passed = $false
        status = 'not-run'
    }
    audio_service_restart = [ordered]@{
        requested = [bool]$AllowAudioServiceRestart
        passed = $false
        status = 'not-run'
    }
    device_toggle = [ordered]@{
        requested = [bool]$AllowDeviceToggle
        passed = $false
        status = 'not-run'
    }
}
$script:stressStartedUtc = [DateTime]::UtcNow.ToString('o')
$script:packageEvidence = @()
$script:stressFailure = $null

if (-not [string]::IsNullOrWhiteSpace($PackageRoot)) {
    $packagePath = (Resolve-Path -LiteralPath $PackageRoot -ErrorAction Stop).Path
    foreach ($name in @('manifest.json', 'qpwgraph_audio.sys', 'qpwgraph-audio.inf', 'qpwgraph-audio.cat')) {
        $path = Join-Path $packagePath $name
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            $script:packageEvidence += [pscustomobject]@{
                name = $name
                sha256 = Get-Sha256 $path
                length = (Get-Item -LiteralPath $path).Length
            }
        } else {
            $script:packageEvidence += [pscustomobject]@{
                name = $name
                sha256 = $null
                length = $null
            }
        }
    }
}

function Write-StressEvidence([bool] $Completed, [string] $Failure) {
    if ([string]::IsNullOrWhiteSpace($EvidencePath)) {
        return
    }
    $document = [ordered]@{
        schema = 1
        kind = 'qpwgraph-windows-driver-stress'
        started_utc = $script:stressStartedUtc
        completed_utc = [DateTime]::UtcNow.ToString('o')
        completed = $Completed
        cycles = $Cycles
        duration_milliseconds = $DurationMilliseconds
        smoke_probe = $script:smokePath
        package_files = @($script:packageEvidence)
        rows = $script:stressRows
        failure = $Failure
        notes = @(
            'This record describes the explicit smoke matrix invoked by run-driver-stress.ps1.'
            'Driver Verifier configuration, bugcheck review, crash rows, install/upgrade rows, and lifecycle rows require separate retained evidence.'
        )
    }
    $parent = Split-Path -Parent $EvidencePath
    if (-not [string]::IsNullOrWhiteSpace($parent)) {
        New-Item -ItemType Directory -Path $parent -Force | Out-Null
    }
    Set-Content -LiteralPath $EvidencePath -Value ($document | ConvertTo-Json -Depth 12) -Encoding UTF8
    Write-Output "Stress evidence written to $((Resolve-Path -LiteralPath $EvidencePath).Path)."
}

if (-not $Execute) {
    Write-Output "Plan: $Cycles render cycles, $Cycles capture cycles, and $Cycles combined cable checks."
    Write-Output 'Plan: client-crash, AudioSrv-restart, device-toggle, install, and upgrade rows require separate explicit evidence.'
    exit 0
}

$script:smokePath = Find-SmokeProbe $SmokeProbe
if ($null -eq $script:smokePath) {
    throw 'An executable qpwgraph-audio-smoke probe is required with -Execute.'
}
if ($WhatIfPreference) {
    Write-Output 'WhatIf mode: no audio clients, services, or devices will be opened or changed.'
    exit 0
}

try {
    Invoke-Smoke @('--verify-roles')

    for ($cycle = 1; $cycle -le $Cycles; $cycle++) {
        Invoke-Smoke @('--round-trip', '--duration-ms', [string]$DurationMilliseconds)
        $script:stressRows.app_cable.completed_cycles = $cycle
        if (($cycle % 10) -eq 0 -or $cycle -eq $Cycles) {
            Write-Output "render/app cable cycles: $cycle/$Cycles"
        }
    }
    $script:stressRows.app_cable.passed = $true
    $script:stressRows.app_cable.status = 'passed'

    for ($cycle = 1; $cycle -le $Cycles; $cycle++) {
        Invoke-Smoke @('--relay-round-trip', '--duration-ms', [string]$DurationMilliseconds)
        $script:stressRows.relay_cable.completed_cycles = $cycle
        if (($cycle % 10) -eq 0 -or $cycle -eq $Cycles) {
            Write-Output "render/relay cable cycles: $cycle/$Cycles"
        }
    }
    $script:stressRows.relay_cable.passed = $true
    $script:stressRows.relay_cable.status = 'passed'

    for ($cycle = 1; $cycle -le $Cycles; $cycle++) {
        Invoke-Smoke @('--verify-cables', '--duration-ms', [string]$DurationMilliseconds)
        $script:stressRows.two_cable_isolation.completed_cycles = $cycle
        if (($cycle % 10) -eq 0 -or $cycle -eq $Cycles) {
            Write-Output "two-cable isolation cycles: $cycle/$Cycles"
        }
    }
    $script:stressRows.two_cable_isolation.passed = $true
    $script:stressRows.two_cable_isolation.status = 'passed'

    if ($AllowAudioServiceRestart) {
        Restart-Service -Name 'Audiosrv' -Force
        Start-Sleep -Seconds 2
        Invoke-Smoke @('--verify-roles')
        Invoke-Smoke @('--verify-cables', '--duration-ms', [string]$DurationMilliseconds)
        $script:stressRows.audio_service_restart.passed = $true
        $script:stressRows.audio_service_restart.status = 'passed'
    } else {
        Write-Output 'AudioSrv restart: not run; pass -AllowAudioServiceRestart for this state-mutating row.'
    }

    if ($AllowDeviceToggle) {
        Write-Output 'Device disable/enable is intentionally delegated to lifecycle-validation.ps1 so the exact devnode target is visible.'
        Write-Output 'Run lifecycle-validation.ps1 -Phase DisableEnable -Execute separately and attach its evidence.'
        $script:stressRows.device_toggle.status = 'delegated'
    } else {
        Write-Output 'Device disable/enable: not run; use lifecycle-validation.ps1 explicitly.'
    }

    Write-Output 'Driver stress matrix completed. Preserve this output with Verifier and event-log evidence.'
    Write-StressEvidence $true $null
} catch {
    $script:stressFailure = $_.Exception.Message
    Write-StressEvidence $false $script:stressFailure
    throw
}
