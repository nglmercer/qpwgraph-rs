#requires -Version 5.1

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $PackageRoot,
    [Parameter(Mandatory = $false)]
    [string] $SubmissionManifest,
    [Parameter(Mandatory = $false)]
    [string] $ExpectedPublisher = 'Microsoft Windows Hardware Compatibility Publisher',
    [Parameter(Mandatory = $false)]
    [switch] $AllowNonMicrosoft,
    [Parameter(Mandatory = $false)]
    [string] $OutputPath
)

$ErrorActionPreference = 'Stop'

Write-Output 'Returned Microsoft-signed driver verification'
Write-Output '  READ-ONLY: verifies package shape, signatures, catalog membership, and semantic role metadata.'
Write-Output '  STATE-MUTATING: only -OutputPath writes a verification record.'
Write-Output '  REBOOT-REQUIRING: none; run Secure Boot smoke separately on a disposable machine.'

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

function Get-ObjectProperty($Object, [string] $Name) {
    if ($null -eq $Object) {
        return $null
    }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) {
        return $null
    }
    return $property.Value
}

function Get-ArtifactRecord($Manifest, [string] $Name) {
    $packageFiles = Get-ObjectProperty $Manifest 'package_files'
    $artifacts = Get-ObjectProperty $Manifest 'artifacts'
    $container = if ($null -ne $packageFiles) { $packageFiles } else { $artifacts }
    return Get-ObjectProperty $container $Name
}

function Invoke-CatalogCheck(
    [string] $ToolPath,
    [string] $CatalogPath,
    [string] $TargetPath,
    [bool] $KernelPolicy
) {
    # The returned SYS must satisfy kernel-driver policy. The INF is a catalog
    # member rather than a kernel image, so it uses normal Authenticode policy.
    # The explicit development override also uses normal policy so a
    # disposable test certificate can exercise the same checks without
    # weakening the release path.
    $policy = if ($KernelPolicy) { '/kp' } else { '/pa' }
    $lines = @(& $ToolPath 'verify' $policy '/c' $CatalogPath $TargetPath 2>&1) |
        ForEach-Object { $_.ToString() }
    $exitCode = $LASTEXITCODE
    return [pscustomobject]@{
        target = Split-Path -Leaf $TargetPath
        policy = $policy
        exit_code = $exitCode
        output = @($lines)
    }
}

$root = (Resolve-Path -LiteralPath $PackageRoot -ErrorAction Stop).Path
$required = @('qpwgraph_audio.sys', 'qpwgraph-audio.inf', 'qpwgraph-audio.cat')
foreach ($name in $required) {
    if (-not (Test-Path -LiteralPath (Join-Path $root $name) -PathType Leaf)) {
        throw "Returned package is missing $name."
    }
}

$inf = Get-Content -LiteralPath (Join-Path $root 'qpwgraph-audio.inf') -Raw
if ($inf -notmatch '(?im)qpwgraph_audio') { throw 'Returned INF does not contain service qpwgraph_audio.' }
foreach ($role in @('app-render', 'app-monitor', 'relay-render', 'relay-capture')) {
    if ($inf -notmatch [regex]::Escape($role)) { throw "Returned INF is missing semantic role $role." }
}

$submitted = $null
if (-not [string]::IsNullOrWhiteSpace($SubmissionManifest)) {
    $submitted = Get-Content -LiteralPath (Resolve-Path -LiteralPath $SubmissionManifest -ErrorAction Stop) -Raw | ConvertFrom-Json
    $submittedKind = [string](Get-ObjectProperty $submitted 'kind')
    if ($submittedKind -notin @(
            'qpwgraph-windows-driver-release-candidate',
            'qpwgraph-windows-hardware-dashboard-submission'
        )) {
        throw "Unsupported submission manifest kind: $submittedKind"
    }
    foreach ($name in @('qpwgraph_audio.sys', 'qpwgraph-audio.inf', 'qpwgraph-audio.cat')) {
        $record = Get-ArtifactRecord $submitted $name
        $sha = [string](Get-ObjectProperty $record 'sha256')
        $length = [int64](Get-ObjectProperty $record 'length')
        if ($null -eq $record -or $sha -notmatch '^[0-9A-Fa-f]{64}$' -or $length -le 0) {
            throw "Submission manifest is missing a SHA-256 record for $name."
        }
    }
}

