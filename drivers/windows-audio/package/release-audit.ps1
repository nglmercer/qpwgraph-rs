#requires -Version 5.1

[CmdletBinding()]
param(
    [Parameter(Mandatory = $false)]
    [string] $PackageRoot,
    [Parameter(Mandatory = $false)]
    [switch] $Json,
    [Parameter(Mandatory = $false)]
    [switch] $Strict
)

$ErrorActionPreference = 'Stop'

# This audit never changes boot, device, signing, or installation state.

function Resolve-AuditPackageRoot([string] $RequestedRoot) {
    if (-not [string]::IsNullOrWhiteSpace($RequestedRoot)) {
        return (Resolve-Path -LiteralPath $RequestedRoot -ErrorAction Stop).Path
    }

    $staged = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\qpwgraph-audio-package'
    if (Test-Path -LiteralPath $staged -PathType Container) {
        return (Resolve-Path -LiteralPath $staged -ErrorAction Stop).Path
    }
    return (Resolve-Path -LiteralPath $PSScriptRoot -ErrorAction Stop).Path
}

$packageRootPath = Resolve-AuditPackageRoot $PackageRoot
$checks = New-Object 'System.Collections.Generic.List[object]'

function Add-Check {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Gate,
        [Parameter(Mandatory = $true)]
        [ValidateSet('pass', 'blocked', 'unknown')]
        [string] $Status,
        [Parameter(Mandatory = $true)]
        [string] $Evidence
    )
    $checks.Add([pscustomobject]@{
            Gate     = $Gate
            Status   = $Status
            Evidence = $Evidence
        })
}

function Find-Executable([string] $Name) {
    $command = Get-Command -Name $Name -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -ne $command) {
        return $command.Source
    }
    return $null
}

function Find-FirstFile([string[]] $Candidates) {
    foreach ($candidate in $Candidates) {
        if (-not [string]::IsNullOrWhiteSpace($candidate) -and
            (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            return (Resolve-Path -LiteralPath $candidate -ErrorAction Stop).Path
        }
    }
    return $null
}

function Find-Client {
    param(
        [Parameter(Mandatory = $true)]
        [string[]] $ProcessNames,
        [Parameter(Mandatory = $true)]
        [string[]] $DisplayPatterns,
        [Parameter(Mandatory = $true)]
        [string[]] $Paths
    )

    foreach ($processName in $ProcessNames) {
        $process = Get-Process -Name $processName -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($null -ne $process) {
            return "running process $($process.ProcessName)"
        }
    }

    $path = Find-FirstFile $Paths
    if ($null -ne $path) {
        return $path
    }

    $uninstallRoots = @(
        'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall',
        'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall',
        'HKLM:\Software\Wow6432Node\Microsoft\Windows\CurrentVersion\Uninstall'
    )
    foreach ($root in $uninstallRoots) {
        if (-not (Test-Path -LiteralPath $root -PathType Container)) {
            continue
        }
        foreach ($entry in (Get-ChildItem -LiteralPath $root -ErrorAction SilentlyContinue)) {
            $properties = Get-ItemProperty -LiteralPath $entry.PSPath -ErrorAction SilentlyContinue
            if ($null -eq $properties -or [string]::IsNullOrWhiteSpace($properties.DisplayName)) {
                continue
            }
            foreach ($pattern in $DisplayPatterns) {
                if ($properties.DisplayName -match $pattern) {
                    return "uninstall entry '$($properties.DisplayName)'"
                }
            }
        }
    }
    return $null
}

function Add-ToolCheck([string] $Gate, [string] $Executable, [string] $Hint) {
    $path = Find-Executable $Executable
    if ($null -ne $path) {
        Add-Check $Gate 'pass' $path
    } else {
        Add-Check $Gate 'blocked' "$Executable was not found on PATH; $Hint"
    }
    return $path
}

function Add-SignatureCheck([string] $Gate, [string] $Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        Add-Check $Gate 'blocked' "missing $Path"
        return
    }
    try {
        $signature = Get-AuthenticodeSignature -LiteralPath $Path
    } catch {
        Add-Check $Gate 'unknown' "could not inspect ${Path}: $($_.Exception.Message)"
        return
    }
    $signer = if ($null -ne $signature.SignerCertificate) {
        $signature.SignerCertificate.Subject
    } else {
        'no signer certificate'
    }
    if ($signature.Status -eq 'Valid') {
        Add-Check $Gate 'pass' "$($signature.Status); $signer"
    } else {
        Add-Check $Gate 'blocked' "$($signature.Status); $signer"
    }
}

