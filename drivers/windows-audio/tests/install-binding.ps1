# Unit-test only the package binding gate; never execute installer top-level code.
$ErrorActionPreference = 'Stop'
$tokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile(
    (Join-Path $PSScriptRoot '../package/install.ps1'), [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count -ne 0) { throw ($parseErrors | Out-String) }
$definition = $ast.Find({ param($node)
    $node -is [Management.Automation.Language.FunctionDefinitionAst] -and
    $node.Name -eq 'Assert-ActivePackage'
}, $true)
if ($null -eq $definition) { throw 'Missing package binding gate' }
Invoke-Expression $definition.Extent.Text
$rootDeviceInstanceId = 'test-only'
function Get-PnpDeviceProperty {
    param($InstanceId, $KeyName, $ErrorAction)
    if ($InstanceId -ne 'test-only') { throw 'Unexpected real device access' }
    if ($KeyName -eq 'DEVPKEY_Device_DriverInfPath') { return @{ Data = $script:boundInf } }
    if ($KeyName -eq 'DEVPKEY_Device_ProblemCode') { return @{ Data = $script:problemCode } }
    throw "Unexpected property $KeyName"
}
foreach ($case in @(
    @{ Inf='oem20.inf'; Problem=0; Pass=$true },
    @{ Inf='OEM20.INF'; Problem=0; Pass=$true },
    @{ Inf='oem19.inf'; Problem=0; Pass=$false },
    @{ Inf=''; Problem=0; Pass=$false },
    @{ Inf='oem20.inf'; Problem=14; Pass=$false },
    @{ Inf='oem20.inf'; Problem=$null; Pass=$false }
)) {
    $script:boundInf = $case.Inf
    $script:problemCode = $case.Problem
    $passed = $true
    try { Assert-ActivePackage 'oem20.inf' } catch { $passed = $false }
    if ($passed -ne $case.Pass) { throw "Unexpected binding result for $($case | Out-String)" }
}
Write-Output 'All six installer binding cases passed without device mutations.'
