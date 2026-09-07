#requires -Version 5.1

[CmdletBinding()]
param(
    [Parameter(Mandatory = $false)]
    [string] $Driver = 'qpwgraph_audio.sys',
    [Parameter(Mandatory = $false)]
    [string] $OutputPath,
    [Parameter(Mandatory = $false)]
    [string] $PackageRoot
)

$ErrorActionPreference = 'Stop'

Write-Output 'Driver Verifier evidence collection'
Write-Output '  READ-ONLY: queries Verifier, boot, PnP, and package state; event-log review is separate.'
Write-Output '  STATE-MUTATING: none.'
Write-Output '  REBOOT-REQUIRING: none.'

function Invoke-Captured([string] $FileName, [string[]] $Arguments) {
    $command = Get-Command -Name $FileName -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -eq $command) {
        return [pscustomobject]@{ available = $false; output = @(); exit_code = $null }
    }
    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = @(& $command.Source @Arguments 2>&1) | ForEach-Object { $_.ToString() }
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    return [pscustomobject]@{
        available = $true
        output = @($output)
        exit_code = $exitCode
    }
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
        return [pscustomobject]@{ Status = 'Unavailable'; SignerCertificate = $null }
    }
    return Get-AuthenticodeSignature -LiteralPath $Path
}

function Get-OptionalSecureBootState {
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

$packageRootPath = $null
if (-not [string]::IsNullOrWhiteSpace($PackageRoot)) {
    $packageRootPath = (Resolve-Path -LiteralPath $PackageRoot -ErrorAction Stop).Path
}

$verifier = Invoke-Captured 'verifier.exe' @('/querysettings')
$verifierQuery = Invoke-Captured 'verifier.exe' @('/query')
$boot = Invoke-Captured 'bcdedit.exe' @('/enum')
$system = Get-CimInstance -ClassName Win32_OperatingSystem -ErrorAction SilentlyContinue |
    Select-Object Caption, Version, BuildNumber, OSArchitecture, LastBootUpTime
$installedDriver = Get-CimInstance -ClassName Win32_SystemDriver -Filter "Name='qpwgraph_audio'" -ErrorAction SilentlyContinue |
    Select-Object Name, State, StartMode, PathName, ServiceType
$devices = @()
$pnp = Get-Command -Name 'Get-PnpDevice' -CommandType Cmdlet -ErrorAction SilentlyContinue
if ($null -ne $pnp) {
    $devices = @(Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
        Where-Object { $_.FriendlyName -match '(?i)QPWGraph|QPW' -or $_.InstanceId -match '(?i)QPWGraph|QPW' } |
        Select-Object Status, Problem, Class, FriendlyName, InstanceId)
}

$packageFiles = @()
if ($null -ne $packageRootPath) {
    foreach ($name in @('qpwgraph_audio.sys', 'qpwgraph-audio.inf', 'qpwgraph-audio.cat', 'manifest.json')) {
        $path = Join-Path $packageRootPath $name
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            $signature = Get-OptionalSignature $path
            $packageFiles += [pscustomobject]@{
                name = $name
                sha256 = Get-Sha256 $path
                signature_status = [string]$signature.Status
                signer = if ($null -ne $signature.SignerCertificate) { $signature.SignerCertificate.Subject } else { $null }
            }
        }
    }
}

$evidence = [ordered]@{
    schema = 1
    kind = 'qpwgraph-windows-verifier-evidence'
    collected_utc = [DateTime]::UtcNow.ToString('o')
    driver = $Driver
    secure_boot_enabled = Get-OptionalSecureBootState
    operating_system = $system
    verifier_querysettings = $verifier
    verifier_query = $verifierQuery
    boot_configuration = $boot
    installed_service = $installedDriver
    provider_devices = $devices
    package_root = $packageRootPath
    package_files = $packageFiles
    notes = @(
        'This record is evidence of observed machine state, not proof that the required stress matrix passed.'
        'Attach the run-driver-stress.ps1 output and event-log/bugcheck review separately.'
    )
}

$json = $evidence | ConvertTo-Json -Depth 8
if (-not [string]::IsNullOrWhiteSpace($OutputPath)) {
    $parent = Split-Path -Parent $OutputPath
    if (-not [string]::IsNullOrWhiteSpace($parent)) {
        New-Item -ItemType Directory -Path $parent -Force | Out-Null
    }
    Set-Content -LiteralPath $OutputPath -Value $json -Encoding UTF8
    Write-Output "Evidence written to $((Resolve-Path -LiteralPath $OutputPath).Path)."
} else {
    Write-Output $json
}
