[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [Parameter(Mandatory = $false)]
    [string] $PackageRoot,
    [Parameter(Mandatory = $false)]
    [string] $DevGenPath,
    [Parameter(Mandatory = $false)]
    [switch] $AllowTestSigned,
    [Parameter(Mandatory = $false)]
    [string] $SmokeProbe,
    [Parameter(Mandatory = $false)]
    [ValidateRange(1, 300)]
    [int] $VerificationTimeoutSeconds = 30,
    [Parameter(Mandatory = $false)]
    [switch] $SkipEndpointVerification
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($PackageRoot)) {
    # Automatic variables are not reliably populated while PowerShell binds
    # parameter defaults. Resolve the script directory after binding instead.
    $PackageRoot = $PSScriptRoot
}

# DevGen puts root devices under ROOT\DEVGEN; the hardware ID is separate.
$rootDeviceInstanceId = 'ROOT\DEVGEN\QPWGRAPH_AUDIO'
$rootDeviceHardwareId = 'Root\QPWGRAPH_AUDIO'
$script:rootDeviceNeedsCleanup = $false

$packageRootPath = (Resolve-Path -LiteralPath $PackageRoot -ErrorAction Stop).Path
$inf = Join-Path $packageRootPath 'qpwgraph-audio.inf'
$manifest = Join-Path $packageRootPath 'manifest.json'

foreach ($requiredFile in @($inf, $manifest, (Join-Path $packageRootPath 'qpwgraph-audio.cat'), (Join-Path $packageRootPath 'qpwgraph_audio.sys'))) {
    if (-not (Test-Path -LiteralPath $requiredFile -PathType Leaf)) {
        throw "Signed package is incomplete: $requiredFile was not found. Build the INF/CAT/SYS from an eWDK release job first."
    }
}

$metadata = Get-Content -LiteralPath $manifest -Raw | ConvertFrom-Json
if ($metadata.schema -ne 1 -or $metadata.product -ne 'qpwgraph-rs' -or $metadata.driver -ne 'qpwgraph-audio') {
    throw "Unsupported QPWGraph driver manifest in $manifest."
}
if ($metadata.implementation_status -ne 'ready') {
    throw "This package is fail-closed bootstrap metadata and does not expose an audio endpoint yet. Build a signed ACX release package before installing it."
}
if ($metadata.driver_service -ne 'qpwgraph_audio' -or $metadata.changes_default_audio_device -ne $false) {
    throw 'The package manifest does not prove the expected service or default-device policy.'
}
$driverVersion = [string]$metadata.driver_version
if ([string]::IsNullOrWhiteSpace($driverVersion)) {
    throw 'The package manifest does not contain a driver_version.'
}
$expectedRoles = @('app-render', 'app-monitor', 'relay-render', 'relay-capture')
$actualRoles = @($metadata.endpoint_roles | ForEach-Object { [string] $_ })
if (@(Compare-Object -ReferenceObject $expectedRoles -DifferenceObject $actualRoles).Count -ne 0) {
    throw 'The package manifest does not contain exactly the four expected endpoint roles.'
}

function Assert-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Driver installation requires an elevated PowerShell prompt.'
    }
}

function Assert-TestSigningEnabled {
    $settings = (& bcdedit.exe /enum '{current}' 2>$null | Out-String)
    if ($LASTEXITCODE -ne 0 -or $settings -notmatch '(?im)^\s*testsigning\s+Yes\s*$') {
        throw '-AllowTestSigned requires Windows test-signing mode to be enabled for the current boot entry.'
    }
    Write-Warning 'Installing a test-signed development package; this is not a release-signing proof.'
}

function Find-DevGen {
    if (-not [string]::IsNullOrWhiteSpace($DevGenPath)) {
        $resolved = (Resolve-Path -LiteralPath $DevGenPath -ErrorAction Stop).Path
        if (-not (Test-Path -LiteralPath $resolved -PathType Leaf)) {
            throw "DevGen.exe was not found at $DevGenPath."
        }
        return $resolved
    }

    $onPath = Get-Command 'devgen.exe' -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -ne $onPath) {
        return $onPath.Source
    }

    $roots = @()
    if (-not [string]::IsNullOrWhiteSpace($env:WDKContentRoot)) {
        $roots += Join-Path $env:WDKContentRoot 'Tools'
    }
    $roots += 'C:\Program Files (x86)\Windows Kits\10\Tools'
    $roots += 'C:\Program Files\Windows Kits\10\Tools'
    foreach ($root in ($roots | Select-Object -Unique)) {
        if (-not (Test-Path -LiteralPath $root -PathType Container)) {
            continue
        }
        foreach ($architecture in @('x64', 'x86', 'arm64')) {
            $candidate = Get-ChildItem -LiteralPath $root -Filter 'devgen.exe' -Recurse -File -ErrorAction SilentlyContinue |
                Where-Object { $_.Directory.Name -ieq $architecture } |
                Sort-Object FullName -Descending |
                Select-Object -First 1
            if ($null -ne $candidate) {
                return $candidate.FullName
            }
        }
    }
    throw 'DevGen.exe was not found. Install the WDK Tools or pass -DevGenPath with the x64 devgen.exe path.'
}

function Test-QpwgraphRootDevice {
    try {
        return $null -ne (Get-PnpDevice -InstanceId $rootDeviceInstanceId -ErrorAction SilentlyContinue |
            Select-Object -First 1)
    } catch {
        return $false
    }
}