$signatureRecords = @()
foreach ($name in @('qpwgraph_audio.sys', 'qpwgraph-audio.cat')) {
    $path = Join-Path $root $name
    $signature = Get-AuthenticodeSignature -LiteralPath $path
    $subject = if ($null -ne $signature.SignerCertificate) { $signature.SignerCertificate.Subject } else { '' }
    $record = [pscustomobject]@{
        name = $name
        sha256 = Get-Sha256 $path
        status = [string]$signature.Status
        signer = $subject
        issuer = if ($null -ne $signature.SignerCertificate) { $signature.SignerCertificate.Issuer } else { $null }
    }
    $signatureRecords += $record
    if ([string]$signature.Status -ne 'Valid') {
        throw "$name does not have a valid Authenticode signature: $($signature.Status)."
    }
    if (-not $AllowNonMicrosoft -and $subject -notmatch [regex]::Escape($ExpectedPublisher)) {
        throw "$name signer '$subject' does not match expected publisher '$ExpectedPublisher'."
    }
}

$signtool = Get-Command -Name 'signtool.exe' -CommandType Application -ErrorAction SilentlyContinue |
    Select-Object -First 1
$catalogCheck = [pscustomobject]@{ available = ($null -ne $signtool); exit_code = $null; output = @() }
if ($null -ne $signtool) {
    $cat = Join-Path $root 'qpwgraph-audio.cat'
    $sys = Join-Path $root 'qpwgraph_audio.sys'
    $infPath = Join-Path $root 'qpwgraph-audio.inf'
    $catalogTargets = @(
            (Invoke-CatalogCheck $signtool.Source $cat $sys (-not [bool]$AllowNonMicrosoft))
            (Invoke-CatalogCheck $signtool.Source $cat $infPath $false)
    )
    $failedCatalogTargets = @($catalogTargets | Where-Object { $_.exit_code -ne 0 })
    $catalogCheck = [pscustomobject]@{
        available = $true
        exit_code = if ($failedCatalogTargets.Count -eq 0) { 0 } else { $failedCatalogTargets[0].exit_code }
        output = @($catalogTargets | ForEach-Object { $_.output })
        targets = $catalogTargets
    }
    if ($failedCatalogTargets.Count -ne 0) {
        throw "signtool catalog verification failed for $($failedCatalogTargets[0].target) with exit code $($failedCatalogTargets[0].exit_code)."
    }
} else {
    throw 'signtool.exe was not found; production returned-package verification requires the WDK signing tools.'
}

$returnedArtifacts = [ordered]@{
    sys_sha256 = Get-Sha256 (Join-Path $root 'qpwgraph_audio.sys')
    inf_sha256 = Get-Sha256 (Join-Path $root 'qpwgraph-audio.inf')
    cat_sha256 = Get-Sha256 (Join-Path $root 'qpwgraph-audio.cat')
}