function Add-ManualGate([string] $Gate, [string] $Evidence) {
    Add-Check $Gate 'unknown' $Evidence
}

# Package shape and local signing state.
$requiredPackageFiles = @(
    'manifest.json',
    'qpwgraph_audio.sys',
    'qpwgraph-audio.inf',
    'qpwgraph-audio.cat',
    'install.ps1',
    'uninstall.ps1',
    'sign-test.ps1',
    'lifecycle-validation.ps1'
)
$missingFiles = @(
    $requiredPackageFiles |
        Where-Object { -not (Test-Path -LiteralPath (Join-Path $packageRootPath $_) -PathType Leaf) }
)
if ($missingFiles.Count -eq 0) {
    Add-Check 'Package artifact set' 'pass' $packageRootPath
} else {
    Add-Check 'Package artifact set' 'blocked' "missing: $($missingFiles -join ', ')"
}

$manifestPath = Join-Path $packageRootPath 'manifest.json'
if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
    Add-Check 'Ready package manifest' 'blocked' "missing $manifestPath"
} else {
    try {
        $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
        if ($manifest.implementation_status -eq 'ready') {
            Add-Check 'Ready package manifest' 'pass' "implementation_status=ready; driver_version=$($manifest.driver_version)"
        } else {
            Add-Check 'Ready package manifest' 'blocked' "implementation_status=$($manifest.implementation_status); run the WDK --build-package step"
        }
    } catch {
        Add-Check 'Ready package manifest' 'blocked' "invalid manifest: $($_.Exception.Message)"
    }
}

$infPath = Join-Path $packageRootPath 'qpwgraph-audio.inf'
if (-not (Test-Path -LiteralPath $infPath -PathType Leaf)) {
    Add-Check 'INF/KMDF metadata' 'blocked' "missing $infPath"
} else {
    $infText = Get-Content -LiteralPath $infPath -Raw
    if ($infText -match '(?im)^\s*KmdfLibraryVersion\s*=\s*1\.31\s*$') {
        Add-Check 'INF/KMDF metadata' 'pass' 'KmdfLibraryVersion=1.31'
    } else {
        Add-Check 'INF/KMDF metadata' 'blocked' 'KmdfLibraryVersion=1.31 was not found'
    }
}

Add-SignatureCheck 'Driver Authenticode signature' (Join-Path $packageRootPath 'qpwgraph_audio.sys')
Add-SignatureCheck 'Catalog Authenticode signature' (Join-Path $packageRootPath 'qpwgraph-audio.cat')

# Build prerequisites. These checks are intentionally read-only and overlap the
# xtask audit so the same report can be collected from a staged package.
$wdkRootText = [Environment]::GetEnvironmentVariable('WDKContentRoot', 'Process')
$wdkRoot = $null
if ([string]::IsNullOrWhiteSpace($wdkRootText)) {
    Add-Check 'WDKContentRoot' 'blocked' 'WDKContentRoot is not set; use a WDK/eWDK developer prompt'
} elseif (-not (Test-Path -LiteralPath $wdkRootText -PathType Container)) {
    Add-Check 'WDKContentRoot' 'blocked' "$wdkRootText is not a directory"
} else {
    $wdkRoot = (Resolve-Path -LiteralPath $wdkRootText -ErrorAction Stop).Path
    Add-Check 'WDKContentRoot' 'pass' $wdkRoot
}

