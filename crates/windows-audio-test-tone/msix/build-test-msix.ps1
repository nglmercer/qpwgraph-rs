# Packages the deterministic WASAPI tone helper as a test-signed MSIX so the
# packaged-app auto-route row can run against a real packaged process.
#
# Install:   ./build-test-msix.ps1 [-Configuration Debug|Release]
# Uninstall: ./build-test-msix.ps1 -Uninstall [-RemoveMachineTrust]
#
# Requires Windows SDK makeappx/signtool. The signing certificate lives in
# CurrentUser\My and is reused while valid. AppX deployment only honors
# machine trust stores, so installing also trusts the test certificate in
# LocalMachine\TrustedPeople via one UAC prompt; -RemoveMachineTrust undoes
# exactly that after the run.
param(
    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Debug',
    [switch]$Uninstall,
    [switch]$RemoveMachineTrust,
    [switch]$TrustMachineCertOnly
)

$ErrorActionPreference = 'Stop'
$PackageName = 'QPWGraph.TestTone'
$Publisher = 'CN=QPWGraph Test'
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\\..\\..')).Path
$StageDir = Join-Path $RepoRoot 'target\\msix-tone'
$MsixPath = Join-Path $StageDir 'QPWGraph.TestTone.msix'

function Test-IsElevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Invoke-SelfElevated {
    param([string]$Switch)
    $script = Join-Path $PSScriptRoot 'build-test-msix.ps1'
    $elevated = Start-Process -FilePath 'powershell.exe' -Verb RunAs -Wait -PassThru `
        -ArgumentList @('-ExecutionPolicy', 'Bypass', '-File', $script, $Switch)
    if ($elevated.ExitCode -ne 0) { throw "elevated $Switch failed with exit $($elevated.ExitCode)." }
}

function Get-TestCertificate {
    # NOTE: EnhancedKeyUsage Oids surface FriendlyName ('Code Signing') with
    # an empty Value on this host, so match either form.
    return Get-ChildItem 'Cert:\\CurrentUser\\My' | Where-Object {
        $_.Subject -eq $Publisher -and $_.NotAfter -gt (Get-Date).AddDays(30) -and
        ($_.EnhancedKeyUsageList | Where-Object {
            $_.FriendlyName -eq 'Code Signing' -or
            $_.ObjectId.Value -eq '1.3.6.1.5.5.7.3.3'
        })
    } | Sort-Object NotAfter -Descending | Select-Object -First 1
}

if ($TrustMachineCertOnly) {
    if (-not (Test-IsElevated)) { Invoke-SelfElevated '-TrustMachineCertOnly'; return }
    $trustLog = Join-Path $StageDir 'trust-elevated.log'
    try {
        "elevated trust step started as $([Security.Principal.WindowsIdentity]::GetCurrent().Name)" |
            Out-File $trustLog -Encoding utf8
        $cert = Get-TestCertificate
        if ($null -eq $cert) { throw 'test-signing certificate not found in CurrentUser\My.' }
        "found cert $($cert.Thumbprint)" | Out-File $trustLog -Append -Encoding utf8
        $store = New-Object Security.Cryptography.X509Certificates.X509Store('TrustedPeople', 'LocalMachine')
        $store.Open('ReadWrite')
        try {
            $present = $store.Certificates | Where-Object { $_.Thumbprint -eq $cert.Thumbprint }
            if ($null -eq $present) {
                $store.Add($cert)
                "trusted $($cert.Thumbprint)" | Out-File $trustLog -Append -Encoding utf8
            } else {
                'already trusted' | Out-File $trustLog -Append -Encoding utf8
            }
        } finally {
            $store.Close()
        }
    } catch {
        "ELEVATED FAILURE: $($_.Exception.Message)" | Out-File $trustLog -Append -Encoding utf8
        throw
    }
    return
}

if ($RemoveMachineTrust) {
    if (-not (Test-IsElevated)) {
        Invoke-SelfElevated '-RemoveMachineTrust'
    } else {
        $store = New-Object Security.Cryptography.X509Certificates.X509Store('TrustedPeople', 'LocalMachine')
        $store.Open('ReadWrite')
        try {
            $ours = @($store.Certificates | Where-Object { $_.Subject -eq $Publisher })
            foreach ($stale in $ours) { $store.Remove($stale) }
            Write-Host "Removed $($ours.Count) QPWGraph test certificate(s) from LocalMachine\TrustedPeople."
        } finally {
            $store.Close()
        }
    }
    if (-not $Uninstall) { return }
}

if ($Uninstall) {
    $installed = Get-AppxPackage -Name $PackageName -ErrorAction SilentlyContinue
    if ($null -eq $installed) {
        Write-Host "MSIX $PackageName is not installed."
    } else {
        $installed | Remove-AppxPackage
        Write-Host "MSIX $PackageName uninstalled."
    }
    return
}

$sdkBin = Get-ChildItem 'C:\\Program Files (x86)\\Windows Kits\\10\\bin\\*\\x64\\makeappx.exe' -ErrorAction SilentlyContinue |
    Sort-Object FullName -Descending | Select-Object -First 1
if ($null -eq $sdkBin) { throw 'makeappx.exe not found under the Windows 10 SDK.' }
$makeappx = $sdkBin.FullName
$signtool = Join-Path (Split-Path $sdkBin.FullName -Parent) 'signtool.exe'
if (-not (Test-Path $signtool)) { throw "signtool.exe not found next to $makeappx." }

Write-Host "Building tone helper ($Configuration)..."
$cargoArgs = @('build', '-p', 'windows-audio-test-tone')
if ($Configuration -eq 'Release') { $cargoArgs += '--release' }
& cargo $cargoArgs
if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit $LASTEXITCODE." }
$profileDir = if ($Configuration -eq 'Release') { 'release' } else { 'debug' }
$helperExe = Join-Path $RepoRoot "target\\$profileDir\\windows-audio-test-tone.exe"
if (-not (Test-Path $helperExe)) { throw "helper executable missing: $helperExe" }

if (Test-Path $StageDir) { Remove-Item $StageDir -Recurse -Force }
$assetsDir = New-Item -ItemType Directory -Path (Join-Path $StageDir 'Assets') -Force
Copy-Item $helperExe (Join-Path $StageDir 'windows-audio-test-tone.exe')
Copy-Item (Join-Path $PSScriptRoot 'AppxManifest.xml') (Join-Path $StageDir 'AppxManifest.xml')
# 1x1 transparent PNG; the store schema requires the files, not artwork.
$pixel = [Convert]::FromBase64String('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==')
foreach ($asset in @('StoreLogo.png', 'Square150x150Logo.png', 'Square44x44Logo.png')) {
    [IO.File]::WriteAllBytes((Join-Path $assetsDir.FullName $asset), $pixel)
}

$cert = Get-TestCertificate
if ($null -eq $cert) {
    Write-Host 'Creating a fresh CurrentUser test-signing certificate...'
    $cert = New-SelfSignedCertificate -Type Custom -Subject $Publisher `
        -KeyUsage DigitalSignature -KeyAlgorithm RSA -KeyLength 2048 `
        -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3') `
        -CertStoreLocation 'Cert:\\CurrentUser\\My' -NotAfter (Get-Date).AddYears(2)
} else {
    Write-Host "Reusing test-signing certificate $($cert.Thumbprint)."
}

