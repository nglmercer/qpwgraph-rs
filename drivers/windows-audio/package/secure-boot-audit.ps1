#requires -Version 5.1

[CmdletBinding()]
param(
    [Parameter(Mandatory = $false)]
    [string] $PackageRoot,
    [Parameter(Mandatory = $false)]
    [string] $SmokeProbe,
    [Parameter(Mandatory = $false)]
    [string] $OutputPath
)

$ErrorActionPreference = 'Stop'

Write-Output 'QPWGraph Secure Boot audit'
Write-Output '  READ-ONLY: reports Secure Boot, boot-signing, package-signature, PnP, and endpoint state.'
Write-Output '  STATE-MUTATING: only -OutputPath writes an audit file.'
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

function Get-SecureBootState {
    try {
        return [bool](Confirm-SecureBootUEFI -ErrorAction Stop)
    } catch {
        $value = Get-ItemProperty -LiteralPath 'HKLM:\SYSTEM\CurrentControlSet\Control\SecureBoot\State' `
            -Name 'UEFISecureBootEnabled' -ErrorAction SilentlyContinue
        if ($null -ne $value) {
            return ([int]$value.UEFISecureBootEnabled) -eq 1
        }
        return $null
    }
}

function Invoke-Captured([string] $FileName, [string[]] $Arguments) {
    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = @(& $FileName @Arguments 2>&1) | ForEach-Object { $_.ToString() }
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    return [pscustomobject]@{
        output = @($output)
        exit_code = $exitCode
    }
}

function Get-TestSigningState {
    $command = Get-Command -Name 'bcdedit.exe' -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -eq $command) {
        return [pscustomobject]@{ available = $false; enabled = $null; output = @() }
    }
    # Enumerating the complete store is more portable than passing the
    # optional {current} identifier: on some UEFI/non-elevated sessions the
    # latter is rejected before bcdedit reports the actual access state.
    $captured = Invoke-Captured $command.Source @('/enum')
    $output = @($captured.output)
    $text = $output -join [Environment]::NewLine
    $enabled = $null
    if ($text -match '(?im)^\s*testsigning\s+Yes\s*$') { $enabled = $true }
    if ($text -match '(?im)^\s*testsigning\s+No\s*$') { $enabled = $false }
    return [pscustomobject]@{ available = $true; enabled = $enabled; exit_code = $captured.exit_code; output = @($output) }
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

function Get-OptionalSignature([string] $Path) {
    $command = Get-Command -Name 'Get-AuthenticodeSignature' -CommandType Cmdlet -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -eq $command) {
        return [pscustomobject]@{ Status = 'Unavailable'; SignerCertificate = $null; Issuer = $null }
    }
    return Get-AuthenticodeSignature -LiteralPath $Path
}

function Get-PackageSignatures([string] $Root) {
    $items = @()
    foreach ($name in @('qpwgraph_audio.sys', 'qpwgraph-audio.cat', 'qpwgraph-audio.inf')) {
        $path = Join-Path $Root $name
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            $signature = Get-OptionalSignature $path
            $items += [pscustomobject]@{
                name = $name
                sha256 = Get-Sha256 $path
                signature_status = [string]$signature.Status
                signer = if ($null -ne $signature.SignerCertificate) { $signature.SignerCertificate.Subject } else { $null }
                issuer = if ($null -ne $signature.SignerCertificate) { $signature.SignerCertificate.Issuer } else { $null }
            }
        } else {
            $items += [pscustomobject]@{ name = $name; sha256 = $null; signature_status = 'Missing'; signer = $null; issuer = $null }
        }
    }
    return @($items)
}

$root = Resolve-PackageRoot $PackageRoot
$signing = Get-TestSigningState
$operatingSystem = Get-CimInstance -ClassName Win32_OperatingSystem -ErrorAction SilentlyContinue |
    Select-Object Caption, Version, BuildNumber, OSArchitecture, LastBootUpTime
$service = Get-CimInstance -ClassName Win32_SystemDriver -Filter "Name='qpwgraph_audio'" -ErrorAction SilentlyContinue |
    Select-Object Name, State, StartMode, PathName, ServiceType
$devices = @()
$pnp = Get-Command -Name 'Get-PnpDevice' -CommandType Cmdlet -ErrorAction SilentlyContinue
if ($null -ne $pnp) {
    $devices = @(Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
        Where-Object { $_.FriendlyName -match '(?i)QPWGraph|QPW' -or $_.InstanceId -match '(?i)QPWGraph|QPW' } |
        Select-Object Status, Problem, Class, FriendlyName, InstanceId)
}

$smoke = [pscustomobject]@{ requested = $SmokeProbe; available = $false; exit_code = $null; output = @() }
if (-not [string]::IsNullOrWhiteSpace($SmokeProbe) -and (Test-Path -LiteralPath $SmokeProbe -PathType Leaf)) {
    $smokeResult = Invoke-Captured (Resolve-Path -LiteralPath $SmokeProbe).Path @('--verify-roles')
    $smokeOutput = @($smokeResult.output)
    $smoke = [pscustomobject]@{
        requested = (Resolve-Path -LiteralPath $SmokeProbe).Path
        available = $true
        exit_code = $smokeResult.exit_code
        output = @($smokeOutput)
    }
}

$audit = [ordered]@{
    schema = 1
    kind = 'qpwgraph-secure-boot-audit'
    collected_utc = [DateTime]::UtcNow.ToString('o')
    package_root = $root
    secure_boot_enabled = Get-SecureBootState
    test_signing = $signing
    operating_system = $operatingSystem
    package_signatures = Get-PackageSignatures $root
    installed_provider = $service
    provider_devices = $devices
    endpoint_role_probe = $smoke
    expected_roles = @('app-render', 'app-monitor', 'relay-render', 'relay-capture')
    notes = @(
        'Test-signed success is not Microsoft production-signing evidence.'
        'Production compatibility requires the returned Microsoft-signed package and a Secure Boot live install/reboot/uninstall run.'
        'This audit never changes defaults, boot options, devices, or package state.'
    )
}

$json = $audit | ConvertTo-Json -Depth 10
if (-not [string]::IsNullOrWhiteSpace($OutputPath)) {
    $parent = Split-Path -Parent $OutputPath
    if (-not [string]::IsNullOrWhiteSpace($parent)) {
        New-Item -ItemType Directory -Path $parent -Force | Out-Null
    }
    Set-Content -LiteralPath $OutputPath -Value $json -Encoding UTF8
    Write-Output "Secure Boot audit written to $((Resolve-Path -LiteralPath $OutputPath).Path)."
} else {
    Write-Output $json
}
