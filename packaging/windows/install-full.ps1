#requires -Version 5.1

[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'High')]
param(
    [Parameter(Mandatory = $false)]
    [string] $SmokeProbe,
    [Parameter(Mandatory = $false)]
    [switch] $AllowTestSigned,
    [Parameter(Mandatory = $false)]
    [switch] $SkipEndpointVerification
)

$ErrorActionPreference = 'Stop'
$driverInstall = Join-Path $PSScriptRoot 'driver\install.ps1'
if (-not (Test-Path -LiteralPath $driverInstall -PathType Leaf)) {
    throw "The full bundle driver installer is missing: $driverInstall"
}

Write-Output 'QPWGraph full bundle installation'
Write-Output '  STATE-MUTATING: installs the optional driver package through the exact package helper.'
Write-Output '  REBOOT-REQUIRING: PnP may report that Windows needs a reboot; this wrapper never reboots automatically.'
Write-Output '  Default audio devices are not changed by this installer.'

$arguments = @{}
if (-not [string]::IsNullOrWhiteSpace($SmokeProbe)) { $arguments.SmokeProbe = $SmokeProbe }
if ($AllowTestSigned) { $arguments.AllowTestSigned = $true }
if ($SkipEndpointVerification) { $arguments.SkipEndpointVerification = $true }
if (-not $WhatIfPreference) {
    & $driverInstall @arguments
    if ($LASTEXITCODE -ne 0) { throw "Driver installation failed with exit code $LASTEXITCODE." }
} else {
    Write-Output 'WhatIf mode: the optional driver was not installed.'
}
