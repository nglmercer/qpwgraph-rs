#requires -Version 5.1
#requires -RunAsAdministrator

[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [Parameter(Mandatory = $false)]
    [ValidateSet('Status', 'Prepare', 'EnableTestMode', 'Install', 'Smoke', 'Uninstall', 'DisableTestMode')]
    [string] $Phase = 'Status',
    [Parameter(Mandatory = $false)]
    [string] $PackageRoot,
    [Parameter(Mandatory = $false)]
    [string] $SmokeProbe,
    [Parameter(Mandatory = $false)]
    [ValidatePattern('^oem[0-9]+\.inf$')]
    [string] $PublishedInf,
    [Parameter(Mandatory = $false)]
    [ValidateRange(100, 60000)]
    [int] $RoundTripDurationMs = 5000,
    [Parameter(Mandatory = $false)]
    [switch] $Reboot
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($PackageRoot)) {
    $PackageRoot = $PSScriptRoot
}
$packageRootPath = (Resolve-Path -LiteralPath $PackageRoot -ErrorAction Stop).Path

function Find-RepositoryRoot([string] $StartingPath) {
    $current = Get-Item -LiteralPath $StartingPath -ErrorAction Stop
    while ($null -ne $current) {
        if (Test-Path -LiteralPath (Join-Path $current.FullName 'drivers\windows-audio\Cargo.toml') -PathType Leaf) {
            return $current.FullName
        }
        $current = $current.Parent
    }
    return $null
}

$repositoryRoot = Find-RepositoryRoot $packageRootPath
$smokePath = $null
if (-not [string]::IsNullOrWhiteSpace($SmokeProbe)) {
    $smokePath = (Resolve-Path -LiteralPath $SmokeProbe -ErrorAction Stop).Path
} elseif ($null -ne $repositoryRoot) {
    $smokeCandidate = Join-Path $repositoryRoot 'drivers\windows-audio\target\debug\qpwgraph-audio-smoke.exe'
    if (Test-Path -LiteralPath $smokeCandidate -PathType Leaf) {
        $smokePath = $smokeCandidate
    }
}

$signScript = Join-Path $packageRootPath 'sign-test.ps1'
$installScript = Join-Path $packageRootPath 'install.ps1'
$uninstallScript = Join-Path $packageRootPath 'uninstall.ps1'
$manifestPath = Join-Path $packageRootPath 'manifest.json'

function Assert-ReadyPackage {
    foreach ($requiredFile in @(
            $manifestPath,
            (Join-Path $packageRootPath 'qpwgraph_audio.sys'),
            (Join-Path $packageRootPath 'qpwgraph-audio.inf'),
            (Join-Path $packageRootPath 'qpwgraph-audio.cat'),
            $signScript,
            $installScript,
            $uninstallScript
        )) {
        if (-not (Test-Path -LiteralPath $requiredFile -PathType Leaf)) {
            throw "The staged package is incomplete: $requiredFile"
        }
    }
    $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    if ($manifest.implementation_status -ne 'ready') {
        throw 'The package manifest is not ready. Run the WDK --build-package step first.'
    }
}

function Invoke-Native([string] $FilePath, [string[]] $Arguments, [string] $Description) {
    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Description failed with exit code $LASTEXITCODE."
    }
}

function Invoke-PowerShellScript([string] $ScriptPath, [string[]] $Arguments, [string] $Description) {
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $ScriptPath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Description failed with exit code $LASTEXITCODE."
    }
}

function Test-TestSigningEnabled {
    $output = (& bcdedit.exe /enum all 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) {
        throw "Could not read the boot configuration. Run this script as Administrator: $($output.Trim())"
    }
    return $output -match '(?im)^\s*testsigning\s+Yes\s*$'
}