if ($null -eq $wdkRoot) {
    Add-Check 'WDK KM CRT headers' 'blocked' 'not checked because WDKContentRoot is unavailable'
    Add-Check 'ACX header' 'blocked' 'not checked because WDKContentRoot is unavailable'
    Add-Check 'ACX stub library' 'blocked' 'not checked because WDKContentRoot is unavailable'
} else {
    $includeRoot = Join-Path $wdkRoot 'Include'
    $crt = $null
    if (Test-Path -LiteralPath $includeRoot -PathType Container) {
        foreach ($versionDirectory in (Get-ChildItem -LiteralPath $includeRoot -Directory -ErrorAction SilentlyContinue)) {
            $candidate = Join-Path $versionDirectory.FullName 'km\crt'
            if (Test-Path -LiteralPath $candidate -PathType Container) {
                $crt = $candidate
                break
            }
        }
    }
    if ($null -ne $crt) {
        Add-Check 'WDK KM CRT headers' 'pass' $crt
    } else {
        Add-Check 'WDK KM CRT headers' 'blocked' "no Include\\<version>\\km\\crt under $includeRoot"
    }

    $acxHeader = Get-ChildItem -LiteralPath $includeRoot -Filter 'acx.h' -File -Recurse -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -ne $acxHeader) {
        Add-Check 'ACX header' 'pass' $acxHeader.FullName
    } else {
        Add-Check 'ACX header' 'blocked' "acx.h was not found below $includeRoot"
    }

    $libRoot = Join-Path $wdkRoot 'Lib'
    $acxStub = Get-ChildItem -LiteralPath $libRoot -Filter 'acxstub.lib' -File -Recurse -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -ne $acxStub) {
        Add-Check 'ACX stub library' 'pass' $acxStub.FullName
    } else {
        Add-Check 'ACX stub library' 'blocked' "acxstub.lib was not found below $libRoot"
    }
}

$null = Add-ToolCheck 'MSVC compiler' 'cl.exe' 'use the matching VS/eWDK developer prompt'
$null = Add-ToolCheck 'MSBuild' 'msbuild.exe' 'use the matching VS/eWDK developer prompt'
$hlkStudioPath = Find-Executable 'hlkstudio.exe'
if ($null -eq $hlkStudioPath) {
    $hlkCandidates = @()
    foreach ($programRoot in @(${env:ProgramFiles(x86)}, $env:ProgramFiles)) {
        if (-not [string]::IsNullOrWhiteSpace($programRoot)) {
            $hlkCandidates += Join-Path $programRoot 'Windows Kits\10\Hardware Lab Kit\Studio\hlkstudio.exe'
        }
    }
    $hlkStudioPath = Find-FirstFile $hlkCandidates
}
if ($null -ne $hlkStudioPath) {
    Add-Check 'HLK Studio' 'pass' $hlkStudioPath
} else {
    Add-Check 'HLK Studio' 'blocked' 'hlkstudio.exe was not found on PATH or in the standard Windows Kits path; install the Windows Hardware Lab Kit'
}

$clangPath = Find-Executable 'clang.exe'
if ($null -eq $clangPath) {
    Add-Check 'LLVM/clang toolchain' 'blocked' 'clang.exe was not found on PATH; use released LLVM 17-21'
} else {
    $versionOutput = @(& $clangPath '--version' 2>&1) | ForEach-Object { $_.ToString() }
    $versionText = $versionOutput -join ' '
    if ($versionText -match '(?i)(?:LLVM|clang) version\s+(\d+)(?:\.\d+)?') {
        $major = [int] $Matches[1]
        if ($major -ge 17 -and $major -le 21) {
            Add-Check 'LLVM/clang toolchain' 'pass' "$clangPath ($($Matches[0]))"
        } else {
            Add-Check 'LLVM/clang toolchain' 'blocked' "$clangPath reports $($Matches[0]); use released LLVM 17-21"
        }
    } else {
        Add-Check 'LLVM/clang toolchain' 'blocked' "$clangPath did not report a parseable LLVM version"
    }
}

# Read-only machine state. A configured verifier with no rules is deliberately
# reported as unknown: this audit cannot prove a clean run, and it never changes
# verifier settings or the boot configuration.
$verifierPath = Find-Executable 'verifier.exe'
if ($null -eq $verifierPath) {
    Add-Check 'Driver Verifier clean' 'unknown' 'verifier.exe was not found; run the release verifier matrix on a test machine'
} else {
    $verifierOutput = @(& $verifierPath '/querysettings' 2>&1) | ForEach-Object { $_.ToString() }
    $verifierText = $verifierOutput -join [Environment]::NewLine
    if ($verifierText -match '(?im)Verifier Flags:\s*0x0+\b') {
        Add-Check 'Driver Verifier clean' 'unknown' 'no verifier rules are configured; a clean exercised run is still required'
    } elseif ($verifierText -match '(?im)Verifier Flags:\s*0x[0-9a-f]+') {
        Add-Check 'Driver Verifier clean' 'blocked' 'verifier rules are configured; run and record the required clean matrix'
    } else {
        Add-Check 'Driver Verifier clean' 'unknown' 'verifier /querysettings did not expose a parseable flag state'
    }
}

