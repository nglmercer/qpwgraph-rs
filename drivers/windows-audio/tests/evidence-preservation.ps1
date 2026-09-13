#requires -Version 5.1
$ErrorActionPreference = 'Stop'

# Run each public script with an occupied output path. It must refuse before
# querying the machine or trying to find/open the deliberately missing probe.
$fixture = Join-Path ([IO.Path]::GetTempPath()) ("qpwgraph-retained-failure-{0}.json" -f [guid]::NewGuid())
$original = '{"completed":false,"failure":"retained regression fixture"}'
[IO.File]::WriteAllText($fixture, $original)
try {
    $scripts = @(
        @{ Name = 'run-driver-stress.ps1'; Arguments = @('-EvidencePath', $fixture, '-Execute', '-SmokeProbe', 'missing-probe.exe') },
        @{ Name = 'collect-verifier-evidence.ps1'; Arguments = @('-OutputPath', $fixture) }
    )
    foreach ($case in $scripts) {
        $scriptPath = Join-Path $PSScriptRoot ("..\package\" + $case.Name)
        $arguments = $case.Arguments
        $previousPreference = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            $output = @(& powershell.exe -NoProfile -ExecutionPolicy Bypass -File $scriptPath @arguments 2>&1)
            $code = $LASTEXITCODE
        } finally { $ErrorActionPreference = $previousPreference }
        if ($code -eq 0 -or ($output -join "`n") -notmatch 'evidence already exists') {
            throw "$($case.Name) did not reject an occupied evidence path before execution: $output"
        }
        if ([IO.File]::ReadAllText($fixture) -cne $original) {
            throw "$($case.Name) changed retained failure evidence"
        }
        Write-Output "$($case.Name): retained failure preserved"
    }
} finally {
    Remove-Item -LiteralPath $fixture -Force
}
