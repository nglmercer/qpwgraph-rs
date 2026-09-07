# Windows audio-driver HLK preparation

HLK execution is an external, machine-backed release gate. The repository can
prepare and verify the exact package, but it cannot replace an HLK Controller,
Studio, or a clean test client.

## Required lab

- A supported 64-bit Windows 10/11 test client with the current Windows build
  recorded in the run evidence.
- Windows Hardware Lab Kit Controller and Studio of the same supported release
  family as the client.
- An isolated network and disposable test image. Do not use a developer's
  daily audio machine as the HLK client.
- The exact ready driver package produced from the locked source commit.
- A test-signing, preproduction-signing, or Microsoft-signed package tier
  explicitly recorded in the manifest. Test signing is not production evidence.

Prepare a manifest before opening HLK Studio:

```powershell
.\drivers\windows-audio\package\prepare-hlk.ps1 `
  -PackageRoot .\drivers\windows-audio\target\qpwgraph-audio-package `
  -OutputPath .\artifacts\hlk-preparation.json `
  -Strict
```

The manifest records the package hashes, driver version, OS build, expected
device instance (`ROOT\DEVGEN\QPWGRAPH_AUDIO`), and the four provider roles:

| Flow | Role | Endpoint |
| --- | --- | --- |
| render | `app-render` | QPWGraph Virtual Output |
| capture | `app-monitor` | QPWGraph Virtual Monitor |
| render | `relay-render` | QPWGraph Relay Sink |
| capture | `relay-capture` | QPWGraph Relay Microphone |

## HLK run

1. Snapshot the clean client and record its Windows build, firmware/Secure Boot
   state, and package hashes.
2. Install the exact package with the repository installer. Do not select a
   Windows default playback or recording device automatically.
3. In HLK Studio, create a machine pool containing only the intended client.
4. Discover the QPWGraph audio device and select the relevant Audio and Device
   Fundamentals tests for the driver model and endpoint flows. Record the test
   selection; do not claim that this document defines the complete HLK list.
5. Run the selected tests with the four endpoints present. Save failures and
   errata decisions rather than suppressing them.
6. Export the HLK result package. Compute its SHA-256 and store it with the
   preparation manifest, package hashes, test-machine build, and signing tier.

The definition of done is an exported result package with all required tests
passing, no unresolved code-workaround errata, and a binary hash that matches
the release candidate or is explained by the Microsoft signing transformation.
`prepare-hlk.ps1` prepares evidence; it does not run HLK or turn an absent
result into a green gate.

## Evidence record

Keep the following together outside normal source changes when the run is
complete:

```text
hlk-preparation.json
hlk-result-package.zip
hlk-result-package.zip.sha256
driver-release-manifest.json
test-machine-build.txt
secure-boot-audit.json
verifier-evidence.json
driver-stress.json
acceptance-evidence.json
returned-driver-verification.json
```

Run `validate-release-evidence.ps1` against that bundle and the returned
Microsoft-signed package before building the full installer. The validator
checks the evidence schemas, package hashes, Secure Boot/test-signing claims,
Verifier collection, structured 100-cycle stress rows, and returned signatures;
it does not execute or replace any lab test.