$secureBoot = $null
try {
    $secureBoot = Confirm-SecureBootUEFI -ErrorAction Stop
} catch {
    $secureBootState = Get-ItemProperty -LiteralPath 'HKLM:\SYSTEM\CurrentControlSet\Control\SecureBoot\State' -Name 'UEFISecureBootEnabled' -ErrorAction SilentlyContinue
    if ($null -ne $secureBootState) {
        $secureBoot = ([int] $secureBootState.UEFISecureBootEnabled) -eq 1
    }
}
if ($secureBoot -eq $true) {
    Add-Check 'Secure Boot enabled' 'pass' 'UEFI Secure Boot is enabled'
} elseif ($secureBoot -eq $false) {
    Add-Check 'Secure Boot enabled' 'blocked' 'UEFI Secure Boot is disabled'
} else {
    Add-Check 'Secure Boot enabled' 'unknown' 'Secure Boot state could not be read from this session'
}

$bcdeditPath = Find-Executable 'bcdedit.exe'
if ($null -eq $bcdeditPath) {
    Add-Check 'Windows test-signing disabled' 'unknown' 'bcdedit.exe was not found'
} else {
    $bootOutput = @(& $bcdeditPath '/enum' 2>&1) | ForEach-Object { $_.ToString() }
    $bootText = $bootOutput -join [Environment]::NewLine
    if ($bootText -match '(?im)^\s*testsigning\s+Yes\s*$') {
        Add-Check 'Windows test-signing disabled' 'blocked' 'test-signing is enabled; public release validation requires it to be off'
    } elseif ($bootText -match '(?im)^\s*testsigning\s+No\s*$') {
        Add-Check 'Windows test-signing disabled' 'pass' 'test-signing is disabled'
    } else {
        Add-Check 'Windows test-signing disabled' 'unknown' 'bcdedit did not expose a testsigning value'
    }
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Add-Check 'Current shell elevation' 'pass' 'running as Administrator'
} else {
    Add-Check 'Current shell elevation' 'unknown' 'not elevated; lifecycle and install gates require an Administrator session'
}

# Installed provider evidence is useful context but is not required to run the
# package audit on a build machine. Do not install, disable, or restart devices.
$pnpCommand = Get-Command -Name 'Get-PnpDevice' -CommandType Cmdlet -ErrorAction SilentlyContinue
if ($null -eq $pnpCommand) {
    Add-Check 'QPWGraph provider device state' 'unknown' 'Get-PnpDevice is unavailable in this PowerShell session'
} else {
    $qpwDevices = @()
    foreach ($deviceClass in @('Media', 'AudioEndpoint')) {
        $qpwDevices += @(Get-PnpDevice -Class $deviceClass -PresentOnly -ErrorAction SilentlyContinue |
            Where-Object {
                $_.FriendlyName -match '(?i)QPWGraph|QPW' -or
                $_.InstanceId -match '(?i)QPWGraph|QPW'
            })
    }
    $qpwDevices = @($qpwDevices | Sort-Object InstanceId -Unique)
    if ($qpwDevices.Count -gt 0) {
        $deviceSummary = ($qpwDevices | ForEach-Object { "$($_.Status): $($_.FriendlyName)" }) -join '; '
        Add-Check 'QPWGraph provider device state' 'pass' $deviceSummary
    } else {
        Add-Check 'QPWGraph provider device state' 'unknown' 'no present QPWGraph media/audio-endpoint device was found; this audit is read-only'
    }
}

# Availability checks make the remaining ordinary-client matrix explicit. They
# do not claim that an application accepted relay audio; that remains a manual
# acceptance gate below.
$clientRoots = @($env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:LOCALAPPDATA) |
    Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
