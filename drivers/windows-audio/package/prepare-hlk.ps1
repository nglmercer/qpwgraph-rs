#requires -Version 5.1

[CmdletBinding()]
param(
    [Parameter(Mandatory = $false)]
    [string] $PackageRoot,
    [Parameter(Mandatory = $false)]
    [string] $OutputPath,
    [Parameter(Mandatory = $false)]
    [switch] $Strict
)

$ErrorActionPreference = 'Stop'

Write-Output 'HLK preparation manifest'
Write-Output '  READ-ONLY: validates the driver package and gathers hashes.'
Write-Output '  STATE-MUTATING: only -OutputPath writes a manifest file; no device or boot state changes.'
Write-Output '  REBOOT-REQUIRING: none.'

function Resolve-PackageRoot([string] $RequestedRoot) {
    if (-not [string]::IsNullOrWhiteSpace($RequestedRoot)) {
        return (Resolve-Path -LiteralPath $RequestedRoot -ErrorAction Stop).Path
    }
    $staged = Join-Path $PSScriptRoot '..\target\qpwgraph-audio-package'
    if (Test-Path -LiteralPath $staged -PathType Container) {
        return (Resolve-Path -LiteralPath $staged).Path
    }
    return (Resolve-Path -LiteralPath $PSScriptRoot).Path
}

function Add-Validation([System.Collections.Generic.List[object]] $Items, [string] $Name, [bool] $Passed, [string] $Detail) {
    $Items.Add([pscustomobject]@{ name = $Name; passed = $Passed; detail = $Detail })
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

$root = Resolve-PackageRoot $PackageRoot
$checks = New-Object 'System.Collections.Generic.List[object]'
$files = @{}
foreach ($name in @('qpwgraph_audio.sys', 'qpwgraph-audio.inf', 'qpwgraph-audio.cat', 'manifest.json')) {
    $path = Join-Path $root $name
    if (Test-Path -LiteralPath $path -PathType Leaf) {
        $files[$name] = [pscustomobject]@{
            path = $path
            sha256 = Get-Sha256 $path
            length = (Get-Item -LiteralPath $path).Length
        }
        Add-Validation $checks "package file: $name" $true $files[$name].sha256
    } else {
        Add-Validation $checks "package file: $name" $false 'missing'
    }
}

foreach ($name in @('qpwgraph_audio.sys', 'qpwgraph-audio.cat')) {
    $path = Join-Path $root $name
    if (Test-Path -LiteralPath $path -PathType Leaf) {
        try {
            $signature = Get-AuthenticodeSignature -LiteralPath $path
            $signer = if ($null -ne $signature.SignerCertificate) {
                $signature.SignerCertificate.Subject
            } else {
                'no signer certificate'
            }
            Add-Validation $checks "signature: $name" ([string]$signature.Status -eq 'Valid') "$($signature.Status); $signer"
        } catch {
            Add-Validation $checks "signature: $name" $false $_.Exception.Message
        }
    }
}

$signtool = Get-Command -Name 'signtool.exe' -CommandType Application -ErrorAction SilentlyContinue |
    Select-Object -First 1
if ($null -ne $signtool -and
    (Test-Path -LiteralPath (Join-Path $root 'qpwgraph-audio.cat') -PathType Leaf) -and
    (Test-Path -LiteralPath (Join-Path $root 'qpwgraph_audio.sys') -PathType Leaf)) {
    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $catalogOutput = @(& $signtool.Source 'verify' '/kp' '/c' (Join-Path $root 'qpwgraph-audio.cat') (Join-Path $root 'qpwgraph_audio.sys') 2>&1) |
            ForEach-Object { $_.ToString() }
        $catalogExit = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    Add-Validation $checks 'catalog membership' ($catalogExit -eq 0) (($catalogOutput -join ' ').Trim())
} else {
    Add-Validation $checks 'catalog membership' $false 'signtool.exe or the SYS/CAT pair was not available'
}

$manifest = $null
$manifestPath = Join-Path $root 'manifest.json'
if ($files.ContainsKey('manifest.json')) {
    try {
        $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    } catch {
        Add-Validation $checks 'manifest JSON' $false $_.Exception.Message
    }
}
if ($null -ne $manifest) {
    Add-Validation $checks 'ready implementation marker' ([string]$manifest.implementation_status -eq 'ready') ([string]$manifest.implementation_status)
    Add-Validation $checks 'driver version' (-not [string]::IsNullOrWhiteSpace([string]$manifest.driver_version)) ([string]$manifest.driver_version)
}

$infText = ''
$infPath = Join-Path $root 'qpwgraph-audio.inf'
if (Test-Path -LiteralPath $infPath -PathType Leaf) {
    $infText = Get-Content -LiteralPath $infPath -Raw
    Add-Validation $checks 'INF KMDF target' ($infText -match '(?im)KmdfLibraryVersion\s*=\s*1\.31') 'KmdfLibraryVersion=1.31'
    Add-Validation $checks 'INF service identity' ($infText -match '(?im)qpwgraph_audio') 'qpwgraph_audio'
    foreach ($role in @('app-render', 'app-monitor', 'relay-render', 'relay-capture')) {
        Add-Validation $checks "INF endpoint role: $role" ($infText -match [regex]::Escape($role)) $role
    }
}

$os = Get-CimInstance -ClassName Win32_OperatingSystem -ErrorAction SilentlyContinue |
    Select-Object Caption, Version, BuildNumber, OSArchitecture
$driverVersion = if ($null -ne $manifest) { [string]$manifest.driver_version } else { $null }
$expectedRoles = [ordered]@{
    'app-render' = 'QPWGraph Virtual Output'
    'app-monitor' = 'QPWGraph Virtual Monitor'
    'relay-render' = 'QPWGraph Relay Sink'
    'relay-capture' = 'QPWGraph Relay Microphone'
}
$document = [ordered]@{
    schema = 1
    kind = 'qpwgraph-windows-hlk-run'
    created_utc = [DateTime]::UtcNow.ToString('o')
    package_root = $root
    driver_version = $driverVersion
    expected_device_instance = 'ROOT\DEVGEN\QPWGRAPH_AUDIO'
    expected_endpoint_roles = $expectedRoles
    operating_system = $os
    package_files = $files
    validation = $checks.ToArray()
    instructions = @(
        'Install this exact package on the HLK test client without changing the Windows default audio device.'
        'Select the relevant Audio and Device Fundamentals tests in HLK Studio.'
        'Export the HLK result package and record its SHA-256 beside this manifest.'
        'This helper prepares evidence; it does not execute HLK tests or mark them passed.'
    )
}

$failed = @($checks | Where-Object { -not $_.passed })
if ($Strict -and $failed.Count -gt 0) {
    throw "HLK package preparation failed validation: $($failed.name -join ', ')"
}

$json = $document | ConvertTo-Json -Depth 10
if (-not [string]::IsNullOrWhiteSpace($OutputPath)) {
    $parent = Split-Path -Parent $OutputPath
    if (-not [string]::IsNullOrWhiteSpace($parent)) {
        New-Item -ItemType Directory -Path $parent -Force | Out-Null
    }
    Set-Content -LiteralPath $OutputPath -Value $json -Encoding UTF8
    Write-Output "HLK manifest written to $((Resolve-Path -LiteralPath $OutputPath).Path)."
} else {
    Write-Output $json
}