Write-Host 'Packing MSIX...'
& $makeappx pack /d $StageDir /p $MsixPath /o
if ($LASTEXITCODE -ne 0) { throw "makeappx pack failed with exit $LASTEXITCODE." }

Write-Host 'Signing MSIX...'
& $signtool sign /fd SHA256 /sha1 $cert.Thumbprint $MsixPath
if ($LASTEXITCODE -ne 0) { throw "signtool sign failed with exit $LASTEXITCODE." }

$machineTrusted = Get-ChildItem 'Cert:\\LocalMachine\\TrustedPeople' -ErrorAction SilentlyContinue |
    Where-Object { $_.Thumbprint -eq $cert.Thumbprint } | Select-Object -First 1
if ($null -eq $machineTrusted) {
    Write-Host 'AppX deployment needs machine-wide trust: requesting one UAC elevation...'
    Invoke-SelfElevated '-TrustMachineCertOnly'
}
$installed = Get-AppxPackage -Name $PackageName -ErrorAction SilentlyContinue
if ($null -ne $installed) {
    Write-Host 'Removing the previous install for a clean reinstall...'
    $installed | Remove-AppxPackage
}
Write-Host 'Installing MSIX...'
Add-AppxPackage -Path $MsixPath
$installed = Get-AppxPackage -Name $PackageName
Write-Host "Installed: $($installed.PackageFullName)"
Write-Host "Family:    $($installed.PackageFamilyName)"
$alias = Join-Path $env:LOCALAPPDATA 'Microsoft\\WindowsApps\\QPWGraphTestTone.exe'
if (Test-Path $alias) {
    Write-Host "Alias:     $alias"
} else {
    Write-Host 'WARNING: execution alias not found after install.'
}
