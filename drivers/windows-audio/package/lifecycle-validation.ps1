#requires -Version 5.1

[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'High')]
param(
    [Parameter(Mandatory = $false)]
    [ValidateSet('Plan', 'SleepResume', 'DisableEnable', 'AudioService', 'All')]
    [string] $Phase = 'Plan',
    [Parameter(Mandatory = $false)]
    [string] $PackageRoot,
    [Parameter(Mandatory = $false)]
    [string] $SmokeProbe,
    [Parameter(Mandatory = $false)]
    [ValidateRange(5, 300)]
    [int] $TimeoutSeconds = 30,
    [Parameter(Mandatory = $false)]
    [switch] $Execute,
    [Parameter(Mandatory = $false)]
    [switch] $AllowSuspend,
    [Parameter(Mandatory = $false)]
    [switch] $AllowAudioServiceRestart
)

$ErrorActionPreference = 'Stop'

# This script never mutates a device or power state unless both -Execute and
# the corresponding phase are supplied. It always targets this exact package
# devnode; it never searches for or disables an unrelated device.
$rootDeviceInstanceId = 'ROOT\DEVGEN\QPWGRAPH_AUDIO'

function Find-RepositoryRoot([string] $StartingPath) {
    $current = Get-Item -LiteralPath $StartingPath -ErrorAction Stop
    while ($null -ne $current) {
        if (Test-Path -LiteralPath (Join-Path $current.FullName 'drivers\windows-audio\Cargo.toml') -PathType Leaf) {
            return $current.FullName
        }
        $current = $current.Parent
    }
    return $null
}

function Resolve-SmokeProbe([string] $RequestedPath) {
    if (-not [string]::IsNullOrWhiteSpace($RequestedPath)) {
        $resolved = (Resolve-Path -LiteralPath $RequestedPath -ErrorAction Stop).Path
        if (-not (Test-Path -LiteralPath $resolved -PathType Leaf)) {
            throw "Smoke probe was not found: $RequestedPath"
        }
        return $resolved
    }

    $repositoryRoot = Find-RepositoryRoot $PSScriptRoot
    if ($null -ne $repositoryRoot) {
        $candidate = Join-Path $repositoryRoot 'drivers\windows-audio\target\debug\qpwgraph-audio-smoke.exe'
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            return (Resolve-Path -LiteralPath $candidate -ErrorAction Stop).Path
        }
    }
    return $null
}

function Assert-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Lifecycle validation requires an elevated PowerShell prompt when -Execute is used.'
    }
}

