#requires -Version 5.1

[CmdletBinding()]
param(
    [Parameter(Mandatory = $false)]
    [string] $RepositoryRoot,
    [Parameter(Mandatory = $false)]
    [string] $ApplicationBinary,
    [Parameter(Mandatory = $false)]
    [string] $DriverPackage,
    [Parameter(Mandatory = $false)]
    [string] $OutputDirectory,
    [Parameter(Mandatory = $false)]
    [string] $Version,
    [Parameter(Mandatory = $false)]
    [switch] $AllowTestSigned
)

$ErrorActionPreference = 'Stop'

Write-Output 'QPWGraph full Windows bundle builder'
Write-Output '  READ-ONLY: validates the application and driver inputs.'
Write-Output '  STATE-MUTATING: writes a self-contained ZIP under the output directory.'
Write-Output '  REBOOT-REQUIRING: none; installation is a separate explicit action.'

function Find-RepositoryRoot([string] $RequestedRoot) {
    if (-not [string]::IsNullOrWhiteSpace($RequestedRoot)) {
        return (Resolve-Path -LiteralPath $RequestedRoot -ErrorAction Stop).Path
    }
    return (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
}

function Get-Version([string] $Requested, [string] $Root) {
    if (-not [string]::IsNullOrWhiteSpace($Requested)) { return $Requested }
    $cargo = Join-Path $Root 'Cargo.toml'
    $match = Select-String -LiteralPath $cargo -Pattern '^version\s*=\s*"([^"]+)"' |
        Select-Object -First 1
    if ($null -eq $match) { throw 'Could not determine the application version; pass -Version.' }
    return $match.Matches[0].Groups[1].Value
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

$root = Find-RepositoryRoot $RepositoryRoot
$app = if ([string]::IsNullOrWhiteSpace($ApplicationBinary)) {
    Join-Path $root 'target\release\qpwgraph-rs.exe'
} else {
    (Resolve-Path -LiteralPath $ApplicationBinary -ErrorAction Stop).Path
}
$driver = if ([string]::IsNullOrWhiteSpace($DriverPackage)) {
    Join-Path $root 'drivers\windows-audio\target\qpwgraph-audio-package'
} else {
    (Resolve-Path -LiteralPath $DriverPackage -ErrorAction Stop).Path
}
$output = if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    Join-Path $root 'dist'
} else {
    [IO.Path]::GetFullPath($OutputDirectory)
}
$versionValue = Get-Version $Version $root

if (-not (Test-Path -LiteralPath $app -PathType Leaf)) { throw "Application binary was not found: $app" }
if (-not (Test-Path -LiteralPath $driver -PathType Container)) { throw "Driver package was not found: $driver" }
$driverManifestPath = Join-Path $driver 'manifest.json'
$driverManifest = Get-Content -LiteralPath $driverManifestPath -Raw | ConvertFrom-Json
if ([string]$driverManifest.implementation_status -ne 'ready') {
    throw "Full bundles require a ready driver package; found $($driverManifest.implementation_status)."
}
if (-not $AllowTestSigned -and [string]$driverManifest.driver_runtime -ne 'rust-only') {
    throw "Production full bundles require a Rust-only driver candidate; found driver_runtime=$($driverManifest.driver_runtime). Use -AllowTestSigned only for a development bundle."
}
foreach ($name in @('qpwgraph_audio.sys', 'qpwgraph-audio.inf', 'qpwgraph-audio.cat', 'install.ps1', 'uninstall.ps1')) {
    if (-not (Test-Path -LiteralPath (Join-Path $driver $name) -PathType Leaf)) {
        throw "Full bundle driver package is missing $name."
    }
}

$sysSignature = Get-AuthenticodeSignature -LiteralPath (Join-Path $driver 'qpwgraph_audio.sys')
$catSignature = Get-AuthenticodeSignature -LiteralPath (Join-Path $driver 'qpwgraph-audio.cat')
if (-not $AllowTestSigned -and ([string]$sysSignature.Status -ne 'Valid' -or [string]$catSignature.Status -ne 'Valid')) {
    throw 'Full release bundling requires valid SYS and CAT signatures. Use -AllowTestSigned only for an explicitly marked development bundle.'
}

New-Item -ItemType Directory -Path $output -Force | Out-Null
$staging = Join-Path ([IO.Path]::GetTempPath()) ("qpwgraph-full-{0}" -f ([guid]::NewGuid().ToString('N')))
$packageName = "qpwgraph-rs-$versionValue-x86_64-pc-windows-msvc-full"
$packageRoot = Join-Path $staging $packageName
New-Item -ItemType Directory -Path (Join-Path $packageRoot 'driver') -Force | Out-Null
Copy-Item -LiteralPath $app -Destination (Join-Path $packageRoot 'qpwgraph-rs.exe')
Copy-Item -LiteralPath (Join-Path $root 'README.md') -Destination (Join-Path $packageRoot 'README.md')
Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination (Join-Path $packageRoot 'LICENSE')
Copy-Item -LiteralPath (Join-Path $root 'packaging\windows\install-full.ps1') -Destination (Join-Path $packageRoot 'install-full.ps1')
Copy-Item -LiteralPath (Join-Path $root 'packaging\windows\uninstall-full.ps1') -Destination (Join-Path $packageRoot 'uninstall-full.ps1')

$driverDestination = Join-Path $packageRoot 'driver'
Get-ChildItem -LiteralPath $driver -File | ForEach-Object {
    Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $driverDestination $_.Name)
}

$releaseNotes = @"
# QPWGraph full Windows bundle

This bundle contains the portable qpwgraph-rs executable and the optional
QPWGraph virtual-audio driver package. The driver is never required for the
portable application to start. Installing it does not change Windows default
audio devices automatically.

Driver version: $([string]$driverManifest.driver_version)
Driver implementation status: $([string]$driverManifest.implementation_status)
Driver signature status at bundle time: SYS=$([string]$sysSignature.Status), CAT=$([string]$catSignature.Status)

Run install-full.ps1 from an elevated PowerShell prompt. Record the exact
published oemNN.inf name it prints; uninstall-full.ps1 requires that exact
name. Review the driver README and run the Secure Boot, Verifier, and client
acceptance procedures before treating this as a production release.
"@
Set-Content -LiteralPath (Join-Path $packageRoot 'DRIVER-NOTICES.md') -Value $releaseNotes -Encoding UTF8

$archive = Join-Path $output "$packageName.zip"
if (Test-Path -LiteralPath $archive -PathType Leaf) { Remove-Item -LiteralPath $archive -Force }
Compress-Archive -Path (Join-Path $packageRoot '*') -DestinationPath $archive -CompressionLevel Optimal
$archiveHash = Get-Sha256 $archive
Set-Content -LiteralPath "$archive.sha256" -Value ("{0}  {1}" -f $archiveHash, (Split-Path -Leaf $archive)) -Encoding ASCII
Write-Output "Full bundle written to $archive"
Write-Output "Full bundle SHA256: $archiveHash"
