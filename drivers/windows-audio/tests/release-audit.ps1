#requires -Version 5.1

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$packageRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\package') -ErrorAction Stop).Path
$auditScript = Join-Path $packageRoot 'release-audit.ps1'

function Invoke-AuditJson([string[]] $Arguments) {
    $output = @(& powershell.exe -NoProfile -ExecutionPolicy Bypass -File $auditScript @Arguments 2>&1)
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        $details = ($output | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine
        throw "release-audit.ps1 failed with exit code ${exitCode}: $details"
    }
    $json = ($output | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine
    return $json | ConvertFrom-Json
}

$baseline = Invoke-AuditJson @('-PackageRoot', $packageRoot, '-Json')
if ($baseline.checks.Count -lt 1) {
    throw 'The baseline release audit did not return any checks.'
}

$tempPath = Join-Path ([IO.Path]::GetTempPath()) ("qpwgraph-release-audit-{0}.json" -f [Guid]::NewGuid())
$evidence = @'
{
  "schema": 1,
  "gates": {
    "HLK audio tests complete": {
      "status": "pass",
      "evidence": "test-hlk-record"
    },
    "Chrome/VLC ordinary relay acceptance": {
      "status": "pass",
      "evidence": "test-client-record"
    }
  }
}
'@
[IO.File]::WriteAllText($tempPath, $evidence, (New-Object System.Text.UTF8Encoding($false)))
try {
    $withEvidence = Invoke-AuditJson @(
        '-PackageRoot', $packageRoot,
        '-EvidencePath', $tempPath,
        '-Json'
    )
    $hlk = @($withEvidence.checks | Where-Object { $_.Gate -eq 'HLK audio tests complete' })
    $clients = @($withEvidence.checks | Where-Object { $_.Gate -eq 'Chrome/VLC ordinary relay acceptance' })
    if ($hlk.Count -ne 1 -or $hlk[0].Status -ne 'pass') {
        throw 'The evidence overlay did not mark the HLK gate as pass.'
    }
    if ($clients.Count -ne 1 -or $clients[0].Status -ne 'pass') {
        throw 'The evidence overlay did not mark the client gate as pass.'
    }
    if ($withEvidence.summary.EvidencePath -ne (Resolve-Path -LiteralPath $tempPath).Path) {
        throw 'The audit summary did not retain the evidence path.'
    }
} finally {
    Remove-Item -LiteralPath $tempPath -Force -ErrorAction SilentlyContinue
}

Write-Output 'release-audit evidence overlay passed'
