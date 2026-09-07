#requires -Version 5.1

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $EvidenceRoot,
    [Parameter(Mandatory = $true)]
    [string] $PackageRoot,
    [Parameter(Mandatory = $false)]
    [string] $OutputPath
)

$ErrorActionPreference = 'Stop'

Write-Output 'QPWGraph external Windows release-evidence validator'
Write-Output '  READ-ONLY: validates retained evidence and compares it with the returned package.'
Write-Output '  STATE-MUTATING: only -OutputPath writes a validation record.'
Write-Output '  REBOOT-REQUIRING: none.'

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

function Read-JsonFile([string] $Path) {
    try {
        return Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
    } catch {
        throw "Could not parse evidence JSON $($Path): $($_.Exception.Message)"
    }
}

function Add-Check(
    [System.Collections.Generic.List[object]] $Items,
    [string] $Name,
    [bool] $Passed,
    [string] $Detail
) {
    $Items.Add([pscustomobject]@{
            name = $Name
            passed = $Passed
            detail = $Detail
        })
}

function Get-ObjectProperty($Object, [string] $Name) {
    if ($null -eq $Object) {
        return $null
    }
    return $Object.PSObject.Properties[$Name].Value
}

function Test-NonEmpty([object] $Value) {
    return $null -ne $Value -and -not [string]::IsNullOrWhiteSpace([string]$Value)
}

$evidence = (Resolve-Path -LiteralPath $EvidenceRoot -ErrorAction Stop).Path
$package = (Resolve-Path -LiteralPath $PackageRoot -ErrorAction Stop).Path
$checks = New-Object 'System.Collections.Generic.List[object]'

function Find-EvidenceFile([string] $Name) {
    $direct = Join-Path $evidence $Name
    if (Test-Path -LiteralPath $direct -PathType Leaf) {
        return (Resolve-Path -LiteralPath $direct).Path
    }
    $matches = @(Get-ChildItem -LiteralPath $evidence -Filter $Name -File -Recurse -ErrorAction SilentlyContinue)
    if ($matches.Count -gt 1) {
        throw "Evidence contains multiple copies of $Name; retain exactly one."
    }
    if ($matches.Count -eq 1) {
        return $matches[0].FullName
    }
    return $null
}

$requiredEvidence = @(
    'hlk-preparation.json',
    'hlk-result-package.zip',
    'hlk-result-package.zip.sha256',
    'driver-release-manifest.json',
    'test-machine-build.txt',
    'secure-boot-audit.json',
    'verifier-evidence.json',
    'driver-stress.json',
    'acceptance-evidence.json',
    'returned-driver-verification.json'
)
$evidencePaths = @{}
foreach ($name in $requiredEvidence) {
    $path = Find-EvidenceFile $name
    $exists = $null -ne $path
    Add-Check $checks "evidence file: $name" $exists $(if ($exists) { $path } else { 'missing' })
    if ($exists) {
        $evidencePaths[$name] = $path
    }
}

$packageFiles = @(
    'manifest.json',
    'qpwgraph_audio.sys',
    'qpwgraph-audio.inf',
    'qpwgraph-audio.cat',
    'install.ps1',
    'uninstall.ps1'
)
foreach ($name in $packageFiles) {
    $path = Join-Path $package $name
    $exists = Test-Path -LiteralPath $path -PathType Leaf
    Add-Check $checks "returned package file: $name" $exists $(if ($exists) { $path } else { 'missing' })
}

$hlk = $null
$secureBoot = $null
$verifier = $null
$stress = $null
$acceptance = $null
$returned = $null
$release = $null
if ($evidencePaths.ContainsKey('hlk-preparation.json')) {
    $hlk = Read-JsonFile $evidencePaths['hlk-preparation.json']
    Add-Check $checks 'HLK preparation schema' ([string](Get-ObjectProperty $hlk 'kind') -eq 'qpwgraph-windows-hlk-run') ([string](Get-ObjectProperty $hlk 'kind'))
}
if ($evidencePaths.ContainsKey('secure-boot-audit.json')) {
    $secureBoot = Read-JsonFile $evidencePaths['secure-boot-audit.json']
    Add-Check $checks 'Secure Boot audit schema' ([string](Get-ObjectProperty $secureBoot 'kind') -eq 'qpwgraph-secure-boot-audit') ([string](Get-ObjectProperty $secureBoot 'kind'))
}
if ($evidencePaths.ContainsKey('verifier-evidence.json')) {
    $verifier = Read-JsonFile $evidencePaths['verifier-evidence.json']
    Add-Check $checks 'Driver Verifier evidence schema' ([string](Get-ObjectProperty $verifier 'kind') -eq 'qpwgraph-windows-verifier-evidence') ([string](Get-ObjectProperty $verifier 'kind'))
}
if ($evidencePaths.ContainsKey('driver-stress.json')) {
    $stress = Read-JsonFile $evidencePaths['driver-stress.json']
    Add-Check $checks 'Driver stress evidence schema' ([string](Get-ObjectProperty $stress 'kind') -eq 'qpwgraph-windows-driver-stress') ([string](Get-ObjectProperty $stress 'kind'))
}
if ($evidencePaths.ContainsKey('acceptance-evidence.json')) {
    $acceptance = Read-JsonFile $evidencePaths['acceptance-evidence.json']
    $gateObject = Get-ObjectProperty $acceptance 'gates'
    Add-Check $checks 'Acceptance evidence schema' ([int](Get-ObjectProperty $acceptance 'schema') -eq 1 -and $null -ne $gateObject) 'schema=1; gates object present'
}
if ($evidencePaths.ContainsKey('returned-driver-verification.json')) {
    $returned = Read-JsonFile $evidencePaths['returned-driver-verification.json']
    Add-Check $checks 'Returned-driver verification schema' ([string](Get-ObjectProperty $returned 'kind') -eq 'qpwgraph-windows-returned-driver-verification') ([string](Get-ObjectProperty $returned 'kind'))
}