function Ensure-QpwgraphRootDevice {
    if (Test-QpwgraphRootDevice) {
        Write-Verbose "Using existing root device $rootDeviceInstanceId"
        $script:rootDeviceNeedsCleanup = $true
        return
    }

    $devgen = Find-DevGen
    $output = @(& $devgen '/add' '/bus' 'ROOT' '/instanceid' 'QPWGRAPH_AUDIO' '/hardwareid' $rootDeviceHardwareId 2>&1)
    $exitCode = $LASTEXITCODE
    if (-not [string]::IsNullOrWhiteSpace(($output | Out-String))) {
        Write-Verbose (($output | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine)
    }
    if ($exitCode -ne 0) {
        throw "DevGen failed to create $rootDeviceInstanceId with exit code $exitCode."
    }
    $script:rootDeviceNeedsCleanup = $true
    Start-Sleep -Milliseconds 250
}

function Remove-QpwgraphRootDevice {
    if (-not $script:rootDeviceNeedsCleanup) {
        return
    }

    $output = @(& pnputil.exe /remove-device $rootDeviceInstanceId /subtree 2>&1)
    $exitCode = $LASTEXITCODE
    if (-not [string]::IsNullOrWhiteSpace(($output | Out-String))) {
        Write-Verbose (($output | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine)
    }
    $script:rootDeviceNeedsCleanup = $false
    if ($exitCode -ne 0) {
        Write-Warning "Could not remove $rootDeviceInstanceId with PnPUtil (exit code $exitCode). Remove the device manually before retrying."
    }
}

function Invoke-SmokeCheck([string] $Argument) {
    $output = (& $SmokeProbe $Argument 2>&1 | Out-String)
    $exitCode = $LASTEXITCODE
    if (-not [string]::IsNullOrWhiteSpace($output)) {
        Write-Verbose ($output.TrimEnd())
    }
    if ($exitCode -ne 0) {
        throw "Endpoint verification $Argument failed with exit code ${exitCode}: $($output.Trim())"
    }
}

function Wait-ForEndpointRoles {
    $deadline = (Get-Date).AddSeconds($VerificationTimeoutSeconds)
    $lastError = 'the smoke probe did not run'
    do {
        try {
            Invoke-SmokeCheck '--verify-roles'
            return
        } catch {
            $lastError = $_.Exception.Message
            if ((Get-Date) -ge $deadline) {
                throw "QPWGraph endpoints did not pass provider-role verification before the timeout: $lastError"
            }
            Start-Sleep -Seconds 1
        }
    } while ($true)
}

if (-not $SkipEndpointVerification) {
    if ([string]::IsNullOrWhiteSpace($SmokeProbe)) {
        throw 'A built qpwgraph-audio-smoke executable is required for endpoint verification. Use -SkipEndpointVerification only for an explicitly managed test install.'
    }
    $SmokeProbe = (Resolve-Path -LiteralPath $SmokeProbe -ErrorAction Stop).Path
    if (-not (Test-Path -LiteralPath $SmokeProbe -PathType Leaf)) {
        throw "Smoke probe was not found: $SmokeProbe"
    }
} elseif (-not [string]::IsNullOrWhiteSpace($SmokeProbe)) {
    throw 'Do not pass -SmokeProbe together with -SkipEndpointVerification.'
}

Assert-Administrator
if ($AllowTestSigned) {
    Assert-TestSigningEnabled
} else {
    Write-Verbose 'Installing through PnPUtil with normal Windows signature policy.'
}

if (-not $PSCmdlet.ShouldProcess($inf, 'install the QPWGraph audio driver package')) {
    return
}

# The installer uses only Microsoft driver tools and does not select a Windows
# default render or capture endpoint. DevGen creates the development-only root
# devnode that PnPUtil can then match to this INF.
Ensure-QpwgraphRootDevice
$pnputilOutput = (& pnputil.exe /add-driver $inf /install 2>&1 | Out-String)
$pnputilExitCode = $LASTEXITCODE
if (-not [string]::IsNullOrWhiteSpace($pnputilOutput)) {
    Write-Verbose ($pnputilOutput.TrimEnd())
}
if ($pnputilExitCode -ne 0) {
    Remove-QpwgraphRootDevice
    throw "PnPUtil failed with exit code $pnputilExitCode. No Windows default device was changed."
}

$publishedMatch = [regex]::Match($pnputilOutput, '(?im)Published\s+Name\s*:\s*(oem\d+\.inf)')
if (-not $publishedMatch.Success) {
    $publishedMatch = [regex]::Match($pnputilOutput, '(?im)\b(oem\d+\.inf)\b')
}
if (-not $publishedMatch.Success) {
    throw 'PnPUtil succeeded but did not report the published oemNN.inf name; refusing to continue without an exact rollback target.'
}
$publishedInf = $publishedMatch.Groups[1].Value.ToLowerInvariant()

try {
    if ($SkipEndpointVerification) {
        Write-Warning 'Endpoint-role verification was explicitly skipped; the package is installed but not validated.'
    } else {
        Wait-ForEndpointRoles
    }
} catch {
    Remove-QpwgraphRootDevice
    $rollbackOutput = (& pnputil.exe /delete-driver $publishedInf /uninstall 2>&1 | Out-String)
    $rollbackExitCode = $LASTEXITCODE
    if ($rollbackExitCode -ne 0) {
        Write-Error "Automatic rollback of $publishedInf failed with exit code ${rollbackExitCode}: $($rollbackOutput.Trim())"
    }
    throw
}

Write-Output "QPWGraph audio driver package version $driverVersion installed as $publishedInf; Windows default devices were not changed."