$artifactRelationships = @()
if ($null -ne $submitted) {
    foreach ($name in @('qpwgraph_audio.sys', 'qpwgraph-audio.inf', 'qpwgraph-audio.cat')) {
        $submittedRecord = Get-ArtifactRecord $submitted $name
        $submittedHash = ([string](Get-ObjectProperty $submittedRecord 'sha256')).ToUpperInvariant()
        $submittedLength = [int64](Get-ObjectProperty $submittedRecord 'length')
        $returnedHash = switch ($name) {
            'qpwgraph_audio.sys' { [string]$returnedArtifacts.sys_sha256 }
            'qpwgraph-audio.inf' { [string]$returnedArtifacts.inf_sha256 }
            'qpwgraph-audio.cat' { [string]$returnedArtifacts.cat_sha256 }
        }
        $returnedLength = (Get-Item -LiteralPath (Join-Path $root $name)).Length
        $exact = $submittedHash -eq $returnedHash.ToUpperInvariant()
        $relationship = if ($exact) {
            'exact_match'
        } elseif ($name -eq 'qpwgraph-audio.inf') {
            'unexpected_inf_change'
        } elseif ($returnedLength -ge $submittedLength) {
            'signature_or_dashboard_transform'
        } else {
            'unexpected_size_reduction'
        }
        $passed = $relationship -in @('exact_match', 'signature_or_dashboard_transform')
        $artifactRelationships += [pscustomobject]@{
            name = $name
            submitted_sha256 = $submittedHash
            returned_sha256 = $returnedHash
            submitted_length = $submittedLength
            returned_length = $returnedLength
            relationship = $relationship
            passed = $passed
        }
        if (-not $passed) {
            throw "Returned artifact $name does not match the submitted package relationship: $relationship."
        }
    }

    # A returned package may omit the custom manifest, but if it carries one,
    # it must remain the exact manifest that was submitted.
    $returnedManifestPath = Join-Path $root 'manifest.json'
    $submittedManifestRecord = Get-ArtifactRecord $submitted 'manifest.json'
    if ((Test-Path -LiteralPath $returnedManifestPath -PathType Leaf) -and $null -ne $submittedManifestRecord) {
        $submittedManifestHash = ([string](Get-ObjectProperty $submittedManifestRecord 'sha256')).ToUpperInvariant()
        $returnedManifestHash = (Get-Sha256 $returnedManifestPath).ToUpperInvariant()
        if ($submittedManifestHash -ne $returnedManifestHash) {
            throw 'Returned manifest.json does not match the submitted manifest.'
        }
        $artifactRelationships += [pscustomobject]@{
            name = 'manifest.json'
            submitted_sha256 = $submittedManifestHash
            returned_sha256 = $returnedManifestHash
            submitted_length = [int64](Get-ObjectProperty $submittedManifestRecord 'length')
            returned_length = (Get-Item -LiteralPath $returnedManifestPath).Length
            relationship = 'exact_match'
            passed = $true
        }
    }
}

$result = [ordered]@{
    schema = 1
    kind = 'qpwgraph-windows-returned-driver-verification'
    verified_utc = [DateTime]::UtcNow.ToString('o')
    package_root = $root
    expected_publisher = $ExpectedPublisher
    non_microsoft_override = [bool]$AllowNonMicrosoft
    signatures = $signatureRecords
    catalog_verification = $catalogCheck
    returned_artifacts = $returnedArtifacts
    artifact_relationships = $artifactRelationships
    submitted_manifest = if ($null -ne $submitted) { [string]$submitted.kind } else { $null }
    notes = @(
        'Catalog membership is verified for both the returned SYS and INF. Production uses kernel policy for SYS and normal Authenticode policy for INF; -AllowNonMicrosoft uses normal policy for disposable test packages.'
        'SYS/CAT hash changes are accepted only as non-shrinking signing or dashboard transformations and are recorded in artifact_relationships.'
        'Run secure-boot-audit.ps1 and the live install/reboot/uninstall matrix before release.'
    )
}
$json = $result | ConvertTo-Json -Depth 10
if (-not [string]::IsNullOrWhiteSpace($OutputPath)) {
    $parent = Split-Path -Parent $OutputPath
    if (-not [string]::IsNullOrWhiteSpace($parent)) {
        New-Item -ItemType Directory -Path $parent -Force | Out-Null
    }
    Set-Content -LiteralPath $OutputPath -Value $json -Encoding UTF8
    Write-Output "Returned-driver verification written to $((Resolve-Path -LiteralPath $OutputPath).Path)."
} else {
    Write-Output $json
}
