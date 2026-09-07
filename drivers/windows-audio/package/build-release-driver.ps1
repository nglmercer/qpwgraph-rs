#requires -Version 5.1

[CmdletBinding()]
param(
    [Parameter(Mandatory = $false)]
    [string] $RepositoryRoot,
    [Parameter(Mandatory = $false)]
    [string] $OutputPath,
    [Parameter(Mandatory = $false)]
    [switch] $AllowDirty
)

$ErrorActionPreference = 'Stop'

Write-Output 'QPWGraph production driver candidate builder'
Write-Output '  READ-ONLY: source/toolchain metadata collection.'
Write-Output '  STATE-MUTATING: invokes the locked eWDK build and writes a release manifest.'
Write-Output '  REBOOT-REQUIRING: none.'

function Find-RepositoryRoot([string] $RequestedRoot) {
    if (-not [string]::IsNullOrWhiteSpace($RequestedRoot)) {
        return (Resolve-Path -LiteralPath $RequestedRoot -ErrorAction Stop).Path
    }
    $current = (Get-Item -LiteralPath $PSScriptRoot).Parent.Parent.Parent
    while ($null -ne $current) {
        if ((Test-Path -LiteralPath (Join-Path $current.FullName 'Cargo.toml') -PathType Leaf) -and
            (Test-Path -LiteralPath (Join-Path $current.FullName 'drivers\windows-audio\Cargo.toml') -PathType Leaf)) {
            return $current.FullName
        }
        $current = $current.Parent
    }
    throw 'Could not locate the repository root.'
}

function Invoke-Captured([string] $FileName, [string[]] $Arguments, [string] $WorkingDirectory) {
    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    Push-Location -LiteralPath $WorkingDirectory
    try {
        $output = @(& $FileName @Arguments 2>&1) | ForEach-Object { $_.ToString() }
        $exitCode = $LASTEXITCODE
    } finally {
        Pop-Location
        $ErrorActionPreference = $previousErrorActionPreference
    }
    if ($exitCode -ne 0) {
        throw "$FileName $($Arguments -join ' ') failed with exit code $exitCode. $($output -join [Environment]::NewLine)"
    }
    return @($output)
}

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

$root = Find-RepositoryRoot $RepositoryRoot
$driverSourceRoot = Join-Path $root 'drivers\windows-audio\driver\src'
if (-not (Test-Path -LiteralPath $driverSourceRoot -PathType Container)) {
    throw "Production driver source root was not found: $driverSourceRoot"
}
$runtimeSources = @(
    Get-ChildItem -LiteralPath $driverSourceRoot -Recurse -File -ErrorAction SilentlyContinue |
        Where-Object { $_.Extension -in @('.c', '.cc', '.cpp') }
)
if ($runtimeSources.Count -gt 0) {
    $paths = ($runtimeSources | ForEach-Object { $_.FullName }) -join '; '
    throw "Production driver candidates require a Rust-only runtime; project-authored C/C++ remains: $paths"
}

$git = Get-Command -Name 'git.exe' -CommandType Application -ErrorAction SilentlyContinue
if ($null -eq $git) { throw 'git.exe was not found on PATH.' }
$status = @(& $git.Source '-C' $root 'status' '--porcelain')
if (-not $AllowDirty -and $status.Count -gt 0) {
    throw 'The source tree is dirty. Commit the candidate or pass -AllowDirty for a non-release development package.'
}

$commit = ((& $git.Source '-C' $root 'rev-parse' 'HEAD') | Out-String).Trim()
$workspace = Join-Path $root 'drivers\windows-audio'
$xtaskArgs = @('run', '-p', 'qpwgraph-audio-xtask', '--locked', '--', '--build-package')
Invoke-Captured 'cargo.exe' $xtaskArgs $workspace | Out-Null

$packageRoot = Join-Path $workspace 'target\qpwgraph-audio-package'
if (-not (Test-Path -LiteralPath $packageRoot -PathType Container)) {
    throw "The build did not produce $packageRoot."
}

$manifestPath = Join-Path $packageRoot 'manifest.json'
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if ([string]$manifest.implementation_status -ne 'ready') {
    throw "The staged package is not marked ready: $($manifest.implementation_status)"
}
$runtimeProperty = $manifest.PSObject.Properties['driver_runtime']
if ($null -ne $runtimeProperty -and [string]$runtimeProperty.Value -ne 'rust-only') {
    throw "The staged package has an unexpected driver_runtime marker: $($runtimeProperty.Value)"
}
if ($null -eq $runtimeProperty) {
    $manifest | Add-Member -NotePropertyName 'driver_runtime' -NotePropertyValue 'rust-only'
    Set-Content -LiteralPath $manifestPath -Value ($manifest | ConvertTo-Json -Depth 10) -Encoding UTF8
}

$hashes = [ordered]@{}
foreach ($name in @('qpwgraph_audio.sys', 'qpwgraph-audio.inf', 'qpwgraph-audio.cat', 'manifest.json')) {
    $path = Join-Path $packageRoot $name
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "The staged package is missing $name."
    }
    $hashes[$name] = [ordered]@{
        sha256 = Get-Sha256 $path
        length = (Get-Item -LiteralPath $path).Length
    }
}

$rust = ((& rustc.exe '--version') | Out-String).Trim()
$llvmOutput = @(& clang.exe '--version' 2>&1) | ForEach-Object { $_.ToString() }
$wdkRoot = [Environment]::GetEnvironmentVariable('WDKContentRoot')
$wdkVersion = if ([string]::IsNullOrWhiteSpace($wdkRoot)) { $null } else { Split-Path -Leaf $wdkRoot.TrimEnd('\') }
$releaseManifest = [ordered]@{
    schema = 1
    kind = 'qpwgraph-windows-driver-release-candidate'
    created_utc = [DateTime]::UtcNow.ToString('o')
    git_commit = $commit
    source_dirty = ($status.Count -gt 0)
    driver_version = [string]$manifest.driver_version
    implementation_status = [string]$manifest.implementation_status
    driver_runtime = 'rust-only'
    package_root = $packageRoot
    artifacts = $hashes
    sys_sha256 = $hashes['qpwgraph_audio.sys'].sha256
    inf_sha256 = $hashes['qpwgraph-audio.inf'].sha256
    cat_sha256 = $hashes['qpwgraph-audio.cat'].sha256
    wdk_version = $wdkVersion
    rust_version = $rust
    llvm_version = ($llvmOutput -join ' ')
    notes = @(
        'This is a pre-submission candidate. It is not Microsoft-signed evidence.'
        'The Hardware Dev Center submission and credentials stay outside the repository.'
    )
}

$json = $releaseManifest | ConvertTo-Json -Depth 10
$destination = if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    Join-Path $packageRoot 'release-manifest.json'
} else {
    $OutputPath
}
$parent = Split-Path -Parent $destination
if (-not [string]::IsNullOrWhiteSpace($parent)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
Set-Content -LiteralPath $destination -Value $json -Encoding UTF8
Write-Output "Release candidate manifest written to $((Resolve-Path -LiteralPath $destination).Path)."
Write-Output "Driver SYS SHA256: $($hashes['qpwgraph_audio.sys'].sha256)"
