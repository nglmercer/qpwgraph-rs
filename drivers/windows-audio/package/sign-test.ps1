[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [Parameter(Mandatory = $false)]
    [string] $PackageRoot,
    [Parameter(Mandatory = $false)]
    [string] $CertificateThumbprint,
    [Parameter(Mandatory = $false)]
    [switch] $CreateCertificate,
    [Parameter(Mandatory = $false)]
    [string] $CertificateSubject = 'CN=QPWGraph Audio Test',
    [Parameter(Mandatory = $false)]
    [string] $CertificateOutputPath,
    [Parameter(Mandatory = $false)]
    [switch] $ImportCertificate
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($PackageRoot)) {
    $PackageRoot = $PSScriptRoot
}

if ([string]::IsNullOrWhiteSpace($CertificateThumbprint) -and -not $CreateCertificate) {
    throw 'Pass -CertificateThumbprint for an existing code-signing certificate or -CreateCertificate to create a disposable test certificate.'
}
if (-not [string]::IsNullOrWhiteSpace($CertificateThumbprint) -and $CreateCertificate) {
    throw 'Pass either -CertificateThumbprint or -CreateCertificate, not both.'
}
if (-not [string]::IsNullOrWhiteSpace($CertificateOutputPath) -and -not $CreateCertificate) {
    throw '-CertificateOutputPath is only valid with -CreateCertificate.'
}
if ($ImportCertificate -and -not $CreateCertificate) {
    throw '-ImportCertificate is only valid with -CreateCertificate.'
}

$packageRootPath = (Resolve-Path -LiteralPath $PackageRoot -ErrorAction Stop).Path
$sys = Join-Path $packageRootPath 'qpwgraph_audio.sys'
$inf = Join-Path $packageRootPath 'qpwgraph-audio.inf'
$cat = Join-Path $packageRootPath 'qpwgraph-audio.cat'
$manifestPath = Join-Path $packageRootPath 'manifest.json'
foreach ($requiredFile in @($sys, $inf, $cat, $manifestPath)) {
    if (-not (Test-Path -LiteralPath $requiredFile -PathType Leaf)) {
        throw "The staged package is incomplete: $requiredFile"
    }
}

$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if ($manifest.implementation_status -ne 'ready') {
    throw 'The package manifest is not ready. Run --build-package before signing.'
}

if (-not $PSCmdlet.ShouldProcess($packageRootPath, 'test-sign the QPWGraph ACX package')) {
    return
}