if ($null -ne $verifier) {
    $settings = Get-ObjectProperty $verifier 'verifier_querysettings'
    $query = Get-ObjectProperty $verifier 'verifier_query'
    $settingsAvailable = [bool](Get-ObjectProperty $settings 'available')
    $queryAvailable = [bool](Get-ObjectProperty $query 'available')
    Add-Check $checks 'Driver Verifier settings were collected' ($settingsAvailable -and [int](Get-ObjectProperty $settings 'exit_code') -eq 0) "available=$settingsAvailable; exit_code=$(Get-ObjectProperty $settings 'exit_code')"
    Add-Check $checks 'Driver Verifier query was collected' ($queryAvailable -and [int](Get-ObjectProperty $query 'exit_code') -eq 0) "available=$queryAvailable; exit_code=$(Get-ObjectProperty $query 'exit_code')"
}

if ($null -ne $stress) {
    $stressCompleted = [bool](Get-ObjectProperty $stress 'completed')
    Add-Check $checks 'Driver stress matrix completed' $stressCompleted ([string](Get-ObjectProperty $stress 'failure'))
    $stressRows = Get-ObjectProperty $stress 'rows'
    foreach ($rowName in @('app_cable', 'relay_cable', 'two_cable_isolation')) {
        $row = Get-ObjectProperty $stressRows $rowName
        $requested = [int](Get-ObjectProperty $row 'requested_cycles')
        $completed = [int](Get-ObjectProperty $row 'completed_cycles')
        $passed = [bool](Get-ObjectProperty $row 'passed')
        Add-Check $checks "Driver stress row: $rowName" ($stressCompleted -and $passed -and $requested -ge 100 -and $completed -ge $requested) "requested=$requested; completed=$completed; passed=$passed"
    }
}

if ($null -ne $acceptance) {
    $gates = Get-ObjectProperty $acceptance 'gates'
    $requiredGates = @(
        'HLK audio tests complete',
        'Rust ACX runtime parity',
        'Driver Verifier stress matrix',
        'Microsoft signing pipeline established',
        'Secure Boot installation verified',
        'Chrome/VLC ordinary relay acceptance',
        'Discord Relay Microphone acceptance',
        'Sleep/resume lifecycle',
        'AudioSrv restart lifecycle',
        'Disable/enable lifecycle',
        'Reboot lifecycle',
        'Crash recovery lifecycle',
        'Install/uninstall/upgrade lifecycle',
        'Destination disappearance and return',
        'Physical endpoint churn selector stability'
    )
    foreach ($gateName in $requiredGates) {
        $gate = Get-ObjectProperty $gates $gateName
        $status = [string](Get-ObjectProperty $gate 'status')
        $evidence = [string](Get-ObjectProperty $gate 'evidence')
        Add-Check $checks "Acceptance gate: $gateName" ($status -eq 'pass' -and (Test-NonEmpty $evidence)) "status=$status; evidence=$evidence"
    }
}
if ($evidencePaths.ContainsKey('driver-release-manifest.json')) {
    $release = Read-JsonFile $evidencePaths['driver-release-manifest.json']
    Add-Check $checks 'Release-candidate manifest schema' ([string](Get-ObjectProperty $release 'kind') -eq 'qpwgraph-windows-driver-release-candidate') ([string](Get-ObjectProperty $release 'kind'))
}