function Invoke-Smoke([string[]] $Arguments) {
    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = @(& $script:smokePath @Arguments 2>&1)
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    if ($output.Count -gt 0) {
        Write-Verbose (($output | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine)
    }
    if ($exitCode -ne 0) {
        $details = ($output | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine
        throw "Smoke probe $($Arguments -join ' ') failed with exit code $exitCode. $details"
    }
}

function Wait-Smoke([string[]] $Arguments, [string] $Description) {
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $lastError = 'the smoke probe did not run'
    do {
        try {
            Invoke-Smoke $Arguments
            Write-Output "$Description passed."
            return
        } catch {
            $lastError = $_.Exception.Message
            if ((Get-Date) -ge $deadline) {
                throw "$Description did not pass before the timeout: $lastError"
            }
            Start-Sleep -Seconds 1
        }
    } while ($true)
}

function Get-QpwgraphRootDevice {
    $getPnpDevice = Get-Command -Name 'Get-PnpDevice' -CommandType Cmdlet -ErrorAction SilentlyContinue
    if ($null -eq $getPnpDevice) {
        throw 'Get-PnpDevice is unavailable; run lifecycle validation from Windows PowerShell with the PnpDevice module.'
    }
    return Get-PnpDevice -InstanceId $rootDeviceInstanceId -ErrorAction SilentlyContinue |
        Select-Object -First 1
}

function Invoke-PnpTool([string[]] $Arguments, [string] $Description) {
    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = @(& pnputil.exe @Arguments 2>&1)
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    if ($output.Count -gt 0) {
        Write-Verbose (($output | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine)
    }
    if ($exitCode -ne 0) {
        $details = ($output | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine
        throw "$Description failed with exit code $exitCode. $details"
    }
}

function Set-QpwgraphRootDeviceEnabled([bool] $Enabled) {
    $verb = if ($Enabled) { 'enable' } else { 'disable' }
    $cmdletName = if ($Enabled) { 'Enable-PnpDevice' } else { 'Disable-PnpDevice' }
    $pnpCmdlet = Get-Command -Name $cmdletName -CommandType Cmdlet -ErrorAction SilentlyContinue
    if ($null -ne $pnpCmdlet) {
        if ($Enabled) {
            Enable-PnpDevice -InstanceId $rootDeviceInstanceId -Confirm:$false -ErrorAction Stop | Out-Null
        } else {
            Disable-PnpDevice -InstanceId $rootDeviceInstanceId -Confirm:$false -ErrorAction Stop | Out-Null
        }
    } else {
        Invoke-PnpTool @("/$verb-device", $rootDeviceInstanceId) "PnPUtil $verb-device"
    }
}

function Invoke-DisableEnable {
    $device = Get-QpwgraphRootDevice
    if ($null -eq $device) {
        throw "The exact QPWGraph root devnode $rootDeviceInstanceId was not found. Install the package first."
    }

    Wait-Smoke @('--verify-roles') 'Pre-disable endpoint-role verification'
    Wait-Smoke @('--verify-cables', '--duration-ms', '2000') 'Pre-disable cable verification'

    $disabled = $false
    try {
        Set-QpwgraphRootDeviceEnabled $false
        $disabled = $true
        Write-Output "Disabled exact device $rootDeviceInstanceId."
        Wait-Smoke @('--verify-absent') 'Disabled endpoint absence verification'

        Set-QpwgraphRootDeviceEnabled $true
        $disabled = $false
        Write-Output "Enabled exact device $rootDeviceInstanceId."
        Wait-Smoke @('--verify-roles') 'Post-enable endpoint-role verification'
        Wait-Smoke @('--verify-cables', '--duration-ms', '2000') 'Post-enable cable verification'
    } finally {
        if ($disabled) {
            try {
                Set-QpwgraphRootDeviceEnabled $true
                Write-Output "Recovered exact device $rootDeviceInstanceId after a validation failure."
            } catch {
                Write-Error "Lifecycle validation could not re-enable ${rootDeviceInstanceId}: $($_.Exception.Message)"
            }
        }
    }
}

function Invoke-AudioServiceRestart {
    Wait-Smoke @('--verify-roles') 'Pre-AudioSrv endpoint-role verification'
    Wait-Smoke @('--verify-cables', '--duration-ms', '2000') 'Pre-AudioSrv cable verification'
    Write-Output 'Restarting the exact Windows Audio service Audiosrv.'
    Restart-Service -Name 'Audiosrv' -Force
    Start-Sleep -Seconds 2
    Wait-Smoke @('--verify-roles') 'Post-AudioSrv endpoint-role verification'
    Wait-Smoke @('--verify-cables', '--duration-ms', '2000') 'Post-AudioSrv cable verification'
}

function Invoke-SleepResume {
    if (-not ('QpwgraphPowerTransition' -as [type])) {
        Add-Type @'
using System;
using System.Runtime.InteropServices;

public static class QpwgraphPowerTransition
{
    [DllImport("PowrProf.dll", SetLastError = true)]
    public static extern bool SetSuspendState(
        bool hibernate,
        bool forceCritical,
        bool disableWakeEvent);
}

'@
    }

    Wait-Smoke @('--verify-roles') 'Pre-suspend endpoint-role verification'
    Wait-Smoke @('--verify-cables', '--duration-ms', '2000') 'Pre-suspend cable verification'
    Write-Output 'Suspending this computer now; manually resume it to continue validation.'
    if (-not [QpwgraphPowerTransition]::SetSuspendState($false, $false, $false)) {
        throw 'Windows refused the suspend request. No lifecycle result was recorded.'
    }
    Start-Sleep -Seconds 2
    Wait-Smoke @('--verify-roles') 'Post-resume endpoint-role verification'
    Wait-Smoke @('--verify-cables', '--duration-ms', '2000') 'Post-resume cable verification'
}

$packageRootPath = if ([string]::IsNullOrWhiteSpace($PackageRoot)) {
    (Resolve-Path -LiteralPath $PSScriptRoot -ErrorAction Stop).Path
} else {
    (Resolve-Path -LiteralPath $PackageRoot -ErrorAction Stop).Path
}
$script:smokePath = Resolve-SmokeProbe $SmokeProbe

Write-Output "QPWGraph Windows lifecycle validation: phase=$Phase package=$packageRootPath"
Write-Output "Exact device target: $rootDeviceInstanceId"
if ($null -ne $script:smokePath) {
    Write-Output "Smoke probe: ${script:smokePath}"
} else {
    Write-Output 'Smoke probe: not found (pass -SmokeProbe or build the nested smoke probe)'
}

if ($Phase -eq 'Plan' -or -not $Execute) {
    Write-Output 'Plan-only mode: no device, endpoint, boot, or power state will be changed.'
    Write-Output 'Execute disable/enable: -Phase DisableEnable -Execute'
    Write-Output 'Execute suspend/resume: -Phase SleepResume -Execute -AllowSuspend'
    Write-Output 'Execute AudioSrv recovery: -Phase AudioService -Execute -AllowAudioServiceRestart'
    Write-Output 'Execute both: -Phase All -Execute -AllowSuspend'
    exit 0
}

if ($null -eq $script:smokePath) {
    throw 'An executable qpwgraph-audio-smoke probe is required for lifecycle validation.'
}
if ($WhatIfPreference) {
    Write-Output 'WhatIf mode: no administrator check or lifecycle command will be executed.'
    exit 0
}
Assert-Administrator
if ($Phase -eq 'SleepResume' -or $Phase -eq 'All') {
    if (-not $AllowSuspend) {
        throw 'Sleep/resume requires the explicit -AllowSuspend switch in addition to -Execute.'
    }
}
if ($Phase -eq 'AudioService' -or $Phase -eq 'All') {
    if (-not $AllowAudioServiceRestart) {
        throw 'AudioSrv recovery requires the explicit -AllowAudioServiceRestart switch in addition to -Execute.'
    }
}

if ($Phase -eq 'DisableEnable' -or $Phase -eq 'All') {
    Invoke-DisableEnable
}
if ($Phase -eq 'SleepResume' -or $Phase -eq 'All') {
    Invoke-SleepResume
}
if ($Phase -eq 'AudioService' -or $Phase -eq 'All') {
    Invoke-AudioServiceRestart
}

Write-Output 'Lifecycle validation completed. Preserve the command output as acceptance evidence.'
exit 0
