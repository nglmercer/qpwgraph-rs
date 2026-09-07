#requires -Version 5.1

[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'High')]
param(
    [Parameter(Mandatory = $true)]
    [string] $PublishedInf,
    [Parameter(Mandatory = $false)]
    [string] $SmokeProbe
)

$ErrorActionPreference = 'Stop'
$driverUninstall = Join-Path $PSScriptRoot 'driver\uninstall.ps1'
if (-not (Test-Path -LiteralPath $driverUninstall -PathType Leaf)) {
    throw "The full bundle driver uninstaller is missing: $driverUninstall"
}

Write-Output 'QPWGraph full bundle uninstallation'
Write-Output '  STATE-MUTATING: removes only the exact published INF supplied by -PublishedInf.'
Write-Output '  REBOOT-REQUIRING: PnP may report that Windows needs a reboot; this wrapper never reboots automatically.'

$arguments = @{ PublishedInf = $PublishedInf }
if (-not [string]::IsNullOrWhiteSpace($SmokeProbe)) { $arguments.SmokeProbe = $SmokeProbe }
if (-not $WhatIfPreference) {
    & $driverUninstall @arguments
    if ($LASTEXITCODE -ne 0) { throw "Driver uninstallation failed with exit code $LASTEXITCODE." }
} else {
    Write-Output "WhatIf mode: published package $PublishedInf was not uninstalled."
}
