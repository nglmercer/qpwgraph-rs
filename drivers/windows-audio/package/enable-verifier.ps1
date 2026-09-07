#requires -Version 5.1

[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'High')]
param(
    [Parameter(Mandatory = $false)]
    [string] $Driver = 'qpwgraph_audio.sys',
    [Parameter(Mandatory = $false)]
    [uint32] $Flags = 0x0000033b,
    [Parameter(Mandatory = $true)]
    [switch] $ConfirmEnable,
    [Parameter(Mandatory = $false)]
    [switch] $Reboot,
    [Parameter(Mandatory = $false)]
    [switch] $AllowReboot
)

$ErrorActionPreference = 'Stop'

Write-Output 'Driver Verifier enable operation'
Write-Output '  STATE-MUTATING: changes global Driver Verifier configuration.'
Write-Output '  REBOOT-REQUIRING: a reboot is normally required before the settings are active.'
Write-Output '  READ-ONLY: this script does not install, uninstall, or change the audio driver.'

if (-not $ConfirmEnable) {
    throw 'Refusing to enable Driver Verifier without the explicit -ConfirmEnable switch.'
}
if ($Flags -eq 0) {
    throw 'Refusing to enable Driver Verifier with an empty flag mask.'
}
if ($Reboot -and -not $AllowReboot) {
    throw 'Refusing to reboot without both -Reboot and -AllowReboot.'
}
if ($WhatIfPreference) {
    Write-Output "WhatIf mode: Driver Verifier was not configured for $Driver and the computer was not rebooted."
    return
}

$verifier = Get-Command -Name 'verifier.exe' -CommandType Application -ErrorAction SilentlyContinue |
    Select-Object -First 1
if ($null -eq $verifier) {
    throw 'verifier.exe was not found on PATH.'
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Enabling Driver Verifier requires an elevated PowerShell prompt.'
}

$flagText = ('0x{0:x8}' -f $Flags)
$arguments = @('/flags', $flagText, '/driver', $Driver)
Write-Output 'Requested checks: Special Pool; Force IRQL checking; Pool Tracking; I/O Verification; Deadlock Detection; Security Checks; Miscellaneous Checks; WDF verification through the KMDF verifier integration.'
Write-Output "The exact mask is caller-controlled: $flagText. Verify the effective mask with verifier /querysettings before the stress run."
& $verifier.Source @arguments
if ($LASTEXITCODE -ne 0) {
    throw "verifier.exe failed with exit code $LASTEXITCODE."
}
Write-Output "Driver Verifier configured for $Driver with flags $flagText."
Write-Output 'Record verifier /querysettings output before rebooting the disposable test machine.'

if ($Reboot) {
    Restart-Computer -Force
}
