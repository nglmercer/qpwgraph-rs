[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^oem[0-9]+\.inf$')]
    [string] $PublishedInf
)

$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Driver removal requires an elevated PowerShell prompt.'
}

if ($PSCmdlet.ShouldProcess($PublishedInf, 'remove the QPWGraph audio driver package')) {
    & pnputil.exe /delete-driver $PublishedInf /uninstall
    if ($LASTEXITCODE -ne 0) {
        throw "PnPUtil failed with exit code $LASTEXITCODE. The package may still be in use."
    }
    Write-Output 'QPWGraph audio driver package removed; Windows default devices were not changed.'
}