function Invoke-Prepare {
    Assert-ReadyPackage
    if ($null -eq $repositoryRoot) {
        throw 'The staged package is not below the repository root; pass -SmokeProbe explicitly.'
    }
    if (-not $PSCmdlet.ShouldProcess($packageRootPath, 'sign the package, trust its public test certificate, and build the smoke probe')) {
        return
    }

    $certificatePath = Join-Path ([IO.Path]::GetTempPath()) ("qpwgraph-audio-test-{0}.cer" -f [Guid]::NewGuid())
    $signOutput = @(& powershell.exe -NoProfile -ExecutionPolicy Bypass -File $signScript -CreateCertificate -CertificateOutputPath $certificatePath 2>&1)
    if ($LASTEXITCODE -ne 0) {
        $details = ($signOutput | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine
        throw "The package signing helper failed: $details"
    }

    $thumbprint = $null
    foreach ($line in $signOutput) {
        if ($line.ToString() -match '^Certificate thumbprint:\s*(\S+)') {
            $thumbprint = $Matches[1]
            break
        }
    }
    if ([string]::IsNullOrWhiteSpace($thumbprint)) {
        throw 'The signing helper did not report a certificate thumbprint.'
    }

    Import-Certificate -FilePath $certificatePath -CertStoreLocation 'Cert:\LocalMachine\Root' | Out-Null
    Import-Certificate -FilePath $certificatePath -CertStoreLocation 'Cert:\LocalMachine\TrustedPublisher' | Out-Null

    $smokeManifest = Join-Path $repositoryRoot 'drivers\windows-audio\tests\smoke\Cargo.toml'
    Invoke-Native 'cargo.exe' @('build', '--manifest-path', $smokeManifest, '--locked') 'The smoke-probe build'
    $script:smokePath = Join-Path $repositoryRoot 'drivers\windows-audio\target\debug\qpwgraph-audio-smoke.exe'

    Write-Output "Package signed with test certificate $thumbprint"
    Write-Output "Public certificate: $certificatePath"
    Write-Output "Smoke probe: $script:smokePath"
    Write-Output 'Next: reboot with -Phase EnableTestMode -Reboot, then run -Phase Install.'
}

function Invoke-EnableTestMode {
    if (Test-TestSigningEnabled) {
        Write-Output 'Windows test-signing is already enabled.'
    } else {
        if (-not $PSCmdlet.ShouldProcess('Windows boot configuration', 'enable test-signing')) {
            return
        }
        Invoke-Native 'bcdedit.exe' @('/set', 'testsigning', 'on') 'Enabling Windows test-signing'
        Write-Output 'Windows test-signing enabled for the next boot.'
    }
    if ($Reboot) {
        if ($PSCmdlet.ShouldProcess('this computer', 'reboot into Windows test mode')) {
            Invoke-Native 'shutdown.exe' @('/r', '/t', '0') 'Rebooting into Windows test mode'
        }
    } else {
        Write-Output 'Reboot now, then run this script with -Phase Install.'
    }
}

function Invoke-Install {
    Assert-ReadyPackage
    if ($null -eq $smokePath -or -not (Test-Path -LiteralPath $smokePath -PathType Leaf)) {
        throw 'The smoke probe was not found. Run -Phase Prepare or pass -SmokeProbe explicitly.'
    }
    if (-not (Test-TestSigningEnabled)) {
        throw 'Windows test-signing is not enabled in the current boot. Run -Phase EnableTestMode and reboot first.'
    }
    Invoke-PowerShellScript $installScript @('-PackageRoot', $packageRootPath, '-AllowTestSigned', '-SmokeProbe', $smokePath, '-Verbose') 'Driver installation'
    Write-Output 'Installation completed. Save the oemNN.inf name printed above before continuing.'
}

function Invoke-Smoke {
    if ($null -eq $smokePath -or -not (Test-Path -LiteralPath $smokePath -PathType Leaf)) {
        throw 'The smoke probe was not found. Run -Phase Prepare or pass -SmokeProbe explicitly.'
    }
    Invoke-Native $smokePath @('--verify-roles') 'Provider-role verification'
    Invoke-Native $smokePath @('--list') 'Endpoint listing'
    Invoke-Native $smokePath @(
        '--render-name', 'QPWGraph Virtual Output',
        '--capture-name', 'QPWGraph Virtual Monitor',
        '--round-trip',
        '--duration-ms', $RoundTripDurationMs.ToString()
    ) 'Render/capture round-trip'
    Write-Output 'Basic ACX endpoint smoke validation passed.'
    Write-Output 'Next: test Relay Microphone with OBS/browser/Discord, then run Verifier and lifecycle tests.'
}

function Invoke-Uninstall {
    if ([string]::IsNullOrWhiteSpace($PublishedInf)) {
        throw 'Pass -PublishedInf oemNN.inf using the exact name printed by -Phase Install.'
    }
    if ($null -eq $smokePath -or -not (Test-Path -LiteralPath $smokePath -PathType Leaf)) {
        throw 'The smoke probe was not found. Run -Phase Prepare or pass -SmokeProbe explicitly.'
    }
    Invoke-PowerShellScript $uninstallScript @('-PublishedInf', $PublishedInf, '-SmokeProbe', $smokePath, '-Verbose') 'Driver uninstall'
    Write-Output 'Driver removal and endpoint-absence verification completed.'
}

function Invoke-DisableTestMode {
    if (-not $PSCmdlet.ShouldProcess('Windows boot configuration', 'disable test-signing')) {
        return
    }
    Invoke-Native 'bcdedit.exe' @('/set', 'testsigning', 'off') 'Disabling Windows test-signing'
    if ($Reboot) {
        if ($PSCmdlet.ShouldProcess('this computer', 'reboot out of Windows test mode')) {
            Invoke-Native 'shutdown.exe' @('/r', '/t', '0') 'Rebooting out of Windows test mode'
        }
    } else {
        Write-Output 'Reboot now to leave Windows test mode.'
    }
}

function Show-Status {
    Write-Output "Package: $packageRootPath"
    Write-Output ("Package present: {0}" -f (Test-Path -LiteralPath $manifestPath -PathType Leaf))
    try {
        Write-Output ("Test-signing enabled: {0}" -f (Test-TestSigningEnabled))
    } catch {
        Write-Warning $_.Exception.Message
    }
    if ($null -ne $smokePath) {
        Write-Output "Smoke probe: $smokePath"
    } else {
        Write-Output 'Smoke probe: not found; run Prepare or pass -SmokeProbe.'
    }
    Get-PnpDevice -Class MEDIA -PresentOnly -ErrorAction SilentlyContinue |
        Where-Object { $_.FriendlyName -like 'QPWGraph*' } |
        Select-Object Status, FriendlyName, InstanceId
}

switch ($Phase) {
    'Status' { Show-Status }
    'Prepare' { Invoke-Prepare }
    'EnableTestMode' { Invoke-EnableTestMode }
    'Install' { Invoke-Install }
    'Smoke' { Invoke-Smoke }
    'Uninstall' { Invoke-Uninstall }
    'DisableTestMode' { Invoke-DisableTestMode }
}