$certificate = $null
$machineStore = $false
if ($CreateCertificate) {
    $certificate = New-SelfSignedCertificate `
        -Type CodeSigningCert `
        -Subject $CertificateSubject `
        -CertStoreLocation 'Cert:\CurrentUser\My' `
        -HashAlgorithm SHA256 `
        -KeyAlgorithm RSA `
        -KeyLength 2048 `
        -NotAfter (Get-Date).AddYears(2)
    $CertificateThumbprint = $certificate.Thumbprint
    if ([string]::IsNullOrWhiteSpace($CertificateOutputPath)) {
        $CertificateOutputPath = Join-Path ([IO.Path]::GetTempPath()) 'qpwgraph-audio-test.cer'
    }
    Export-Certificate -Cert $certificate -FilePath $CertificateOutputPath -Type CERT | Out-Null
    if ($ImportCertificate) {
        Import-Certificate -FilePath $CertificateOutputPath -CertStoreLocation 'Cert:\LocalMachine\Root' | Out-Null
        Import-Certificate -FilePath $CertificateOutputPath -CertStoreLocation 'Cert:\LocalMachine\TrustedPublisher' | Out-Null
        Write-Output 'Imported the disposable test certificate into LocalMachine\Root and LocalMachine\TrustedPublisher.'
    } else {
        Write-Warning "Copy $CertificateOutputPath to the test machine and import it into LocalMachine\Root and LocalMachine\TrustedPublisher before installation."
    }
} else {
    $normalizedThumbprint = $CertificateThumbprint -replace '\s', ''
    $certificate = Get-ChildItem -Path 'Cert:\CurrentUser\My' |
        Where-Object { $_.Thumbprint -eq $normalizedThumbprint -and $_.HasPrivateKey } |
        Select-Object -First 1
    if ($null -eq $certificate) {
        $certificate = Get-ChildItem -Path 'Cert:\LocalMachine\My' |
            Where-Object { $_.Thumbprint -eq $normalizedThumbprint -and $_.HasPrivateKey } |
            Select-Object -First 1
        $machineStore = $null -ne $certificate
    }
    if ($null -eq $certificate) {
        throw "A code-signing certificate with private key was not found for thumbprint $normalizedThumbprint."
    }
    $CertificateThumbprint = $certificate.Thumbprint
}

function Find-WdkTool([string] $Name) {
    $onPath = Get-Command $Name -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -ne $onPath) {
        return $onPath.Source
    }
    $roots = @()
    if (-not [string]::IsNullOrWhiteSpace($env:WDKContentRoot)) {
        $roots += Join-Path $env:WDKContentRoot 'bin'
    }
    $roots += 'C:\Program Files (x86)\Windows Kits\10\bin'
    foreach ($root in ($roots | Select-Object -Unique)) {
        if (-not (Test-Path -LiteralPath $root -PathType Container)) {
            continue
        }
        # The current WDK ships Inf2Cat.exe under x86, while signtool.exe is
        # normally available under x64. Prefer x64 where present, but accept
        # x86 for tools that are not shipped for every architecture.
        foreach ($architecture in @('x64', 'x86', 'arm64')) {
            $candidate = Get-ChildItem -LiteralPath $root -Filter $Name -Recurse -File -ErrorAction SilentlyContinue |
                Where-Object {
                    $_.Directory.Name -ieq $architecture
                } |
                Sort-Object FullName -Descending |
                Select-Object -First 1
            if ($null -ne $candidate) {
                return $candidate.FullName
            }
        }
    }
    throw "$Name was not found on PATH or below the WDK bin directory. Install the WDK packaging tools or run from a WDK/eWDK developer prompt."
}

function Invoke-Tool([string] $Tool, [string[]] $Arguments, [string] $Name) {
    & $Tool @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Name failed with exit code $LASTEXITCODE."
    }
}

function Invoke-SignatureVerification(
    [string] $Tool,
    [string] $Catalog,
    [string] $File,
    [string] $Description
) {
    $arguments = @('verify', '/v', '/pa')
    if (-not [string]::IsNullOrWhiteSpace($Catalog)) {
        $arguments += @('/c', $Catalog)
    }
    $arguments += $File
    $output = (& $Tool @arguments 2>&1 | Out-String)
    $exitCode = $LASTEXITCODE
    if (-not [string]::IsNullOrWhiteSpace($output)) {
        Write-Verbose ($output.TrimEnd())
    }
    if ($exitCode -ne 0) {
        throw "$Description failed with exit code ${exitCode}: $($output.Trim())"
    }
}

$signTool = Find-WdkTool 'signtool.exe'
$inf2Cat = Find-WdkTool 'Inf2Cat.exe'
$storeArguments = @()
if ($machineStore) {
    $storeArguments += '/sm'
}

Write-Output "Signing $sys with certificate $CertificateThumbprint"
Invoke-Tool $signTool (@('sign', '/fd', 'SHA256', '/sha1', $CertificateThumbprint) + $storeArguments + @($sys)) 'SignTool driver signing'

# The catalog hashes the package contents. Regenerate it after signing the
# SYS, then sign the regenerated catalog with the same certificate.
Write-Output 'Regenerating the package catalog after driver signing'
Invoke-Tool $inf2Cat @("/driver:$packageRootPath", '/os:10_X64') 'Inf2Cat'
Invoke-Tool $signTool (@('sign', '/fd', 'SHA256', '/sha1', $CertificateThumbprint) + $storeArguments + @($cat)) 'SignTool catalog signing'

# Verify the exact files that the installer will hand to PnPUtil. This catches
# an unsigned or stale catalog before it is copied into the driver store.
Invoke-SignatureVerification $signTool '' $cat 'Catalog signature verification'
Invoke-SignatureVerification $signTool $cat $sys 'Driver catalog verification'
Invoke-SignatureVerification $signTool $cat $inf 'INF catalog verification'

Write-Output "Test-signed package ready at $packageRootPath"
Write-Output "Certificate thumbprint: $CertificateThumbprint"