$clientDefinitions = @(
    [pscustomobject]@{
        Name = 'Firefox'
        Processes = @('firefox')
        Patterns = @('Mozilla Firefox')
        Paths = @($clientRoots | ForEach-Object { Join-Path $_ 'Mozilla Firefox\firefox.exe' })
    },
    [pscustomobject]@{
        Name = 'Chrome'
        Processes = @('chrome')
        Patterns = @('Google Chrome')
        Paths = @($clientRoots | ForEach-Object { Join-Path $_ 'Google\Chrome\Application\chrome.exe' })
    },
    [pscustomobject]@{
        Name = 'VLC'
        Processes = @('vlc')
        Patterns = @('VLC media player', 'VideoLAN VLC')
        Paths = @($clientRoots | ForEach-Object { Join-Path $_ 'VideoLAN\VLC\vlc.exe' })
    },
    [pscustomobject]@{
        Name = 'Discord'
        Processes = @('Discord')
        Patterns = @('Discord')
        Paths = @(
            $clientRoots | ForEach-Object { Join-Path $_ 'Discord\Discord.exe' }
            if (-not [string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
                Join-Path $env:LOCALAPPDATA 'Discord\Update.exe'
            }
        )
    },
    [pscustomobject]@{
        Name = 'OBS Studio'
        Processes = @('obs64', 'obs32')
        Patterns = @('OBS Studio')
        Paths = @($clientRoots | ForEach-Object { Join-Path $_ 'obs-studio\bin\64bit\obs64.exe' })
    }
)
foreach ($client in $clientDefinitions) {
    $clientEvidence = Find-Client $client.Processes $client.Patterns $client.Paths
    if ($null -ne $clientEvidence) {
        Add-Check "Client available: $($client.Name)" 'pass' $clientEvidence
    } else {
        Add-Check "Client available: $($client.Name)" 'blocked' 'application was not found; install it only on the disposable acceptance machine'
    }
}

# These rows intentionally remain unknown until a human runs the corresponding
# acceptance procedure. Keeping them in the same report prevents a green local
# build from being mistaken for complete Windows feature parity.
Add-ManualGate 'HLK audio tests complete' 'run the relevant HLK audio tests and attach the result'
Add-ManualGate 'Microsoft signing pipeline established' 'obtain and record the Microsoft-signed release package'
Add-ManualGate 'Secure Boot installation verified' 'install the Microsoft-signed package with Secure Boot enabled and record the result'
Add-ManualGate 'Chrome/VLC ordinary relay acceptance' 'run the normal-speaker relay matrix with Chrome and VLC'
Add-ManualGate 'Discord Relay Microphone acceptance' 'run peer-to-Discord capture acceptance on the disposable test machine'
Add-ManualGate 'Sleep/resume lifecycle' 'exercise suspend/resume with active and idle streams and record endpoint/cable results'
Add-ManualGate 'Disable/enable lifecycle' 'exercise device disable/enable and record endpoint/cable results'
Add-ManualGate 'Destination disappearance and return' 'remove and restore the physical destination during an isolated effect route'
Add-ManualGate 'Physical endpoint churn selector stability' 'change the physical endpoint and verify stable selectors recover the route'

$passCount = @($checks | Where-Object { $_.Status -eq 'pass' }).Count
$blockedCount = @($checks | Where-Object { $_.Status -eq 'blocked' }).Count
$unknownCount = @($checks | Where-Object { $_.Status -eq 'unknown' }).Count
$summary = [pscustomobject]@{
    PackageRoot = $packageRootPath
    Pass        = $passCount
    Blocked     = $blockedCount
    Unknown     = $unknownCount
    Strict      = [bool] $Strict
}
$checkArray = @($checks | ForEach-Object { $_ })

if ($Json) {
    [pscustomobject]@{
        summary = $summary
        checks  = $checkArray
    } | ConvertTo-Json -Depth 6
} else {
    Write-Output "QPWGraph Windows audio release-gate audit: $packageRootPath"
    foreach ($check in $checks) {
        Write-Output ('[{0,-7}] {1}: {2}' -f $check.Status.ToUpperInvariant(), $check.Gate, $check.Evidence)
    }
    Write-Output "Summary: pass=$passCount blocked=$blockedCount unknown=$unknownCount"
    if ($Strict -and ($blockedCount -gt 0 -or $unknownCount -gt 0)) {
        Write-Output 'Strict mode: release gates are not complete.'
    }
}

if ($Strict -and ($blockedCount -gt 0 -or $unknownCount -gt 0)) {
    exit 1
}
exit 0
