#requires -Version 5.1
$ErrorActionPreference = 'Stop'

function Import-TestFunction([string] $FunctionName) {
    $script = Join-Path $PSScriptRoot '..\package\validate-release-evidence.ps1'
    $tokens = $null
    $errors = $null
    $ast = [Management.Automation.Language.Parser]::ParseFile($script, [ref]$tokens, [ref]$errors)
    if ($errors.Count -ne 0) { throw ($errors | Out-String) }
    $definition = $ast.Find({ param($node)
        $node -is [Management.Automation.Language.FunctionDefinitionAst] -and
        $node.Name -eq $FunctionName
    }, $true)
    if ($null -eq $definition) { throw "Missing function $FunctionName" }
    return $definition.Extent.Text
}

foreach ($name in @('Get-ObjectProperty', 'Get-CapturedText',
        'Test-VerifierConfiguration', 'Get-PackageFileHash')) {
    Invoke-Expression (Import-TestFunction $name)
}

$activeSettings = [pscustomobject]@{ available = $true; exit_code = 0; output = @(
    'Verifier Flags: 0x0000033b', 'Verified Drivers:', 'qpwgraph_audio.sys') }
$activeQuery = [pscustomobject]@{ available = $true; exit_code = 0; output = @('qpwgraph_audio.sys') }
if (-not (Test-VerifierConfiguration $activeSettings $activeQuery 'qpwgraph_audio.sys')) {
    throw 'Active candidate-driver Verifier configuration was rejected'
}
$inactive = [pscustomobject]@{ available = $true; exit_code = 0; output = @(
    'Verifier Flags: 0x00000000', 'Verified Drivers:', 'None') }
if (Test-VerifierConfiguration $inactive $activeQuery 'qpwgraph_audio.sys') {
    throw 'Zero Verifier flags were accepted'
}
if (Test-VerifierConfiguration $activeSettings ([pscustomobject]@{
            available = $true; exit_code = 0; output = @('other.sys')
        }) 'different.sys') {
    throw 'Verifier evidence for another driver was accepted'
}

$files = [pscustomobject]@{ package_files = @(
    [pscustomobject]@{ name = 'qpwgraph_audio.sys'; sha256 = 'aabb' }
) }
if ((Get-PackageFileHash $files 'qpwgraph_audio.sys') -ne 'AABB' -or
    (Get-PackageFileHash $files 'missing.inf') -ne '') {
    throw 'Candidate package hash lookup failed closed incorrectly'
}
Write-Output 'release evidence validation regressions passed'
