[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^oem[0-9]+\.inf$')]
    [string] $PublishedInf,
    [Parameter(Mandatory = $false)]
    [string] $SmokeProbe,
    [Parameter(Mandatory = $false)]
    [ValidateRange(1, 300)]
    [int] $VerificationTimeoutSeconds = 30,
    [Parameter(Mandatory = $false)]
    [switch] $SkipEndpointVerification
)

$ErrorActionPreference = 'Stop'
$rootDeviceInstanceId = 'ROOT\QPWGRAPH_AUDIO'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Driver removal requires an elevated PowerShell prompt.'
}

if (-not $SkipEndpointVerification) {
    if ([string]::IsNullOrWhiteSpace($SmokeProbe)) {
        throw 'A built qpwgraph-audio-smoke executable is required to verify endpoint removal. Use -SkipEndpointVerification only for an explicitly managed test uninstall.'
    }
    $SmokeProbe = (Resolve-Path -LiteralPath $SmokeProbe -ErrorAction Stop).Path
    if (-not (Test-Path -LiteralPath $SmokeProbe -PathType Leaf)) {
        throw "Smoke probe was not found: $SmokeProbe"
    }
} elseif (-not [string]::IsNullOrWhiteSpace($SmokeProbe)) {
    throw 'Do not pass -SmokeProbe together with -SkipEndpointVerification.'
}

function Invoke-SmokeAbsentCheck {
    $output = (& $SmokeProbe '--verify-absent' 2>&1 | Out-String)
    $exitCode = $LASTEXITCODE
    if (-not [string]::IsNullOrWhiteSpace($output)) {
        Write-Verbose ($output.TrimEnd())
    }
    if ($exitCode -ne 0) {
        throw "Endpoint-removal verification failed with exit code ${exitCode}: $($output.Trim())"
    }
}

function Wait-ForEndpointRemoval {
    $deadline = (Get-Date).AddSeconds($VerificationTimeoutSeconds)
    $lastError = 'the smoke probe did not run'
    do {
        try {
            Invoke-SmokeAbsentCheck
            return
        } catch {
            $lastError = $_.Exception.Message
            if ((Get-Date) -ge $deadline) {
                throw "QPWGraph endpoints did not disappear before the timeout: $lastError"
            }
            Start-Sleep -Seconds 1
        }
    } while ($true)
}

if ($PSCmdlet.ShouldProcess($PublishedInf, 'remove the QPWGraph audio driver package')) {
    $removeDeviceOutput = (& pnputil.exe /remove-device $rootDeviceInstanceId /subtree 2>&1 | Out-String)
    $removeDeviceExitCode = $LASTEXITCODE
    if (-not [string]::IsNullOrWhiteSpace($removeDeviceOutput)) {
        Write-Verbose ($removeDeviceOutput.TrimEnd())
    }
    if ($removeDeviceExitCode -ne 0 -and $removeDeviceOutput -notmatch '(?i)no matching devices|not found|does not exist') {
        throw "PnPUtil could not remove the QPWGraph root device (exit code $removeDeviceExitCode). The driver package was left installed."
    }

    $pnputilOutput = (& pnputil.exe /delete-driver $PublishedInf /uninstall 2>&1 | Out-String)
    $pnputilExitCode = $LASTEXITCODE
    if (-not [string]::IsNullOrWhiteSpace($pnputilOutput)) {
        Write-Verbose ($pnputilOutput.TrimEnd())
    }
    if ($pnputilExitCode -ne 0) {
        throw "PnPUtil failed with exit code $pnputilExitCode. The package may still be in use."
    }
    if ($SkipEndpointVerification) {
        Write-Warning 'Endpoint-removal verification was explicitly skipped.'
    } else {
        Wait-ForEndpointRemoval
    }
    Write-Output 'QPWGraph audio driver package removed; Windows default devices were not changed.'
}
