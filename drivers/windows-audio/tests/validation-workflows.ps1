# Test extracted functions with mocks only; do not execute package top-level code.
$ErrorActionPreference = 'Stop'
function Import-TestFunction([string] $ScriptName, [string] $FunctionName) {
    $tokens = $null
    $parseErrors = $null
    $ast = [Management.Automation.Language.Parser]::ParseFile(
        (Join-Path $PSScriptRoot "../package/$ScriptName"), [ref]$tokens, [ref]$parseErrors)
    if ($parseErrors.Count -ne 0) { throw ($parseErrors | Out-String) }
    $definition = $ast.Find({ param($node)
        $node -is [Management.Automation.Language.FunctionDefinitionAst] -and
        $node.Name -eq $FunctionName
    }, $true)
    if ($null -eq $definition) { throw "Missing function $FunctionName" }
    return $definition.Extent.Text
}

Invoke-Expression (Import-TestFunction 'run-driver-stress.ps1' 'Invoke-CableCycles')
$Cycles = 3
$DurationMilliseconds = 250
function Invoke-Smoke([string[]] $Arguments) {
    if (($Arguments -join ' ') -ne '--verify-cables --duration-ms 250') {
        throw 'Unexpected smoke arguments'
    }
    $script:invocations++
    if ($script:invocations -eq $script:failAt) { throw 'injected no-packet failure' }
}
foreach ($failAt in @(0, 1, 3)) {
    $script:failAt = $failAt
    $script:invocations = 0
    $script:stressRows = [ordered]@{
        two_cable_isolation = [ordered]@{ completed_cycles = 0; passed = $false; status = 'not-run' }
    }
    $failure = $null
    try { Invoke-CableCycles 'two_cable_isolation' '--verify-cables' 'test' } catch { $failure = $_ }
    $row = $script:stressRows.two_cable_isolation
    if ($failAt -eq 0) {
        if ($failure -or -not $row.passed -or $row.status -ne 'passed' -or $row.completed_cycles -ne 3) {
            throw 'Successful stress row was not recorded correctly'
        }
    } elseif ($null -eq $failure -or $row.passed -or $row.status -ne 'failed' -or
        $row.completed_cycles -ne ($failAt - 1) -or $row.failed_cycle -ne $failAt) {
        throw 'Failed stress row lost its exact failure/cycle evidence'
    }
    if (-not $row.started_utc -or -not $row.completed_utc) { throw 'Missing row timestamps' }
}

# PnpDevice exposes CDXML functions on Windows 10, not just binary cmdlets.
# No real device is accessed: all named commands below are local mock functions.
$rootDeviceInstanceId = 'test-only'
function Get-PnpDevice {
    param($InstanceId, $ErrorAction)
    if ($InstanceId -ne 'test-only') { throw 'Unexpected real device access' }
    return [pscustomobject]@{ InstanceId = $InstanceId; Status = 'OK' }
}
function Enable-PnpDevice {
    param($InstanceId, $Confirm, $ErrorAction)
    if ($InstanceId -ne 'test-only' -or $Confirm) { throw 'Unsafe enable arguments' }
    $script:toggles += 'enable'
}
function Disable-PnpDevice {
    param($InstanceId, $Confirm, $ErrorAction)
    if ($InstanceId -ne 'test-only' -or $Confirm) { throw 'Unsafe disable arguments' }
    $script:toggles += 'disable'
}
function Invoke-PnpTool { throw 'Unexpected PnPUtil fallback with function-based PnpDevice available' }
Invoke-Expression (Import-TestFunction 'lifecycle-validation.ps1' 'Get-QpwgraphRootDevice')
Invoke-Expression (Import-TestFunction 'lifecycle-validation.ps1' 'Set-QpwgraphRootDeviceEnabled')
if ((Get-QpwgraphRootDevice).InstanceId -ne 'test-only') { throw 'Function-based lookup failed' }
$script:toggles = @()
Set-QpwgraphRootDeviceEnabled $false
Set-QpwgraphRootDeviceEnabled $true
if (($script:toggles -join ',') -ne 'disable,enable') { throw 'Function-based PnP commands not invoked' }
Invoke-Expression (Import-TestFunction 'test-validation.ps1' 'Show-QpwgraphDeviceStatus')
if ((Show-QpwgraphDeviceStatus).InstanceId -ne 'test-only') { throw 'Status ignored function-based PnpDevice' }

Invoke-Expression (Import-TestFunction 'lifecycle-validation.ps1' 'Invoke-DisableEnable')
function Wait-Smoke([string[]] $Arguments, [string] $Description) {
    $script:checks += $Description
    if ($Description -eq $script:failCheck) { throw 'injected lifecycle failure' }
}
foreach ($failCheck in @('', 'Disabled endpoint absence verification', 'Post-enable cable verification')) {
    $script:failCheck = $failCheck
    $script:toggles = @()
    $script:checks = @()
    $failure = $null
    try { Invoke-DisableEnable } catch { $failure = $_ }
    if (($script:toggles -join ',') -ne 'disable,enable') {
        throw 'Lifecycle did not leave the exact test device enabled'
    }
    if ([bool]$failure -ne (-not [string]::IsNullOrEmpty($failCheck))) {
        throw 'Lifecycle failure was suppressed or success failed'
    }
    if (-not $failCheck -and $script:checks.Count -ne 5) { throw 'Lifecycle skipped a verification' }
}
Invoke-Expression (Import-TestFunction 'run-client-crash.ps1' 'Test-ActiveHandshake')
foreach ($case in @(
    @{ Line = 'QPWGRAPH_ROUND_TRIP_ACTIVE pid=42 frames=480'; Pass = $true },
    @{ Line = 'QPWGRAPH_ROUND_TRIP_ACTIVE pid=43 frames=480'; Pass = $false },
    @{ Line = 'QPWGRAPH_ROUND_TRIP_ACTIVE pid=42 frames=0'; Pass = $false },
    @{ Line = 'opening render endpoint'; Pass = $false },
    @{ Line = 'QPWGRAPH_ROUND_TRIP_ACTIVE pid=42 frames=480 extra'; Pass = $false }
)) {
    if ((Test-ActiveHandshake $case.Line 42) -ne $case.Pass) { throw 'Crash handshake accepted the wrong process or missing PCM' }
}
Write-Output 'Validation workflow regressions passed without opening audio clients or changing devices.'
