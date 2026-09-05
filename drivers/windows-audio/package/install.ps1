[CmdletBinding()]
param(
    [Parameter(Mandatory = $false)]
    [string] $PackageRoot = $PSScriptRoot,
    [Parameter(Mandatory = $false)]
    [switch] $AllowTestSigned
)

$ErrorActionPreference = 'Stop'

function Assert-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Driver installation requires an elevated PowerShell prompt.'
    }
}

Assert-Administrator
$inf = Join-Path (Resolve-Path -LiteralPath $PackageRoot) 'qpwgraph-audio.inf'
if (-not (Test-Path -LiteralPath $inf -PathType Leaf)) {
    throw "Signed package is incomplete: $inf was not found. Build the INF/CAT from an eWDK release job first."
}
$manifest = Join-Path (Resolve-Path -LiteralPath $PackageRoot) 'manifest.json'
if (-not (Test-Path -LiteralPath $manifest -PathType Leaf)) {
    throw "Signed package is incomplete: $manifest was not found."
}
$metadata = Get-Content -LiteralPath $manifest -Raw | ConvertFrom-Json
if ($metadata.implementation_status -ne 'ready') {
    throw "This package is fail-closed bootstrap metadata and does not expose an audio endpoint yet. Build a signed ACX release package before installing it."
}

if (-not $AllowTestSigned) {
    Write-Verbose 'Installing the package through PnPUtil; Windows signature policy remains enabled.'
}

# PnPUtil installs the device package and is intentionally the only privileged
# operation here. It does not select a default render/capture endpoint.
& pnputil.exe /add-driver $inf /install
if ($LASTEXITCODE -ne 0) {
    throw "PnPUtil failed with exit code $LASTEXITCODE. No Windows default device was changed."
}
Write-Output 'QPWGraph audio driver package installed; Windows default devices were not changed.'