if ($null -ne $hlk) {
    $expectedRoles = @('app-render', 'app-monitor', 'relay-render', 'relay-capture')
    $roleObject = Get-ObjectProperty $hlk 'expected_endpoint_roles'
    $roleNames = @()
    if ($null -ne $roleObject) {
        $roleNames = @($roleObject.PSObject.Properties | ForEach-Object { $_.Name })
    }
    $rolesPass = $true
    foreach ($role in $expectedRoles) {
        if ($roleNames -notcontains $role) {
            $rolesPass = $false
        }
    }
    Add-Check $checks 'HLK endpoint-role contract' $rolesPass ($roleNames -join ', ')

    $validations = @((Get-ObjectProperty $hlk 'validation'))
    $failedValidations = @($validations | Where-Object { -not [bool](Get-ObjectProperty $_ 'passed') })
    $validationDetail = if ($validations.Count -eq 0) { 'no validation rows' } else { "$($failedValidations.Count) failed validation row(s)" }
    Add-Check $checks 'HLK package preparation passed' ($validations.Count -gt 0 -and $failedValidations.Count -eq 0) $validationDetail

    $hlkFiles = Get-ObjectProperty $hlk 'package_files'
    $releaseArtifacts = Get-ObjectProperty $release 'artifacts'
    foreach ($name in @('qpwgraph_audio.sys', 'qpwgraph-audio.inf', 'qpwgraph-audio.cat')) {
        $hlkRecord = Get-ObjectProperty $hlkFiles $name
        $releaseRecord = Get-ObjectProperty $releaseArtifacts $name
        $hlkHash = [string](Get-ObjectProperty $hlkRecord 'sha256')
        $releaseHash = [string](Get-ObjectProperty $releaseRecord 'sha256')
        Add-Check $checks "HLK/release hash: $name" (Test-NonEmpty $hlkHash -and $hlkHash -eq $releaseHash) "hlk=$hlkHash; release=$releaseHash"
    }
}

if ($null -ne $secureBoot) {
    $secureBootState = Get-ObjectProperty $secureBoot 'secure_boot_enabled'
    $testSigning = Get-ObjectProperty (Get-ObjectProperty $secureBoot 'test_signing') 'enabled'
    Add-Check $checks 'Secure Boot was enabled' ([bool]$secureBootState) ([string]$secureBootState)
    Add-Check $checks 'Windows test-signing was disabled' ($testSigning -eq $false) ([string]$testSigning)

    $probe = Get-ObjectProperty $secureBoot 'endpoint_role_probe'
    $probeSucceeded = $null -ne $probe -and [bool](Get-ObjectProperty $probe 'available') -and [int](Get-ObjectProperty $probe 'exit_code') -eq 0
    Add-Check $checks 'Secure Boot endpoint-role smoke passed' $probeSucceeded "available=$([bool](Get-ObjectProperty $probe 'available')); exit_code=$(Get-ObjectProperty $probe 'exit_code')"
}

if ($null -ne $returned) {
    $expectedPublisher = 'Microsoft Windows Hardware Compatibility Publisher'
    $override = [bool](Get-ObjectProperty $returned 'non_microsoft_override')
    $publisher = [string](Get-ObjectProperty $returned 'expected_publisher')
    Add-Check $checks 'Returned package used the Microsoft publisher path' (-not $override -and $publisher -eq $expectedPublisher) "expected=$publisher; override=$override"

    $signatures = @((Get-ObjectProperty $returned 'signatures'))
    foreach ($name in @('qpwgraph_audio.sys', 'qpwgraph-audio.cat')) {
        $record = $signatures | Where-Object { [string](Get-ObjectProperty $_ 'name') -eq $name } | Select-Object -First 1
        $status = [string](Get-ObjectProperty $record 'status')
        $signer = [string](Get-ObjectProperty $record 'signer')
        Add-Check $checks "Returned signature: $name" ($status -eq 'Valid' -and $signer -match [regex]::Escape($expectedPublisher)) "status=$status; signer=$signer"
    }

    $catalog = Get-ObjectProperty $returned 'catalog_verification'
    $catalogAvailable = [bool](Get-ObjectProperty $catalog 'available')
    $catalogExit = [int](Get-ObjectProperty $catalog 'exit_code')
    $catalogTargets = @((Get-ObjectProperty $catalog 'targets'))
    $catalogTargetNames = @($catalogTargets | Where-Object { [int](Get-ObjectProperty $_ 'exit_code') -eq 0 } | ForEach-Object { [string](Get-ObjectProperty $_ 'target') })
    $catalogTargetsPass = $catalogTargetNames -contains 'qpwgraph_audio.sys' -and $catalogTargetNames -contains 'qpwgraph-audio.inf'
    Add-Check $checks 'Returned catalog membership' ($catalogAvailable -and $catalogExit -eq 0 -and $catalogTargetsPass) "available=$catalogAvailable; exit_code=$catalogExit; targets=$($catalogTargetNames -join ', ')"

    $relationships = @((Get-ObjectProperty $returned 'artifact_relationships'))
    $failedRelationships = @($relationships | Where-Object { -not [bool](Get-ObjectProperty $_ 'passed') })
    Add-Check $checks 'Returned artifact relationships' ($relationships.Count -ge 3 -and $failedRelationships.Count -eq 0) "records=$($relationships.Count); failed=$($failedRelationships.Count)"

    $submitted = [string](Get-ObjectProperty $returned 'submitted_manifest')
    Add-Check $checks 'Returned package references candidate manifest' ($submitted -eq 'qpwgraph-windows-driver-release-candidate') $submitted

    $returnedHashes = Get-ObjectProperty $returned 'returned_artifacts'
    foreach ($name in @('qpwgraph_audio.sys', 'qpwgraph-audio.inf', 'qpwgraph-audio.cat')) {
        $path = Join-Path $package $name
        $actual = if (Test-Path -LiteralPath $path -PathType Leaf) { Get-Sha256 $path } else { '' }
        $property = switch ($name) {
            'qpwgraph_audio.sys' { 'sys_sha256' }
            'qpwgraph-audio.inf' { 'inf_sha256' }
            'qpwgraph-audio.cat' { 'cat_sha256' }
        }
        $reported = [string](Get-ObjectProperty $returnedHashes $property)
        Add-Check $checks "Returned hash: $name" (Test-NonEmpty $actual -and $actual -eq $reported) "actual=$actual; reported=$reported"
    }
}

