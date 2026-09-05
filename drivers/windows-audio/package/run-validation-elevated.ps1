[CmdletBinding()]
param(
    [Parameter(Mandatory = $false)]
    [ValidateSet('Status', 'Prepare', 'EnableTestMode', 'Install', 'Smoke', 'Uninstall', 'DisableTestMode')]
    [string] $Phase = 'Status',
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
$packageRoot = (Resolve-Path -LiteralPath $PSScriptRoot -ErrorAction Stop).Path
$validationScript = Join-Path $packageRoot 'test-validation.ps1'
$logPath = Join-Path $packageRoot 'validation-last.log'

function Test-IsAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

if (-not (Test-IsAdministrator)) {
    $arguments = @(
        '-NoProfile',
        '-ExecutionPolicy', 'Bypass',
        '-File', $PSCommandPath,
        '-Phase', $Phase,
        '-RoundTripDurationMs', $RoundTripDurationMs.ToString()
    )
    if (-not [string]::IsNullOrWhiteSpace($PublishedInf)) {
        $arguments += @('-PublishedInf', $PublishedInf)
    }
    if ($Reboot) {
        $arguments += '-Reboot'
    }
    $elevated = Start-Process -FilePath 'powershell.exe' -Verb RunAs -Wait -PassThru -ArgumentList $arguments
    exit $elevated.ExitCode
}

if (-not (Test-Path -LiteralPath $validationScript -PathType Leaf)) {
    throw "The validation runner was not found: $validationScript"
}

$validationParameters = @{
    Phase              = $Phase
    RoundTripDurationMs = $RoundTripDurationMs
}
if (-not [string]::IsNullOrWhiteSpace($PublishedInf)) {
    $validationParameters.PublishedInf = $PublishedInf
}
if ($Reboot) {
    $validationParameters.Reboot = $true
}

Remove-Item -LiteralPath $logPath -Force -ErrorAction SilentlyContinue
$utf8 = New-Object System.Text.UTF8Encoding($false)
[IO.File]::WriteAllText(
    $logPath,
    "launcher phase=$Phase publishedInf=$PublishedInf reboot=$Reboot$([Environment]::NewLine)",
    $utf8
)
$exitCode = 0
try {
    $validationOutput = @(& $validationScript @validationParameters 2>&1)
    $validationNativeExitCode = $LASTEXITCODE
    foreach ($item in $validationOutput) {
        $line = $item.ToString()
        [IO.File]::AppendAllText($logPath, $line + [Environment]::NewLine, $utf8)
        Write-Output $line
    }
    $statusLine = "validation_last_native_exit=$validationNativeExitCode"
    [IO.File]::AppendAllText($logPath, $statusLine + [Environment]::NewLine, $utf8)
    Write-Output $statusLine
} catch {
    $line = $_.ToString()
    [IO.File]::AppendAllText($logPath, $line + [Environment]::NewLine, $utf8)
    Write-Output $line
    $exitCode = 1
}

exit $exitCode
