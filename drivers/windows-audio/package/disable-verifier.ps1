#requires -Version 5.1

[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'High')]
param(
    [Parameter(Mandatory = $true)]
    [switch] $ConfirmReset,
    [Parameter(Mandatory = $false)]
    [switch] $Reboot,
    [Parameter(Mandatory = $false)]
    [switch] $AllowReboot
)

$ErrorActionPreference = 'Stop'

Write-Output 'Driver Verifier reset operation'
Write-Output '  STATE-MUTATING: resets the machine-wide Driver Verifier configuration.'
Write-Output '  REBOOT-REQUIRING: a reboot is normally required before the reset is active.'
Write-Output '  READ-ONLY: this script does not install, uninstall, or change the audio driver.'

if (-not $ConfirmReset) {
    throw 'Refusing to reset Driver Verifier without the explicit -ConfirmReset switch.'
}
if ($Reboot -and -not $AllowReboot) {
    throw 'Refusing to reboot without both -Reboot and -AllowReboot.'
}
if ($WhatIfPreference) {
    Write-Output 'WhatIf mode: Driver Verifier was not reset and the computer was not rebooted.'
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
    throw 'Resetting Driver Verifier requires an elevated PowerShell prompt.'
}

& $verifier.Source '/reset'
if ($LASTEXITCODE -ne 0) {
    throw "verifier.exe failed with exit code $LASTEXITCODE."
}
Write-Output 'Driver Verifier reset was requested.'

if ($Reboot) {
    Restart-Computer -Force
}
