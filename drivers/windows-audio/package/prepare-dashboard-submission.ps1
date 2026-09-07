#requires -Version 5.1

[CmdletBinding()]
param(
    [Parameter(Mandatory = $false)]
    [string] $PackageRoot,
    [Parameter(Mandatory = $true)]
    [string] $OutputDirectory,
    [Parameter(Mandatory = $false)]
    [string] $ReleaseManifest
)

$ErrorActionPreference = 'Stop'

Write-Output 'Hardware Dev Center submission preparation'
Write-Output '  READ-ONLY: validates source package and gathers exact hashes.'
Write-Output '  STATE-MUTATING: writes a submission directory and manifest under -OutputDirectory.'
Write-Output '  REBOOT-REQUIRING: none.'
Write-Output '  SECURITY: no account credentials, tokens, certificates, or private keys are read or written.'

function Resolve-DefaultPackage {
    if (-not [string]::IsNullOrWhiteSpace($PackageRoot)) {
        return (Resolve-Path -LiteralPath $PackageRoot -ErrorAction Stop).Path
    }
    $candidate = Join-Path $PSScriptRoot '..\target\qpwgraph-audio-package'
    return (Resolve-Path -LiteralPath $candidate -ErrorAction Stop).Path
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

$package = Resolve-DefaultPackage
$output = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Path $output -Force | Out-Null
$packageOutput = Join-Path $output 'driver-package'
if (Test-Path -LiteralPath $packageOutput -PathType Container) {
    $existing = @(Get-ChildItem -LiteralPath $packageOutput -Force -ErrorAction Stop)
    if ($existing.Count -ne 0) {
        throw "Submission driver-package is not empty; choose a new OutputDirectory so stale files cannot be uploaded: $packageOutput"
    }
}
New-Item -ItemType Directory -Path $packageOutput -Force | Out-Null

$manifestPath = Join-Path $package 'manifest.json'
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if ([string]$manifest.implementation_status -ne 'ready') {
    throw "Only a ready package can be prepared for submission; found $($manifest.implementation_status)."
}

$artifacts = [ordered]@{}
foreach ($name in @('qpwgraph_audio.sys', 'qpwgraph-audio.inf', 'qpwgraph-audio.cat', 'manifest.json')) {
    $source = Join-Path $package $name
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "Submission package is missing $name."
    }
    Copy-Item -LiteralPath $source -Destination (Join-Path $packageOutput $name) -Force
    $artifacts[$name] = [ordered]@{ sha256 = (Get-Sha256 $source); length = (Get-Item -LiteralPath $source).Length }
}

$release = $null
if (-not [string]::IsNullOrWhiteSpace($ReleaseManifest)) {
    $release = Get-Content -LiteralPath (Resolve-Path -LiteralPath $ReleaseManifest -ErrorAction Stop) -Raw | ConvertFrom-Json
} else {
    $candidate = Join-Path $package 'release-manifest.json'
    if (Test-Path -LiteralPath $candidate -PathType Leaf) {
        $release = Get-Content -LiteralPath $candidate -Raw | ConvertFrom-Json
    }
}
if ($null -eq $release -or [string]$release.kind -ne 'qpwgraph-windows-driver-release-candidate') {
    throw 'Dashboard submission requires the release-manifest.json produced by build-release-driver.ps1.'
}
if ([string]$release.driver_runtime -ne 'rust-only') {
    throw "Dashboard submission requires a Rust-only driver candidate; found driver_runtime=$($release.driver_runtime)."
}

$submission = [ordered]@{
    schema = 1
    kind = 'qpwgraph-windows-hardware-dashboard-submission'
    prepared_utc = [DateTime]::UtcNow.ToString('o')
    git_commit = if ($null -ne $release) { [string]$release.git_commit } else { $null }
    driver_version = [string]$manifest.driver_version
    source_release_manifest = if ($null -ne $release) { $release.kind } else { $null }
    package_files = $artifacts
    expected_service = 'qpwgraph_audio'
    expected_device_instance = 'ROOT\DEVGEN\QPWGRAPH_AUDIO'
    submission_steps = @(
        'Create or select the organization Hardware Dev Center account outside this repository.'
        'Upload the driver-package directory using the account-controlled dashboard.'
        'Retain the returned submission identifier and signed package outside the source tree.'
        'Run verify-returned-driver.ps1 against the returned package before Secure Boot smoke tests.'
    )
    secrets_policy = 'No credentials or signing secrets are accepted by this script.'
}
Set-Content -LiteralPath (Join-Path $output 'submission-manifest.json') -Value ($submission | ConvertTo-Json -Depth 10) -Encoding UTF8
Set-Content -LiteralPath (Join-Path $output 'SUBMISSION-README.txt') -Value @"
This directory is a deterministic Microsoft Hardware Dev Center submission boundary.

Upload only driver-package to the organization-controlled dashboard. Do not add
certificates, tokens, passwords, EV-token exports, or refresh credentials here.
After Microsoft returns the package, verify it with verify-returned-driver.ps1 and
retain the signed result and its SHA-256 in the release evidence store.
"@ -Encoding UTF8
Write-Output "Submission material prepared at $output."