if ($null -ne $release) {
    foreach ($property in @('git_commit', 'driver_version', 'driver_runtime', 'sys_sha256', 'inf_sha256', 'cat_sha256', 'rust_version', 'llvm_version')) {
        $value = Get-ObjectProperty $release $property
        Add-Check $checks "Release manifest field: $property" (Test-NonEmpty $value) ([string]$value)
    }
    Add-Check $checks 'Release candidate Rust-only runtime marker' ([string](Get-ObjectProperty $release 'driver_runtime') -eq 'rust-only') ([string](Get-ObjectProperty $release 'driver_runtime'))
    $implementationStatus = [string](Get-ObjectProperty $release 'implementation_status')
    Add-Check $checks 'Release candidate implementation marker' ($implementationStatus -eq 'ready') $implementationStatus
}

$resultPackage = $evidencePaths['hlk-result-package.zip']
$resultHashFile = $evidencePaths['hlk-result-package.zip.sha256']
if ($null -ne $resultPackage -and $null -ne $resultHashFile) {
    $hashLine = (Get-Content -LiteralPath $resultHashFile -Raw).Trim()
    $match = [regex]::Match($hashLine, '^\s*([0-9A-Fa-f]{64})\s+(.+?)\s*$')
    $actualHash = Get-Sha256 $resultPackage
    $hashName = if ($match.Success) { Split-Path -Leaf $match.Groups[2].Value.Trim() } else { '' }
    $expectedHash = if ($match.Success) { $match.Groups[1].Value.ToUpperInvariant() } else { '' }
    $resultName = Split-Path -Leaf $resultPackage
    Add-Check $checks 'HLK result-package hash' ($match.Success -and $hashName -eq $resultName -and $expectedHash -eq $actualHash) "actual=$actualHash; recorded=$expectedHash; name=$hashName"
}

$machineBuild = $evidencePaths['test-machine-build.txt']
if ($null -ne $machineBuild) {
    $machineBuildText = (Get-Content -LiteralPath $machineBuild -Raw).Trim()
    Add-Check $checks 'HLK test-machine build recorded' (-not [string]::IsNullOrWhiteSpace($machineBuildText)) $machineBuildText
}

$failed = @($checks | Where-Object { -not $_.passed })
$result = [ordered]@{
    schema = 1
    kind = 'qpwgraph-windows-release-evidence-validation'
    validated_utc = [DateTime]::UtcNow.ToString('o')
    evidence_root = $evidence
    package_root = $package
    passed = ($failed.Count -eq 0)
    checks = $checks.ToArray()
    notes = @(
        'This validator checks retained evidence structure, hashes, and claimed machine state; it does not execute HLK, Verifier, Secure Boot, or client tests.'
        'The HLK result package and lifecycle/client acceptance record remain externally produced artifacts and must be retained with the exact release evidence bundle.'
    )
}

$json = $result | ConvertTo-Json -Depth 12
if (-not [string]::IsNullOrWhiteSpace($OutputPath)) {
    $parent = Split-Path -Parent $OutputPath
    if (-not [string]::IsNullOrWhiteSpace($parent)) {
        New-Item -ItemType Directory -Path $parent -Force | Out-Null
    }
    Set-Content -LiteralPath $OutputPath -Value $json -Encoding UTF8
    Write-Output "Evidence validation written to $((Resolve-Path -LiteralPath $OutputPath).Path)."
} else {
    Write-Output $json
}

if ($failed.Count -gt 0) {
    throw "External release evidence validation failed: $($failed.Count) check(s) did not pass."
}
